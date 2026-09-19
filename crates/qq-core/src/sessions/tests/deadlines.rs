use super::*;

struct DeadlineSession {
    _directory: TempDir,
    runtime: SessionRuntime,
    session_id: SessionId,
    events: SessionEventStream,
}

impl DeadlineSession {
    async fn open(loader: Arc<dyn RuntimeLoader>, mode: ApprovalMode) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            loader,
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session_with_mode(&runtime, workspace_id, None, mode).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        Self {
            _directory: directory,
            runtime,
            session_id,
            events,
        }
    }

    async fn submit(&self, text: &str, duration_ms: u64) -> RunId {
        let receipt = self
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: self.session_id,
                    input: vec![InputPart::text(text.to_owned())],
                    limits: RunLimits {
                        max_duration_ms: Some(duration_ms),
                        ..RunLimits::default()
                    },
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
            panic!("prompt")
        };
        run_id
    }
}

#[tokio::test]
async fn duration_after_durable_tool_result_records_unavailable_checkpoint_before_settlement() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut harness = DeadlineSession::open(
        Arc::new(BlockingCheckpointLoader { requests }),
        ApprovalMode::Auto,
    )
    .await;
    std::fs::write(harness._directory.path().join("note.txt"), "tool result\n").unwrap();
    let run_id = harness.submit("inspect the note", 500).await;

    let mut observed = collect_until(&mut harness.events, |event| {
        matches!(event, SessionEvent::ToolCallFinished { .. })
    })
    .await;
    observed.extend(collect_until(&mut harness.events, finished_for(run_id)).await);

    let tool_finished = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
        .expect("tool result is durable");
    let checkpoint = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::CheckpointReviewed {
                    outcome: qq_protocol::CheckpointOutcome::Unavailable,
                    feedback,
                    ..
                } if feedback.contains("verdict was durably recorded")
                    && feedback.contains("budget")
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "budgeted unreviewed result has durable checkpoint status; observed={observed:#?}"
            )
        });
    let terminal = observed
        .iter()
        .position(|event| {
            matches!(
                event.event,
                SessionEvent::RunFinished {
                    run_id: finished,
                    outcome: RunOutcome::BudgetExhausted { ref exhaustion },
                    ..
                } if finished == run_id && exhaustion.limit == BudgetLimitKind::Duration
            )
        })
        .expect("duration budget settles the run");
    assert!(tool_finished < checkpoint && checkpoint < terminal);
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn duration_withdraws_pending_approval_without_executing_the_tool() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut harness = DeadlineSession::open(
        Arc::new(ApprovalLoader {
            requests: Arc::clone(&requests),
            tool: "__test_mutate",
            arguments: "{}",
            tool_turns: 1,
        }),
        ApprovalMode::Ask,
    )
    .await;
    let run_id = harness.submit("wait for approval", 500).await;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::RunFinished { outcome: RunOutcome::BudgetExhausted { exhaustion }, .. }
            if exhaustion.limit == BudgetLimitKind::Duration
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
    );
    assert!(
        respond_approval(
            &harness.runtime,
            run_id,
            call.id,
            ApprovalDecision::ApproveOnce
        )
        .await
        .is_err()
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    harness.runtime.shutdown().await.unwrap();
}

