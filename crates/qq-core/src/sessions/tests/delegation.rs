use super::*;

mod audits;
mod budgets;
mod depth;
mod lifecycle;
mod spawning;
mod write_children;

async fn await_frozen<T>(future: impl std::future::Future<Output = T>, timeout: Duration) -> T {
    let (finished, waiting) = std::sync::mpsc::sync_channel::<()>(1);
    let mut watchdog = tokio::task::spawn_blocking(move || waiting.recv_timeout(timeout));
    let result = tokio::select! {
        result = future => result,
        _ = &mut watchdog => panic!("frozen-clock operation did not complete within {timeout:?} of real time"),
    };
    drop(finished);
    assert!(matches!(
        watchdog.await.unwrap(),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    result
}

struct RefusalProvider;

impl Provider for RefusalProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::RefusalDelta {
                text: "cannot complete that task".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct ResolvingLoader {
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    worker: Option<ModelSelection>,
    resolutions: Arc<AtomicUsize>,
    loads: Arc<StdMutex<Vec<ModelSelection>>>,
}

impl RuntimeLoader for ResolvingLoader {
    fn resolve_worker_model(
        &self,
        _workspace: String,
        parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        self.resolutions.fetch_add(1, Ordering::AcqRel);
        let selection = self.worker.clone().unwrap_or(parent);
        Box::pin(std::future::ready(Ok(selection)))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.loads.lock().unwrap().push(request.model.clone());
        let mut spawn_model_routes = vec!["test/explicit".to_owned()];
        if let Some(worker) = self.worker.as_ref().and_then(|worker| worker.model.clone()) {
            spawn_model_routes.push(worker);
        }
        let provider = if request.model.model.as_deref() == Some("test/model") {
            Arc::clone(&self.parent)
        } else {
            Arc::clone(&self.child)
        };
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(spawn_model_routes),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(load_error)
        })
    }
}

struct RejectingWorkerLoader {
    parent: Arc<dyn Provider>,
}

impl RuntimeLoader for RejectingWorkerLoader {
    fn resolve_worker_model(
        &self,
        _workspace: String,
        _parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        Box::pin(std::future::ready(Err(RuntimeLoadError {
            kind: RunFailureKind::Policy,
            message: "configured worker route is denied".to_owned(),
        })))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = Arc::clone(&self.parent);
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(load_error)
        })
    }
}

struct ValidatingLoader {
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    worker: Option<ModelSelection>,
    rejection: Option<RuntimeLoadError>,
    validations: Arc<StdMutex<Vec<ModelSelection>>>,
    loads: Arc<StdMutex<Vec<ModelSelection>>>,
}

impl RuntimeLoader for ValidatingLoader {
    fn resolve_worker_model(
        &self,
        _workspace: String,
        parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        let selection = self.worker.clone().unwrap_or(parent);
        Box::pin(std::future::ready(Ok(selection)))
    }

    fn validate_spawn_model(
        &self,
        _workspace: String,
        selection: ModelSelection,
        advertised: Arc<[String]>,
    ) -> SpawnModelValidationFuture {
        assert_eq!(advertised.as_ref(), ["test/child", "test/model"]);
        self.validations.lock().unwrap().push(selection);
        let result = match &self.rejection {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        };
        Box::pin(std::future::ready(result))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.loads.lock().unwrap().push(request.model.clone());
        let provider = if request.model.model.as_deref() == Some("test/model") {
            Arc::clone(&self.parent)
        } else {
            Arc::clone(&self.child)
        };
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(vec![
                            "test/child".to_owned(),
                            "test/model".to_owned(),
                        ]),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(load_error)
        })
    }
}

struct AccountingTextProvider {
    usage: qq_provider::ProviderUsage,
}

impl Provider for AccountingTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        provider_script([text_delta("done"), completed(Some(self.usage))])
    }
}

struct AccountingSpawnProvider {
    turn: StdMutex<usize>,
}

impl Provider for AccountingSpawnProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "first".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "first".to_owned(),
                    json: r#"{"task":"first","model":"test/child-first"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "first".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "second".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "second".to_owned(),
                    json: r#"{"task":"second","model":"test/child-second"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "second".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(provider_usage(2, 3)),
                }),
            ]))
        } else {
            provider_script([text_delta("done"), completed(Some(provider_usage(5, 7)))])
        }
    }
}

struct AccountingSpawnLoader;

impl RuntimeLoader for AccountingSpawnLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider: Arc<dyn Provider> = match request.model.model.as_deref() {
            Some("test/model") => Arc::new(AccountingSpawnProvider {
                turn: StdMutex::new(0),
            }),
            Some("test/child-first") => Arc::new(AccountingTextProvider {
                usage: provider_usage(11, 13),
            }),
            Some("test/child-second") => Arc::new(AccountingTextProvider {
                usage: provider_usage(17, 19),
            }),
            other => panic!("unexpected accounting test model: {other:?}"),
        };
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(vec![
                            "test/child-first".to_owned(),
                            "test/child-second".to_owned(),
                        ]),
                        &request.workspace,
                        Some(ModelPricing {
                            input_usd_nanos_per_token: 1,
                            output_usd_nanos_per_token: 1,
                            cache_read_usd_nanos_per_token: Some(1),
                            cache_write_usd_nanos_per_token: Some(1),
                            context_tier: None,
                            provenance: "accounting-test".to_owned(),
                        }),
                    )
                })
                .map_err(load_error)
        })
    }
}

