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

/// One turn of `count` `read_file` calls, then text. Every provider request
/// is captured so the test can compare what the model saw live against
/// what later runs replay.
struct FanOutReadsProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    count: usize,
}

impl Provider for FanOutReadsProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        use qq_provider::ProviderEvent;
        let has_results = request.messages().iter().any(|message| {
            message
                .content()
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        self.requests.lock().unwrap().push(request);
        if has_results {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let mut events = Vec::with_capacity(self.count * 3 + 1);
        for index in 0..self.count {
            let id = format!("read-{index}");
            events.push(Ok(ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: "read_file".to_owned(),
            }));
            events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: format!(r#"{{"path":"big-{index}.txt","limit":2000}}"#),
            }));
            events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

struct FanOutReadsLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    count: usize,
}

impl RuntimeLoader for FanOutReadsLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        let count = self.count;
        Box::pin(async move {
            Runtime::new(FanOutReadsProvider { requests, count }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

/// The `(call_id, content)` of every `ToolResult` block in a request, in
/// order.
fn tool_result_blocks(request: &ModelRequest) -> Vec<(String, String)> {
    request
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                call_id, content, ..
            } => Some((call_id.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

/// F23 (ENG-804): four ~30 KiB reads in one turn (each inside `read_file`'s
/// own 32 KiB bound) exceed the 96 KiB per-turn output budget, so the live
/// request re-bounds the late results.
/// The follow-up run, the reopened store, and the compaction summarizer
/// must all replay exactly the bytes the model saw live — not the larger
/// per-call results — and every span the budget cut must stay recoverable
/// through `read_tool_result`.
#[tokio::test]
async fn turn_budget_projection_replays_identically_after_follow_up_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let line = format!("{}\n", "z".repeat(63));
    for index in 0..4 {
        std::fs::write(
            directory.path().join(format!("big-{index}.txt")),
            line.repeat(470),
        )
        .unwrap();
    }
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(FanOutReadsLoader {
        requests: Arc::clone(&requests),
        count: 4,
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
    let first = queue_prompt(&runtime, session_id, "read all four".to_owned()).await;
    let observed = collect_until(&mut events, finished_for(first)).await;
    // Reads overlap, so finished events arrive in any order; index them by
    // call ordinal, which is the order results enter context.
    let mut finished: Vec<ToolCallSnapshot> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call } => Some(tool_call.clone()),
            _ => None,
        })
        .collect();
    finished.sort_by_key(|call| call.call_ordinal);
    assert_eq!(finished.len(), 4, "{finished:?}");
    for call in &finished {
        assert!(!call.is_error, "{call:?}");
        let stored = call.result.as_deref().unwrap();
        assert!(
            (29 * 1024..=31 * 1024).contains(&stored.len()),
            "each call's own bounded result is ~30 KiB: {}",
            stored.len()
        );
    }

    // The live request that followed the tool turn is the oracle.
    let live = {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        tool_result_blocks(&requests[1])
    };
    assert_eq!(live.len(), 4);
    let live_total: usize = live.iter().map(|(_, content)| content.len()).sum();
    assert!(
        live_total <= crate::tools::output::MAX_TURN_TOOL_OUTPUT_BYTES,
        "{live_total}"
    );
    let cut: Vec<usize> = (0..4)
        .filter(|index| live[*index].1.contains("turn budget reached"))
        .collect();
    assert!(
        !cut.is_empty(),
        "the budget cut at least one result: {live:?}"
    );
    for index in &cut {
        assert!(
            live[*index].1.len() < finished[*index].result.as_deref().unwrap().len(),
            "the projection is smaller than the stored result"
        );
    }

    // Follow-up run in the same process: assembly replays the turn.
    let second = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
    collect_until(&mut events, finished_for(second)).await;
    let replayed = {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        tool_result_blocks(&requests[2])
    };
    assert_eq!(
        replayed, live,
        "the follow-up request must replay the live projection byte for byte"
    );
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);

    // Reopened store: assembly from disk alone, and the joined loader
    // agrees with the reference oracle.
    assert_assembly_matches_reference(&directory.path().join("sessions.sqlite3"), session_id);
    let runtime = SessionRuntime::open(options(), loader).await.unwrap();
    let third = queue_prompt(&runtime, session_id, "and again".to_owned()).await;
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    collect_until(&mut events, finished_for(third)).await;
    let reopened = {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        tool_result_blocks(&requests[3])
    };
    assert_eq!(
        reopened, live,
        "a reopened store must replay the live projection byte for byte"
    );

    // Every cut span keeps a recall path: the marker names a handle the
    // store answers with the complete output.
    for index in &cut {
        let content = &live[*index].1;
        let marker = content
            .lines()
            .find(|line| line.starts_with("…[qq: "))
            .unwrap_or_else(|| panic!("a cut result carries a marker: {content}"));
        assert!(!marker.contains("not stored"), "{marker}");
        let start = marker.find("full output t:").expect(marker) + "full output ".len();
        let end = marker[start..].find([';', ']']).unwrap() + start;
        let handle = crate::runtime::SpillHandle::parse(&marker[start..end])
            .unwrap_or_else(|| panic!("a parsable handle: {marker}"));
        let read = runtime
            .inner
            .store
            .read_tool_spill(session_id, handle.call_prefix, handle.digest_prefix)
            .await
            .unwrap();
        let crate::runtime::SpillRead::Found { text, .. } = read else {
            panic!("the cut output is stored: {read:?}");
        };
        // The call did not spill (its own bound left it whole), so the recall
        // path returns the persisted per-call result: exactly the bytes the
        // turn budget cut from.
        assert_eq!(&text, finished[*index].result.as_deref().unwrap());
        assert!(text.len() > content.len());
    }
    runtime.shutdown().await.unwrap();
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

struct AnthropicReplayLoader(Arc<dyn Provider>);
impl RuntimeLoader for AnthropicReplayLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = self.0.clone();
        Box::pin(async move {
            Runtime::new(provider, "claude-opus-5-5", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

#[tokio::test]
async fn anthropic_signed_tool_turn_survives_runtime_restart() {
    use qq_provider::{
        EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ProviderCompiler, ProviderRecipe,
    };
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut captured = Vec::new();
        for turn in 0..3 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let body = loop {
                let count = socket.read(&mut chunk).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(
                            &bytes[end + 4..end + 4 + length],
                        )
                        .unwrap();
                    }
                }
            };
            captured.push(body);
            let blocks = if turn == 0 {
                vec![
                    serde_json::json!({"type":"thinking","thinking":"inspect file","signature":"opaque-signed"}),
                    serde_json::json!({"type":"tool_use","id":"call_0","name":"read_file","input":{}}),
                ]
            } else {
                vec![serde_json::json!({"type":"text","text":"done"})]
            };
            let mut events = vec![
                serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
            ];
            for (index, block) in blocks.into_iter().enumerate() {
                events.push(serde_json::json!({"type":"content_block_start","index":index,"content_block":block}));
                if turn == 0 && index == 1 {
                    events.push(serde_json::json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"note.txt\"}"}}));
                }
                events.push(serde_json::json!({"type":"content_block_stop","index":index}));
            }
            events.push(serde_json::json!({"type":"message_delta","delta":{"stop_reason":if turn == 0 {"tool_use"} else {"end_turn"}}}));
            events.push(serde_json::json!({"type":"message_stop"}));
            let response = events
                .into_iter()
                .map(|value| format!("data: {value}\n\n"))
                .collect::<String>();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
        }
        captured
    });
    let provider = ProviderCompiler::new()
        .unwrap()
        .compile(ProviderRecipe::http(HttpProviderRecipe::new(
            EndpointSpec::base(endpoint, true),
            HttpProtocol::AnthropicMessages,
            HttpAuth::NoAuth,
        )))
        .unwrap();
    let loader = Arc::new(AnthropicReplayLoader(provider));
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool payload").unwrap();
    let options = || SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    let runtime = SessionRuntime::open(options(), loader.clone())
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Full).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!()
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run = queue_prompt(&runtime, session_id, "read note".into()).await;
    let observed = collect_until(&mut events, finished_for(run)).await;
    assert!(
        observed.iter().any(|e| matches!(
            e.event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        )),
        "{observed:?}"
    );
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);
    let runtime = SessionRuntime::open(options(), loader).await.unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run = queue_prompt(&runtime, session_id, "continue".into()).await;
    collect_until(&mut events, finished_for(run)).await;
    runtime.shutdown().await.unwrap();
    let requests = server.join().unwrap();
    for request in &requests[1..] {
        let messages = request["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|m| m["content"][0]["type"] == "thinking")
            .unwrap_or_else(|| panic!("signed thinking missing: {request}"));
        assert_eq!(assistant["content"][0]["signature"], "opaque-signed");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert!(
            messages
                .iter()
                .any(|m| m["content"].as_array().is_some_and(|blocks| blocks
                    .iter()
                    .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "call_0")))
        );
    }
}