struct HeldDeadlineLoader {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    requests: Arc<AtomicUsize>,
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn duration_limited_run_does_not_settle_when_process_cleanup_is_unconfirmed() {
    let mut harness = DeadlineSession::open(
        Arc::new(ApprovalLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            tool: "shell",
            arguments: crate::tools::PANIC_SHELL_ARGUMENTS,
            tool_turns: 1,
        }),
        ApprovalMode::Full,
    )
    .await;
    let workspace = std::fs::canonicalize(harness._directory.path()).unwrap();
    let spawned = crate::tools::observe_shell_spawn(&workspace, true);
    let run_id = harness.submit("run a command", 1_000).await;
    let pid = tokio::time::timeout(Duration::from_secs(2), spawned)
        .await
        .unwrap()
        .unwrap();
    let unavailable = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match harness.events.next().await.unwrap() {
                Err(SessionRuntimeError::Unavailable) => break true,
                Ok(event)
                    if event.run_id == Some(run_id)
                        && matches!(event.event, SessionEvent::RunFinished { .. }) =>
                {
                    break false;
                }
                Ok(_) => {}
                Err(error) => panic!("{error}"),
            }
        }
    })
    .await
    .unwrap();
    crate::tools::assert_panicked_process_exits(pid).await;
    assert!(
        unavailable,
        "cleanup uncertainty must not publish a duration or other terminal outcome"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn duration_stops_shell_while_output_persistence_is_blocked() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut harness = DeadlineSession::open(
        Arc::new(ApprovalLoader {
            requests,
            tool: "shell",
            arguments: r#"{"command":"printf ready; sleep 30"}"#,
            tool_turns: 1,
        }),
        ApprovalMode::Ask,
    )
    .await;
    let workspace = std::fs::canonicalize(harness._directory.path()).unwrap();
    let spawned = crate::tools::observe_shell_spawn(&workspace, false);
    let run_id = harness.submit("run a command", 1_000).await;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    let (buffered, release_output) = execution::hold_buffered_tool_output(call.id);
    respond_approval(
        &harness.runtime,
        run_id,
        call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    let pid = tokio::time::timeout(Duration::from_secs(2), spawned)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), buffered)
        .await
        .unwrap()
        .unwrap();
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let store = harness.runtime.inner.store.clone();
    let blocked = tokio::spawn(async move {
        store
            .call(Priority::Control, move |_| {
                let _ = entered.send(());
                release_rx
                    .recv()
                    .map_err(|_| SessionRuntimeError::Unavailable)
            })
            .await
    });
    entered_rx.await.unwrap();
    release_output.send(()).unwrap();
    // The output flush cannot finish. Cancellation must still reach the
    // process; observing /proc avoids stealing Tokio's child reap.
    let process = PathBuf::from(format!("/proc/{pid}"));
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        while process.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    if stopped.is_err() {
        harness.runtime.shutdown().await.unwrap();
    }
    assert!(
        stopped.is_ok(),
        "the shell outlived its deadline while persistence held the stream"
    );
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    assert!(matches!(finished_outcome(&observed, run_id),
        Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn duration_waits_for_blocking_attachment_guidance_and_skill_cleanup() {
    for kind in ["attachment", "guidance", "skill"] {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut harness = DeadlineSession::open(
            Arc::new(ApprovalLoader {
                requests: Arc::clone(&requests),
                tool: "load_skill",
                arguments: r#"{"name":"stable"}"#,
                tool_turns: usize::from(kind == "skill"),
            }),
            ApprovalMode::Full,
        )
        .await;
        let workspace = std::fs::canonicalize(harness._directory.path()).unwrap();
        let skill = workspace.join(".qq/skills/stable");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Follow the workspace policy.\n").unwrap();
        std::fs::write(workspace.join("note.txt"), "observation").unwrap();
        let (entered, release) = crate::workspace::hold_blocking_preparation(workspace);
        let input = match kind {
            "attachment" => vec![InputPart::WorkspaceFile {
                path: "note.txt".to_owned(),
                expected_hash: None,
                range: None,
            }],
            "guidance" => vec![InputPart::text("/stable finish")],
            "skill" => vec![InputPart::text("load the skill")],
            _ => unreachable!(),
        };
        let receipt = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    input,
                    limits: RunLimits {
                        max_duration_ms: Some(500),
                        ..RunLimits::default()
                    },
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
            panic!("prompt")
        };
        tokio::time::timeout(Duration::from_secs(2), entered)
            .await
            .unwrap()
            .unwrap();
        let premature = tokio::time::timeout(Duration::from_millis(750), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                if matches!(event.event, SessionEvent::RunFinished { .. }) {
                    break;
                }
            }
        })
        .await;
        release.send(()).unwrap();
        assert!(
            premature.is_err(),
            "{kind} released the session with blocking work still active"
        );
        let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
        assert!(
            matches!(finished_outcome(&observed, run_id),
                Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
            ),
            "{kind}: {observed:?}"
        );
        assert_eq!(requests.lock().unwrap().len(), usize::from(kind == "skill"));
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn duration_does_not_restart_for_output_repair() {
    let mut harness = output_contract_harness(&["not json", "<hang>"]).await;
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("report")],
                limits: RunLimits {
                    max_duration_ms: Some(500),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: Some(Box::new(report_contract(1))),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("prompt")
    };
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    assert!(matches!(finished_outcome(&observed, run_id),
        Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
    ));
    assert!(finished_final_output(&observed, run_id).is_none());
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        2,
        "the repair must actually begin"
    );
    harness.runtime.shutdown().await.unwrap();
}

struct PreparedDeadlineLoader(Runtime);

struct CompactionDeadlineProvider(AtomicUsize);

impl Provider for CompactionDeadlineProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: over_threshold_output(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
        }
        if request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"))
        {
            Box::pin(async_stream::stream! {
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta { text: valid_summary("retained work") });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        } else {
            Box::pin(stream::pending())
        }
    }
}

