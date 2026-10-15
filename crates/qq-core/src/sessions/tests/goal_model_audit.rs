use super::*;

struct Loader {
    scenario: &'static str,
    prepared: AtomicUsize,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}
struct Model {
    scenario: &'static str,
    audit: bool,
    turns: AtomicUsize,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}
impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let ordinal = self.prepared.load(Ordering::SeqCst);
        if ordinal >= 2 && self.scenario != "allowance" {
            return Box::pin(std::future::pending());
        }
        self.prepared.fetch_add(1, Ordering::SeqCst);
        let model = Model {
            scenario: self.scenario,
            audit: ordinal % 2 == 1,
            turns: AtomicUsize::new(0),
            requests: self.requests.clone(),
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
                    serde_json::json!({"achieved":self.scenario=="confirm","evidence":"independently reviewed the persisted work and claim"}).to_string()
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
        Box::pin(stream::iter(events))
    }
}

#[tokio::test]
async fn goal_model_audit_confirms_only_an_independent_valid_evidence_verdict() {
    exercise("confirm").await;
}
#[tokio::test]
async fn goal_model_audit_rejection_invalid_output_and_tools_return_to_work_without_repairs() {
    for scenario in ["reject", "invalid", "tools"] {
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
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
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
    })
    .await
    .expect("a pending no-check goal must receive an independent bounded audit");
    assert_eq!(
        observed.counters.audits,
        if scenario == "allowance" { 8 } else { 1 }
    );
    assert_eq!(
        observed.counters.runs,
        if scenario == "allowance" { 17 } else { 2 }
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
