use super::*;

struct AuditGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

struct Loader {
    scenario: &'static str,
    prepared: AtomicUsize,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    gate: Option<Arc<AuditGate>>,
}
struct Model {
    scenario: &'static str,
    audit: bool,
    turns: AtomicUsize,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    gate: Option<Arc<AuditGate>>,
}
impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.load_with_progress(request, RuntimeLoadProgress::default())
    }

    fn load_with_progress(
        &self,
        request: RuntimeLoadRequest,
        progress: RuntimeLoadProgress,
    ) -> RuntimeLoadFuture {
        let ordinal = self.prepared.load(Ordering::SeqCst);
        if ordinal >= 2 && self.scenario != "allowance" {
            return Box::pin(async move {
                progress.cancelled().await;
                Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "preparation stopped".to_owned(),
                })
            });
        }
        self.prepared.fetch_add(1, Ordering::SeqCst);
        let model = Model {
            scenario: self.scenario,
            audit: ordinal % 2 == 1,
            turns: AtomicUsize::new(0),
            requests: self.requests.clone(),
            gate: self.gate.clone(),
        };
        Box::pin(async move {
            Ok(loaded_runtime(
                Runtime::new(model, "audit-model", 256).unwrap(),
                &request.workspace,
                None,
            ))
        })
    }
}
impl Provider for Model {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let first = self.turns.fetch_add(1, Ordering::SeqCst) == 0;
        let mut events = if self.audit {
            assert!(
                first,
                "independent goal audit must have exactly one primary turn"
            );
            assert_eq!(request.tool_choice(), qq_provider::ToolChoice::None);
            assert!(request.tools().is_empty());
            assert!(
                request_texts(&request)
                    .iter()
                    .any(|text| text.contains("Independently audit"))
            );
            if self.scenario == "tools" {
                vec![
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "forbidden".to_owned(),
                        name: "shell".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "forbidden".to_owned(),
                        json: r#"{"command":"printf unexpected > marker"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "forbidden".to_owned(),
                    }),
                ]
            } else {
                let text = if self.scenario == "invalid" {
                    "not a JSON verdict".to_owned()
                } else {
                    serde_json::json!({"achieved":matches!(self.scenario,"confirm"|"stale"|"paused_edit"|"deadline"|"empty_evidence"|"oversized_evidence"),
                        "evidence":match self.scenario {"empty_evidence"=>" ".to_owned(),"oversized_evidence"=>"x".repeat(513),_=>"independently reviewed the persisted work and claim".to_owned()}}).to_string()
                };
                vec![Ok(qq_provider::ProviderEvent::OutputTextDelta { text })]
            }
        } else if first {
            vec![
                Ok(qq_provider::ProviderEvent::ToolCallStarted {id:"claim".to_owned(),name:"update_goal".to_owned()}),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {id:"claim".to_owned(),json:r#"{"status":{"type":"achieved","evidence":"the requested work is ready for independent verification"}}"#.to_owned()}),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {id:"claim".to_owned()}),
            ]
        } else {
            vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "work ready".to_owned(),
            })]
        };
        self.requests.lock().unwrap().push(request);
        events.push(Ok(qq_provider::ProviderEvent::Completed {
            usage: Some(qq_provider::ProviderUsage {
                input_tokens: 5,
                output_tokens: 3,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: None,
            }),
        }));
        let gate = self.gate.clone().filter(|_| self.audit);
        Box::pin(async_stream::stream! {
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.acquire().await.unwrap().forget();
            }
            for event in events { yield event; }
        })
    }
}

#[tokio::test]
async fn goal_model_audit_confirms_only_an_independent_valid_evidence_verdict() {
    exercise("confirm").await;
}
#[tokio::test]
async fn goal_model_audit_rejection_invalid_output_and_tools_return_to_work_without_repairs() {
    for scenario in [
        "reject",
        "invalid",
        "tools",
        "empty_evidence",
        "oversized_evidence",
    ] {
        exercise(scenario).await;
    }
}
#[tokio::test]
async fn goal_model_audit_allowance_refuses_a_ninth_audit_without_a_provider_request() {
    exercise("allowance").await;
}

