use super::*;

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