struct FailingProvider;

impl Provider for FailingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::once(async {
            Err(qq_provider::ProviderError::Transport("offline".to_owned()))
        }))
    }
}

struct SpawnThenHangProvider {
    turn: AtomicUsize,
    second_turn_started: Arc<tokio::sync::Notify>,
}

impl Provider for SpawnThenHangProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        if self.turn.fetch_add(1, Ordering::AcqRel) == 0 {
            provider_script(
                tool_call(
                    "spawn",
                    "spawn_agent",
                    r#"{"task":"finish first","model":"test/child"}"#,
                )
                .into_iter()
                .chain([completed(None)]),
            )
        } else {
            self.second_turn_started.notify_one();
            Box::pin(stream::pending())
        }
    }
}

struct GaugedTextProvider {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Provider for GaugedTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let active = Arc::clone(&self.active);
        let peak = Arc::clone(&self.peak);
        Box::pin(async_stream! {
            let current = active.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(current, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(50)).await;
            active.fetch_sub(1, Ordering::AcqRel);
            yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "child done".to_owned(),
            });
            yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
        })
    }
}

async fn depth_harness(
    routed: Vec<(&'static str, Arc<dyn Provider>)>,
    queue: Vec<Arc<dyn Provider>>,
    max_depth: u16,
    max_active_runs: usize,
) -> SpawnHarness {
    spawn_harness_with_loader(
        Arc::new(DepthLoader {
            inner: QueueLoader {
                routed,
                queue: StdMutex::new(queue),
            },
            max_depth,
            write_children: false,
            pricing: None,
        }),
        max_active_runs,
    )
    .await
}

fn delegating_child() -> Arc<dyn Provider> {
    Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"look deeper","model":"test/grandchild"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    })
}

fn metered_delegation(
    script: Vec<(&'static str, String)>,
    usage: Vec<Option<TokenUsage>>,
) -> Arc<dyn Provider> {
    Arc::new(MeteredDelegationProvider {
        inner: ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script,
            turn: StdMutex::new(0),
        },
        usage,
        turn: AtomicUsize::new(0),
    })
}

async fn nested_spend_harness(grandchild_usage: Option<TokenUsage>) -> SpawnHarness {
    let parent = metered_delegation(
        vec![
            (
                "spawn_agent",
                r#"{"task":"survey","model":"test/child"}"#.to_owned(),
            ),
            (
                "spawn_agent",
                r#"{"task":"follow up","model":"test/later-child"}"#.to_owned(),
            ),
        ],
        vec![Some(usage(1, 0))],
    );
    let child = metered_delegation(
        vec![(
            "spawn_agent",
            r#"{"task":"look deeper","model":"test/grandchild"}"#.to_owned(),
        )],
        vec![Some(usage(1, 0)), Some(usage(1, 0)), Some(usage(900, 0))],
    );
    spawn_harness_with_loader(
        Arc::new(DepthLoader {
            inner: QueueLoader {
                routed: vec![
                    ("test/child", child),
                    (
                        "test/grandchild",
                        metered_delegation(Vec::new(), vec![grandchild_usage]),
                    ),
                    (
                        "test/later-child",
                        metered_delegation(Vec::new(), vec![Some(usage(10, 0))]),
                    ),
                ],
                queue: StdMutex::new(vec![parent]),
            },
            max_depth: 2,
            write_children: false,
            pricing: Some(budget_pricing()),
        }),
        8,
    )
    .await
}

async fn submit_nested_budget(harness: &SpawnHarness, cost_bound: bool) -> RunId {
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("delegate within the budget")],
                limits: RunLimits {
                    max_total_tokens: Some(100),
                    max_cost_usd_nanos: cost_bound.then_some(100_000),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("expected queued root");
    };
    run_id
}

struct StatelessFanOut {
    spawns: usize,
    route: &'static str,
}

impl Provider for StatelessFanOut {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let has_results = request.messages().iter().any(|message| {
            message
                .content()
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        if has_results {
            return text_reply("done");
        }
        let route = self.route;
        let mut events = Vec::new();
        for index in 0..self.spawns {
            let id = format!("call_{index}");
            events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: "spawn_agent".to_owned(),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: format!(r#"{{"task":"leaf {index}","model":"{route}"}}"#),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

struct VerdictProvider {
    reply: &'static str,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for VerdictProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        provider_script([
            text_delta(self.reply.to_owned()),
            completed(Some(provider_usage(50, 5))),
        ])
    }
}

struct MutateThenScriptProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    answers: StdMutex<std::collections::VecDeque<&'static str>>,
    turn: AtomicUsize,
}

impl Provider for MutateThenScriptProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            return provider_script(
                tool_call(
                    "call_0",
                    "write_file",
                    r#"{"path":"out.txt","content":"hello"}"#,
                )
                .into_iter()
                .chain([completed(None)]),
            );
        }
        let text = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or("out of script");
        text_reply(text.to_owned())
    }
}

async fn child_budget_harness(
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    write_children: bool,
    audit: crate::runtime::AuditMode,
) -> SpawnHarness {
    child_budget_harness_with_window(parent, child, write_children, audit, None).await
}