async fn exercise(scenario: &'static str) {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("sessions.sqlite3");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(Loader {
        scenario,
        prepared: AtomicUsize::new(0),
        requests: requests.clone(),
        gate: None,
    });
    let runtime =
        SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader.clone())
            .await
            .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
        "type":"set_goal","session_id":session_id,"objective":"verify the persisted work independently",
        "budget":{"max_goal_runs":100,"deadline_ms":now_ms()+3_600_000}
    })).unwrap()).await.unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    let observed = async {
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut audits = 0;
        loop {
            let event = tokio::time::timeout_at(deadline, events.next())
                .await
                .unwrap_or_else(|error| {
                    panic!("{scenario}: independent bounded audit did not settle: {error}")
                })
                .unwrap()
                .unwrap();
            if let SessionEvent::GoalUpdated { goal } = &event.event
                && goal.counters.audits > audits
            {
                audits = goal.counters.audits;
                deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            }
            if let SessionEvent::GoalUpdated { goal } = event.event
                && ((scenario == "confirm" && goal.status == qq_protocol::GoalStatus::Achieved)
                    || (scenario == "allowance"
                        && matches!(
                            goal.status,
                            qq_protocol::GoalStatus::Blocked {
                                reason: qq_protocol::GoalBlockedReason::AuditExhausted
                            }
                        ))
                    || (!matches!(scenario, "confirm" | "allowance")
                        && goal.counters.audits == 1
                        && !goal.audit_pending))
            {
                break goal;
            }
        }
    }
    .await;
    assert_eq!(
        observed.counters.audits,
        if scenario == "allowance" { 8 } else { 1 }
    );
    assert_eq!(
        observed.counters.runs,
        if scenario == "allowance" {
            17
        } else if scenario == "tools" {
            1
        } else {
            2
        }
    );
    assert!(!directory.path().join("marker").exists());
    if !matches!(scenario, "confirm" | "allowance") {
        assert_eq!(observed.status, qq_protocol::GoalStatus::Active);
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: observed.goal_id,
                        revision: observed.revision,
                    },
                    action: qq_protocol::GoalControl::Pause,
                },
            )
            .await
            .unwrap();
    }
    runtime.shutdown().await.unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        if scenario == "allowance" { 26 } else { 3 }
    );
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert!(snapshot.runs.iter().all(|run| run.outcome.is_some()));
    let verdicts = snapshot
        .runs
        .iter()
        .filter(|run| run.final_output.is_some())
        .count();
    assert_eq!(
        verdicts,
        if scenario == "allowance" {
            8
        } else if scenario == "tools" {
            0
        } else {
            1
        }
    );
    drop(events);
    drop(runtime);
    let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database), loader)
        .await
        .unwrap();
    let restored = reopened
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(restored.goal, snapshot.goal);
    assert_eq!(restored.runs, snapshot.runs);
    assert_eq!(
        requests.lock().unwrap().len(),
        if scenario == "allowance" { 26 } else { 3 }
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn goal_model_audit_late_and_stale_confirmation_cannot_achieve() {
    for scenario in ["deadline", "stale", "paused_edit"] {
        let start = now_ms();
        let epoch = super::goal_driver_soak::Epoch::new(start);
        let directory = TempDir::new().unwrap();
        let gate = Arc::new(AuditGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let loader = Arc::new(Loader {
            scenario,
            prepared: AtomicUsize::new(0),
            requests: requests.clone(),
            gate: Some(gate.clone()),
        });
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            loader,
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("missing session")
        };
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal","session_id":session_id,"objective":"original objective",
                    "budget":{"max_goal_runs":100,"deadline_ms":start+600_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .unwrap();
        let (audit, goal) = tokio::time::timeout(Duration::from_secs(10), async {
            let mut latest = None;
            loop {
                let event = events.next().await.unwrap().unwrap();
                match event.event {
                    SessionEvent::GoalUpdated { goal } => latest = Some(goal),
                    SessionEvent::RunStarted { run_id, .. }
                        if latest
                            .as_ref()
                            .is_some_and(|goal| goal.counters.audits == 1) =>
                    {
                        break (run_id, latest.unwrap());
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(requests.lock().unwrap().len(), 3);
        let mut edited_identity = None;
        if scenario == "deadline" {
            epoch.advance(start + 600_001);
        } else {
            let mut identity = qq_protocol::GoalIdentity {
                goal_id: goal.goal_id,
                revision: goal.revision,
            };
            if scenario == "paused_edit" {
                let receipt = runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::GoalControl {
                            session_id,
                            expected: identity,
                            action: qq_protocol::GoalControl::Pause,
                        },
                    )
                    .await
                    .unwrap();
                let CommandOutcome::GoalUpdated { goal: paused, .. } = receipt.outcome else {
                    panic!("paused goal")
                };
                assert!(paused.audit_pending && paused.achievement_claim.is_some());
                assert_eq!(paused.audit_revision, Some(paused.revision));
                identity.revision = paused.revision;
            }
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    serde_json::from_value(serde_json::json!({
                        "type":"set_goal","session_id":session_id,"objective":"edited objective",
                        "expected":identity,
                        "budget":{"max_goal_runs":100,"deadline_ms":start+600_000}
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
            if scenario == "paused_edit" {
                let CommandOutcome::GoalUpdated { goal: edited, .. } = receipt.outcome else {
                    panic!("edited goal")
                };
                assert!(
                    !edited.audit_pending
                        && edited.audit_revision.is_none()
                        && edited.achievement_claim.is_none()
                );
                assert_eq!(
                    edited.status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::User
                    }
                );
                let identity = qq_protocol::GoalIdentity {
                    goal_id: edited.goal_id,
                    revision: edited.revision,
                };
                edited_identity = Some(identity);
            }
        }
        gate.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let SessionEvent::RunFinished { run_id, .. } =
                    events.next().await.unwrap().unwrap().event
                    && run_id == audit
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        if let Some(expected) = edited_identity {
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::GoalControl {
                        session_id,
                        expected,
                        action: qq_protocol::GoalControl::Resume,
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::GoalUpdated { goal: resumed, .. } = receipt.outcome else {
                panic!("resumed goal")
            };
            assert!(
                !resumed.audit_pending
                    && resumed.audit_revision.is_none()
                    && resumed.achievement_claim.is_none()
            );
            assert_eq!(resumed.status, qq_protocol::GoalStatus::Active);
        }
        runtime.shutdown().await.unwrap();
        let focused = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        let actual = focused.goal.unwrap();
        if scenario == "deadline" {
            assert!(matches!(
                actual.status,
                qq_protocol::GoalStatus::BudgetExhausted { .. }
            ));
        } else {
            assert_eq!(actual.objective, "edited objective");
            assert_eq!(
                actual.revision,
                goal.revision + if scenario == "paused_edit" { 3 } else { 1 }
            );
            assert_eq!(actual.status, qq_protocol::GoalStatus::Active);
            assert!(!actual.audit_pending);
        }
        assert_ne!(actual.status, qq_protocol::GoalStatus::Achieved);
        assert_eq!(requests.lock().unwrap().len(), 3);
    }
}
