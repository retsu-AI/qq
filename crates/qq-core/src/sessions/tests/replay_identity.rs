use super::*;

struct ReplayLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for ReplayLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(ReplayProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ReplayProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for ReplayProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        use qq_provider::ProviderEvent;
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        let path = match turn {
            0 => Some("first.txt"),
            1 => Some("missing.txt"),
            3 => Some("second.txt"),
            _ => None,
        };
        let events = match path {
            Some(path) => vec![
                ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0".to_owned(),
                    json: serde_json::json!({"path": path}).to_string(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_0".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ],
            None => vec![
                ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ],
        };
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }
}

#[tokio::test]
async fn repeated_call_ids_replay_their_own_results_after_follow_up_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("first.txt"), "first result\n").unwrap();
    std::fs::write(directory.path().join("second.txt"), "second result\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(ReplayLoader {
        requests: Arc::clone(&requests),
    });
    let options = || SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    let runtime = SessionRuntime::open(options(), loader.clone())
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    for prompt in ["read first and missing", "read second"] {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::text(prompt.to_owned())],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        collect_through_finished(&mut events).await;
    }
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);
    let runtime = SessionRuntime::open(options(), loader).await.unwrap();
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    collect_through_finished(&mut events).await;
    runtime.shutdown().await.unwrap();
    let requests = requests.lock().unwrap();
    for (request_index, expected_count) in [(3, 2), (5, 3)] {
        let results = requests[request_index]
            .messages()
            .iter()
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => Some((call_id, content, is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), expected_count);
        assert!(results.iter().all(|(id, _, _)| id.as_str() == "call_0"));
        assert!(results[0].1.contains("first result"), "{:?}", results[0]);
        assert!(!results[0].2);
        assert!(results[1].2);
        assert!(!results[1].1.contains(INTERRUPTED_TOOL_RESULT));
        if expected_count == 3 {
            assert!(results[2].1.contains("second result"));
            assert!(!results[2].2);
        }
    }
}