#[tokio::test]
async fn completed_compaction_does_not_restart_the_original_duration() {
    let runtime = Runtime::new(
        CompactionDeadlineProvider(AtomicUsize::new(0)),
        "test-model",
        256,
    )
    .unwrap();
    let mut harness = DeadlineSession::open(
        Arc::new(PreparedDeadlineLoader(runtime)),
        ApprovalMode::Full,
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let run_id = harness.submit(&"y".repeat(MAX_PROMPT_BYTES), 1_500).await;
    collect_until(&mut harness.events, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    })
    .await;
    // The summary used at least one second; a restarted 1.5 s clock cannot
    // expire within this interval. Keep the interval wider than the remainder.
    let result = tokio::time::timeout(
        Duration::from_millis(900),
        collect_until(&mut harness.events, finished_for(run_id)),
    )
    .await;
    harness.runtime.shutdown().await.unwrap();
    let observed = result.expect("compaction must not grant a fresh duration allowance");
    assert!(matches!(finished_outcome(&observed, run_id),
        Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
    ));
}

impl RuntimeLoader for PreparedDeadlineLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let runtime = self.0.clone();
        Box::pin(async move { Ok(loaded_runtime(runtime, &request.workspace, None)) })
    }
}

struct ActiveDeadlineCall(Arc<AtomicUsize>);

struct PendingDeadlineSource {
    cancellation: Arc<StdMutex<Option<RunCancellation>>>,
}

impl crate::ContextSource for PendingDeadlineSource {
    fn name(&self) -> &str {
        "deadline-context"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn cache_key(&self, _: &crate::ContextRequest) -> Option<[u8; 32]> {
        None
    }
    fn fetch(
        &self,
        _: crate::ContextRequest,
        cancelled: RunCancellation,
    ) -> crate::ContextFetchFuture {
        *self.cancellation.lock().unwrap() = Some(cancelled);
        Box::pin(std::future::pending())
    }
    fn fail_policy(&self) -> crate::FailPolicy {
        crate::FailPolicy::Closed
    }
}

#[tokio::test]
async fn duration_cancels_context_preparation_before_any_provider_request() {
    let cancellation = Arc::new(StdMutex::new(None));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = ApprovalProvider {
        requests: Arc::clone(&requests),
        turn: StdMutex::new(0),
        tool: "__test_mutate",
        arguments: "{}",
        tool_turns: 0,
        usage: None,
    };
    let runtime = Runtime::new(provider, "test-model", 256)
        .unwrap()
        .with_context_source(Arc::new(PendingDeadlineSource {
            cancellation: Arc::clone(&cancellation),
        }));
    let mut harness = DeadlineSession::open(
        Arc::new(PreparedDeadlineLoader(runtime)),
        ApprovalMode::Full,
    )
    .await;
    let run_id = harness.submit("retrieve context", 300).await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    assert!(matches!(finished_outcome(&observed, run_id),
        Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
    ));
    assert!(
        cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert!(requests.lock().unwrap().is_empty());
    harness.runtime.shutdown().await.unwrap();
}

impl Drop for ActiveDeadlineCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn duration_cancels_read_only_and_mutating_external_calls() {
    for read_only in [true, false] {
        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let handler_active = Arc::clone(&active);
        let handler_entered = Arc::clone(&entered);
        let host = crate::EmbeddedToolHost::builder("deadline")
            .tool(
                "wait",
                "wait indefinitely",
                serde_json::json!({"type":"object"}),
                crate::ToolHints {
                    read_only,
                    ..crate::ToolHints::default()
                },
                Arc::new(move |_| {
                    let active = Arc::clone(&handler_active);
                    let entered = Arc::clone(&handler_entered);
                    Box::pin(async move {
                        active.fetch_add(1, Ordering::SeqCst);
                        let _active = ActiveDeadlineCall(active);
                        entered.notify_one();
                        std::future::pending().await
                    })
                }),
            )
            .build()
            .unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let provider = ApprovalProvider {
            requests: Arc::clone(&requests),
            turn: StdMutex::new(0),
            tool: "ext__deadline__wait",
            arguments: "{}",
            tool_turns: 1,
            usage: None,
        };
        let runtime = Runtime::new(provider, "test-model", 256)
            .unwrap()
            .with_tool_host(host);
        let mut harness = DeadlineSession::open(
            Arc::new(PreparedDeadlineLoader(runtime)),
            ApprovalMode::Full,
        )
        .await;
        let run_id = harness.submit("call the host", 500).await;
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
        assert!(matches!(finished_outcome(&observed, run_id),
            Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
        ));
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "handler still active after terminal"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn duration_cancels_automatic_compaction_and_settles_the_original_prompt() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Stall,
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("y".repeat(MAX_PROMPT_BYTES))],
                limits: RunLimits {
                    max_duration_ms: Some(1_000),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("prompt")
    };
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    harness.runtime.shutdown().await.unwrap();
    let compaction = observed
        .iter()
        .find_map(|event| match event.event {
            SessionEvent::RunStarted {
                run_id: started, ..
            } if started != run_id => Some(started),
            _ => None,
        })
        .expect("the summary must actually start");
    for expected in [compaction, run_id] {
        assert!(
            matches!(finished_outcome(&observed, expected),
                Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
            ),
            "{observed:?}"
        );
    }
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        2,
        "original prompt must not run after expired summary"
    );
}

impl RuntimeLoader for HeldDeadlineLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.load_with_progress(request, RuntimeLoadProgress::default())
    }

    fn load_with_progress(
        &self,
        request: RuntimeLoadRequest,
        progress: RuntimeLoadProgress,
    ) -> RuntimeLoadFuture {
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            progress.set(RuntimeLoadStage::ResolvingCheckpointCredential);
            entered.notify_one();
            release.notified().await;
            CountingTextLoader {
                provider_calls: requests,
            }
            .load(request)
            .await
        })
    }
}

