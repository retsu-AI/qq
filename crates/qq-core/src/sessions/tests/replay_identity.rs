use super::*;

#[tokio::test]
#[ignore = "manual same-host reconstruction comparison"]
async fn measure_replay_follow_up() {
    for tool in ["read_file", "shell"] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "payload ".repeat(1500)).unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let args = if tool == "read_file" {
            serde_json::json!({"path": "note.txt"})
        } else {
            serde_json::json!({"command": format!("echo {}", "payload ".repeat(1500))})
        };
        let script = (0..24)
            .flat_map(|_| [Some((tool, args.clone())), None])
            .collect();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ReplayLoader {
                requests,
                script: Arc::new(script),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Full).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!()
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let mut samples = Vec::new();
        for ordinal in 0..54 {
            let start = std::time::Instant::now();
            let run = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
            let observed = collect_until(&mut events, finished_for(run)).await;
            let elapsed = start.elapsed().as_nanos();
            assert!(observed.iter().any(|event| matches!(
                event.event,
                SessionEvent::RunFinished {
                    outcome: RunOutcome::Completed,
                    ..
                }
            )));
            if ordinal >= 24 {
                samples.push(elapsed);
            }
        }
        runtime.shutdown().await.unwrap();
        println!("replay_follow_up {tool} ns={samples:?}");
    }
}

type ReplayScript = Vec<Option<(&'static str, serde_json::Value)>>;

struct ReplayLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: Arc<ReplayScript>,
}

impl RuntimeLoader for ReplayLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        let script = Arc::clone(&self.script);
        Box::pin(async move {
            Runtime::new(ReplayProvider { requests, script }, "test-model", 256)
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
    script: Arc<ReplayScript>,
}

impl Provider for ReplayProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        use qq_provider::ProviderEvent;
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        let summarizing = request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"));
        requests.push(request);
        drop(requests);
        let events = match self.script.get(turn).and_then(Option::as_ref) {
            Some((name, arguments)) => vec![
                ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: (*name).to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0".to_owned(),
                    json: arguments.to_string(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_0".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ],
            None => vec![
                ProviderEvent::OutputTextDelta {
                    text: if summarizing {
                        valid_summary("folded")
                    } else {
                        "done".to_owned()
                    },
                },
                ProviderEvent::Completed { usage: None },
            ],
        };
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }
}

#[tokio::test]
async fn repeated_call_ids_replay_their_own_results_after_follow_up_and_reopen() {
    check_repeated_call_replay(false).await;
}

#[tokio::test]
async fn missing_result_cannot_borrow_a_later_turns_result_with_the_same_id() {
    check_repeated_call_replay(true).await;
}

async fn check_repeated_call_replay(missing_first_result: bool) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("first.txt"), "first result\n").unwrap();
    std::fs::write(directory.path().join("second.txt"), "second result\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(ReplayLoader {
        requests: Arc::clone(&requests),
        script: Arc::new(vec![
            Some(("read_file", serde_json::json!({"path": "first.txt"}))),
            Some((
                "read_file",
                serde_json::json!({"path": if missing_first_result { "second.txt" } else { "missing.txt" }}),
            )),
            None,
            Some(("read_file", serde_json::json!({"path": "second.txt"}))),
            None,
            None,
            None,
            Some((
                "search_history",
                serde_json::json!({"query": "result", "limit": 8}),
            )),
        ]),
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
    let mut first_run = None;
    for prompt in ["read first and missing", "read second"] {
        let receipt = runtime
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
        if let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome {
            first_run.get_or_insert(run_id);
        }
        let observed = collect_through_finished(&mut events).await;
        assert!(observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        )));
    }
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);
    if missing_first_result {
        // Legacy crash fixture: the assistant turn committed, but the old
        // writer never created its tool row. Exercise recovery via reopen.
        let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        assert_eq!(
            connection
                .execute(
                    "DELETE FROM tool_calls WHERE run_id = ?1 AND turn_ordinal = 1",
                    [first_run.unwrap().to_string()],
                )
                .unwrap(),
            1
        );
    }
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
    compact_session(&runtime, session_id).await;
    collect_through_compacted(&mut events).await;
    let run = queue_prompt(&runtime, session_id, "recover history".to_owned()).await;
    let observed = collect_until(&mut events, finished_for(run)).await;
    let history = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "search_history" => {
                tool_call.result.as_deref()
            }
            _ => None,
        })
        .expect("history tool completed");
    if !missing_first_result {
        assert!(history.contains("first result"), "{history}");
    }
    assert!(history.contains("second result"), "{history}");
    assert!(!history.contains(INTERRUPTED_TOOL_RESULT));
    runtime.shutdown().await.unwrap();
    let requests = requests.lock().unwrap();
    for (request_index, expected_count) in [(3, 2), (5, 3), (6, 3)] {
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
        if missing_first_result && request_index >= 5 {
            assert_eq!(results[0].1, INTERRUPTED_TOOL_RESULT);
            assert!(results[0].2);
        } else {
            assert!(results[0].1.contains("first result"), "{:?}", results[0]);
            assert!(!results[0].2);
        }
        assert_eq!(*results[1].2, !missing_first_result);
        if missing_first_result {
            assert!(results[1].1.contains("second result"));
        }
        assert!(!results[1].1.contains(INTERRUPTED_TOOL_RESULT));
        if expected_count == 3 {
            assert!(results[2].1.contains("second result"));
            assert!(!results[2].2);
        }
    }
}

#[tokio::test]
async fn repeated_call_ids_keep_pruning_effects_and_arguments_local_to_each_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("first.txt"), "first".repeat(200)).unwrap();
    std::fs::write(directory.path().join("second.txt"), "second".repeat(200)).unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ReplayLoader {
            requests: Arc::clone(&requests),
            script: Arc::new(vec![
                Some((
                    "shell",
                    serde_json::json!({"command": format!("echo {}", "durable".repeat(100))}),
                )),
                None,
                Some(("read_file", serde_json::json!({"path": "first.txt"}))),
                Some(("read_file", serde_json::json!({"path": "second.txt"}))),
            ]),
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
    for prompt in [
        "shell",
        "reads",
        "age one",
        "age two",
        "age three",
        "inspect",
    ] {
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
        let observed = collect_through_finished(&mut events).await;
        assert!(observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        )));
    }
    runtime.shutdown().await.unwrap();
    let requests = requests.lock().unwrap();
    let results = requests
        .last()
        .unwrap()
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 3);
    assert!(!results[0].1);
    assert!(
        results[0].0.contains(&"durable".repeat(100)),
        "shell output must stay verbatim: {}",
        results[0].0
    );
    assert!(
        results[1]
            .0
            .contains("[pruned: read_file {\"path\":\"first.txt\"}"),
        "{}",
        results[1].0
    );
    assert!(
        results[2]
            .0
            .contains("[pruned: read_file {\"path\":\"second.txt\"}"),
        "{}",
        results[2].0
    );
}
