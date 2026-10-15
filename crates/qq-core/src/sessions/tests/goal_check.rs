use super::*;

#[tokio::test]
async fn goal_check_once_executes_without_a_model_and_achieves_durably() {
    struct Loader {
        loads: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let mut events = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "claim".to_owned(), name: "update_goal".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "claim".to_owned(),
                        json: r#"{"status":{"type":"achieved","evidence":"work is ready for independent verification"}}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted { id: "claim".to_owned() }),
                ]
            } else {
                vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "check my work".to_owned(),
                })]
            };
            events.push(Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
            }));
            Box::pin(stream::iter(events))
        }
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.loads.fetch_add(1, Ordering::SeqCst);
            let calls = self.calls.clone();
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model(calls), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    struct Authority;
    impl WorkspaceGrantAuthority for Authority {
        fn seed_grants(&self, _: &Path) -> GrantSeedFuture {
            Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
        }
        fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> GrantPromotionFuture {
            panic!("exact goal-check authority must not promote ordinary grants")
        }
        fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert_eq!(command, "printf checked >> goal-check-marker");
            Box::pin(std::future::ready(Ok(())))
        }
    }
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("sessions.sqlite3");
    let loads = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(database.clone());
    options.grant_authority = Some(Arc::new(Authority));
    let runtime = SessionRuntime::open(
        options,
        Arc::new(Loader {
            loads: loads.clone(),
            calls: calls.clone(),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"verify checked completion",
                "check":{"command":"printf checked >> goal-check-marker","timeout_ms":5000},
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing goal")
    };
    assert!(!directory.path().join("goal-check-marker").exists());
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::Once,
                    command: "printf checked >> goal-check-marker".to_owned(),
                },
            },
        )
        .await
        .unwrap();
    let work = collect_through_finished_generously(&mut events).await;
    assert!(
        work.iter().any(|event| matches!(&event.event,
            SessionEvent::GoalUpdated { goal }
                if goal.audit_pending && goal.achievement_claim.is_some()
                    && goal.status == qq_protocol::GoalStatus::Active
        )),
        "a model achievement proposal must remain active pending independent verification"
    );
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let achieved = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::GoalUpdated { goal } = event.event {
                if matches!(goal.status, qq_protocol::GoalStatus::Achieved) {
                    break goal;
                }
            }
        }
    })
    .await
    .expect("authorized exit-zero check must execute and achieve the goal");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("goal-check-marker")).unwrap(),
        "checked"
    );
    assert_eq!(
        achieved.check_authorization,
        Some(qq_protocol::GoalCheckAuthorization::Consumed)
    );
    assert_eq!(achieved.counters.audits, 1);
    assert_eq!(achieved.counters.runs, 1, "checks are not model runs");
    assert_eq!(
        loads.load(Ordering::SeqCst),
        1,
        "a check must not load a model"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "only the work run may call the model"
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
    assert_eq!(snapshot.runs.len(), 2);
    assert_eq!(
        snapshot
            .runs
            .iter()
            .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .count(),
        1
    );
    assert!(
        snapshot
            .runs
            .iter()
            .all(|run| run.outcome == Some(RunOutcome::Completed))
    );
    runtime.shutdown().await.unwrap();
    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(database),
        Arc::new(Loader {
            loads: loads.clone(),
            calls: calls.clone(),
        }),
    )
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
    assert_eq!(restored.goal, Some(achieved));
    assert_eq!(restored.runs, snapshot.runs);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    reopened.shutdown().await.unwrap();
}