#[tokio::test]
async fn duration_includes_loader_but_does_not_release_its_owned_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(HeldDeadlineLoader {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Full).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("finish".to_owned())],
                limits: RunLimits {
                    max_duration_ms: Some(100),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("prompt")
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let premature = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if matches!(event.event, SessionEvent::RunFinished { run_id: finished, .. } if finished == run_id) {
                break;
            }
        }
    }).await;
    release.notify_one();
    assert!(
        premature.is_err(),
        "loader ownership must outlive expiry until it exits"
    );
    let observed = collect_until(&mut events, finished_for(run_id)).await;
    runtime.shutdown().await.unwrap();
    let exhaustion = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                outcome: RunOutcome::BudgetExhausted { exhaustion },
                ..
            } if exhaustion.limit == BudgetLimitKind::Duration => Some(exhaustion),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{observed:?}"));
    assert!(
        exhaustion
            .message
            .contains("resolving checkpoint reviewer credentials"),
        "{}",
        exhaustion.message
    );
    assert!(
        exhaustion.message.contains("no model request was started"),
        "{}",
        exhaustion.message
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "expired preparation must not send a model request"
    );
}

struct DeadlineLoader {
    requests: Arc<AtomicUsize>,
}

impl RuntimeLoader for DeadlineLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(DeadlineProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct DeadlineProvider {
    requests: Arc<AtomicUsize>,
}

impl Provider for DeadlineProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        use qq_provider::ProviderEvent;
        let events = if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                ProviderEvent::ToolCallStarted { id: "sleep".to_owned(), name: "shell".to_owned() },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "sleep".to_owned(),
                    json: serde_json::json!({"command": "sleep 30; printf late > late.txt", "timeout_seconds": 40}).to_string(),
                },
                ProviderEvent::ToolCallCompleted { id: "sleep".to_owned() },
                ProviderEvent::Completed { usage: None },
            ]
        } else {
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        };
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn duration_expires_during_shell_execution_and_drains_before_session_reuse() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(DeadlineLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Full).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let began = std::time::Instant::now();
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("run shell".to_owned())],
                limits: RunLimits {
                    max_duration_ms: Some(300),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("unexpected receipt")
    };
    let mut tool_started = false;
    let terminal = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            match event.event {
                SessionEvent::ToolCallStarted { .. } => tool_started = true,
                SessionEvent::RunFinished {
                    run_id: finished,
                    outcome,
                    ..
                } if finished == run_id => break outcome,
                _ => {}
            }
        }
    })
    .await;
    let elapsed = began.elapsed();
    // Even the red baseline must clean up its sleeping process before failing.
    if terminal.is_err() {
        runtime.shutdown().await.unwrap();
    }
    assert!(
        tool_started,
        "the deadline must interrupt an executing tool, not just preparation"
    );
    assert!(
        matches!(terminal, Ok(RunOutcome::BudgetExhausted { ref exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration),
        "elapsed={elapsed:?}, outcome={terminal:?}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "no final provider turn after expiry"
    );
    assert!(!directory.path().join("late.txt").exists());
    let next = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
    let observed = collect_until(&mut events, finished_for(next)).await;
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));
    runtime.shutdown().await.unwrap();
}
