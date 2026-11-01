use super::*;

fn is_checkpoint_request(request: &ModelRequest) -> bool {
    last_user_text(request) == Some(SLICE_CHECKPOINT_NOTICE)
}

fn last_user_text(request: &ModelRequest) -> Option<&str> {
    let message = request.messages().last()?;
    if message.role() != Role::User {
        return None;
    }
    match message.content() {
        [ContentBlock::Text { text }] => Some(text.as_str()),
        _ => None,
    }
}

#[tokio::test]
async fn the_measured_token_chain_survives_the_slice_checkpoint_and_continuation() {
    struct MeasuredCheckpoint {
        emitted: Mutex<usize>,
    }

    impl Provider for MeasuredCheckpoint {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let usage = Some(qq_provider::ProviderUsage {
                input_tokens: 1_000,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 1,
                reasoning_tokens: None,
            });
            if is_checkpoint_request(&request) {
                assert!(
                    !request.tools().is_empty(),
                    "tools stay declared on the checkpoint turn"
                );
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "slice checkpoint".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage }),
                ]));
            }
            let mut emitted = self.emitted.lock().unwrap();
            if *emitted >= MAX_TOOL_CALLS_PER_SLICE + 2 {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "task complete".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage }),
                ]));
            }
            let first = *emitted;
            let count = if first < MAX_TOOL_CALLS_PER_SLICE {
                (MAX_TOOL_CALLS_PER_SLICE - first).min(MAX_TOOL_CALLS_PER_TURN - 1)
            } else {
                1
            };
            *emitted += count;
            drop(emitted);
            let mut events = Vec::with_capacity(count * 3 + 1);
            for index in first..first + count {
                let id = format!("call-{index}");
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "unknown".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: "{}".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(ProviderEvent::Completed { usage }));
            Box::pin(stream::iter(events))
        }
    }

    let runtime = Runtime::new(
        MeasuredCheckpoint {
            emitted: Mutex::new(0),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("finish a long task")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Auto)),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let prepared: Vec<(u32, Option<u64>)> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Prepared {
                turn_ordinal,
                weight,
                ..
            } => Some((*turn_ordinal, weight.compatible_input_tokens)),
            _ => None,
        })
        .collect();
    let prefixes = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Prepared { static_prefix, .. } => Some(*static_prefix),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(prefixes.len() > 3);
    assert!(
        prefixes.iter().all(|prefix| *prefix == prefixes[0]),
        "{prefixes:?}"
    );
    assert_eq!(prepared[0].1, None);
    let unmeasured: Vec<u32> = prepared[1..]
        .iter()
        .filter(|(_, tokens)| tokens.is_none())
        .map(|(turn, _)| *turn)
        .collect();
    assert!(
        unmeasured.is_empty(),
        "turns without a measured chain: {unmeasured:?}"
    );
    for (turn, tokens) in &prepared[1..] {
        let tokens = tokens.expect("measured");
        assert!((900..=1_400).contains(&tokens), "turn {turn}: {tokens}");
    }
    let raw_bytes = sessions::context::estimate_tokens(MAX_TOOL_CALLS_PER_SLICE as u64 * 64);
    assert!(
        prepared[1..]
            .iter()
            .all(|(_, tokens)| tokens.is_some_and(|tokens| tokens < raw_bytes)),
        "{prepared:?}"
    );
}

#[tokio::test]
async fn renews_the_internal_tool_budget_until_the_task_completes() {
    struct CompletesAfterCheckpoint {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
        emitted: Arc<Mutex<usize>>,
        checkpoint_at: Arc<Mutex<Option<usize>>>,
    }

    impl Provider for CompletesAfterCheckpoint {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            if is_checkpoint_request(&request) {
                let emitted = *self.emitted.lock().unwrap();
                *self.checkpoint_at.lock().unwrap() = Some(emitted);
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "slice checkpoint".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }

            let mut emitted = self.emitted.lock().unwrap();
            let required = MAX_TOOL_CALLS_PER_SLICE + 1;
            if *emitted >= required {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "task complete".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }

            let first = *emitted;
            let count = (required - first).min(MAX_TOOL_CALLS_PER_TURN - 1);
            *emitted += count;
            drop(emitted);
            let mut events = Vec::with_capacity(count * 3 + 1);
            for index in first..first + count {
                let id = format!("call-{index}");
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "unknown".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: "{}".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let emitted = Arc::new(Mutex::new(0));
    let checkpoint_at = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        CompletesAfterCheckpoint {
            requests: Arc::clone(&requests),
            emitted: Arc::clone(&emitted),
            checkpoint_at: Arc::clone(&checkpoint_at),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run(RunCommand::new("finish a long task"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.last(), Some(&RunEvent::Completed));
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::OutputTextDelta { text } if text == "slice checkpoint"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::OutputTextDelta { text } if text == "task complete"
    )));
    assert_eq!(*emitted.lock().unwrap(), MAX_TOOL_CALLS_PER_SLICE + 1);
    assert_eq!(*checkpoint_at.lock().unwrap(), Some(255));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::Completed))
            .count(),
        1
    );
    let requests = requests.lock().unwrap();
    let checkpoint_index = requests.iter().position(is_checkpoint_request).unwrap();
    assert!(!requests[checkpoint_index].tools().is_empty());
    assert!(!requests[checkpoint_index + 1].tools().is_empty());
    assert!(!is_checkpoint_request(&requests[checkpoint_index + 1]));
}

#[tokio::test]
async fn a_tool_call_on_the_checkpoint_turn_settles_as_a_rejection_and_the_run_continues() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct CallsOnCheckpoint {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
        emitted: Mutex<usize>,
        checkpoint_seen: AtomicBool,
    }

    impl Provider for CallsOnCheckpoint {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            let read = |id: String, offset: usize| {
                [
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: serde_json::json!({"path":"note.txt","offset":offset,"limit":1})
                            .to_string(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id }),
                ]
            };
            if is_checkpoint_request(&request) {
                self.checkpoint_seen.store(true, Ordering::SeqCst);
                let mut events = Vec::from(read("checkpoint-call".to_owned(), 1));
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                return Box::pin(stream::iter(events));
            }
            if self.checkpoint_seen.load(Ordering::SeqCst) {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "task complete".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let mut emitted = self.emitted.lock().unwrap();
            let first = *emitted;
            let count = (MAX_TOOL_CALLS_PER_SLICE - 1 - first).min(MAX_TOOL_CALLS_PER_TURN - 1);
            *emitted += count;
            drop(emitted);
            let mut events = Vec::with_capacity(count * 3 + 1);
            for index in first..first + count {
                if index == first {
                    let id = format!("call-{index}");
                    events.extend([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: id.clone(),
                            name: "write_file".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: id.clone(),
                            json: r#"{"path":"progress.txt","content":"x"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted { id }),
                    ]);
                } else {
                    events.extend(read(format!("call-{index}"), index + 1));
                }
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("note.txt"),
        (0..MAX_TOOL_CALLS_PER_SLICE)
            .map(|index| format!("hello {index}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        CallsOnCheckpoint {
            requests: Arc::clone(&requests),
            emitted: Mutex::new(0),
            checkpoint_seen: AtomicBool::new(false),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("finish a long task")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{:?}",
        events.last()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Failed { .. })),
    );
    let executed = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::ToolCallFinished {
                    is_error: false,
                    ..
                }
            )
        })
        .count();
    assert_eq!(executed, MAX_TOOL_CALLS_PER_SLICE - 1);
    let rejected: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallUnexecuted {
                interrupted: false,
                message: result,
                ..
            } => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0], SLICE_CHECKPOINT_REJECTION);

    let requests = requests.lock().unwrap();
    let checkpoint_index = requests.iter().position(is_checkpoint_request).unwrap();
    assert!(!requests[checkpoint_index].tools().is_empty());
    let continuation = &requests[checkpoint_index + 1];
    assert!(!continuation.tools().is_empty());
    assert_eq!(requests[checkpoint_index].system(), requests[0].system());
    assert_eq!(continuation.system(), requests[0].system());
    assert_eq!(
        last_user_text(continuation),
        Some(SLICE_CONTINUATION_NOTICE)
    );
    let messages = continuation.messages();
    assert!(matches!(
        messages[messages.len() - 2].content(),
        [ContentBlock::ToolResult { call_id, content, is_error: true }]
            if call_id == "checkpoint-call" && content == SLICE_CHECKPOINT_REJECTION
    ));
    assert_eq!(requests.len(), checkpoint_index + 2);
}

#[tokio::test]
async fn calls_past_the_per_turn_cap_settle_as_tool_errors_and_the_run_continues() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct TwentyReadsThenAnswer {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for TwentyReadsThenAnswer {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 1 {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let mut events = Vec::new();
            for index in 0..MAX_TOOL_CALLS_PER_TURN + 4 {
                let id = format!("call-{index}");
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "read_file".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: serde_json::json!({"path":"note.txt","offset":index+1,"limit":1})
                        .to_string(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("note.txt"),
        (0..MAX_TOOL_CALLS_PER_TURN + 4)
            .map(|index| format!("hello {index}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TwentyReadsThenAnswer {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("read it twenty times")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let results: Vec<(&str, bool)> = requests[1]
        .messages()
        .last()
        .unwrap()
        .content()
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => (content.as_str(), *is_error),
            _ => panic!("tool-result message"),
        })
        .collect();
    assert_eq!(results.len(), MAX_TOOL_CALLS_PER_TURN + 4);
    for (content, is_error) in &results[..MAX_TOOL_CALLS_PER_TURN] {
        assert!(!is_error, "{content}");
        assert!(content.contains("hello"));
    }
    for (content, is_error) in &results[MAX_TOOL_CALLS_PER_TURN..] {
        assert!(is_error);
        assert!(content.contains("not executed"), "{content}");
        assert!(content.contains("call this again next turn"), "{content}");
    }
}

#[tokio::test]
async fn a_mixed_turn_overlaps_its_leading_reads_then_runs_the_rest_in_order() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct MixedTurn {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for MixedTurn {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 1 {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let calls = [
                ("r1", "read_file", r#"{"path":"note.txt"}"#),
                ("r2", "read_file", r#"{"path":"note.txt"}"#),
                ("r3", "read_file", r#"{"path":"note.txt"}"#),
                (
                    "e1",
                    "edit_file",
                    r#"{"edits":[{"path":"note.txt","old":"before\n","new":"after\n"}]}"#,
                ),
                ("r4", "read_file", r#"{"path":"note.txt"}"#),
            ];
            let mut events = Vec::new();
            for (id, name, json) in calls {
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.to_owned(),
                    name: name.to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.to_owned(),
                    json: json.to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id: id.to_owned() }));
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "before\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        MixedTurn {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("read, edit, read")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let requests = requests.lock().unwrap();
    let results: Vec<(&str, &str, bool)> = requests[1]
        .messages()
        .last()
        .unwrap()
        .content()
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => (call_id.as_str(), content.as_str(), *is_error),
            _ => panic!("tool-result message"),
        })
        .collect();
    assert_eq!(
        results.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
        ["r1", "r2", "r3", "e1", "r4"]
    );
    for (id, content, is_error) in &results[..3] {
        assert!(!is_error, "{id}: {content}");
        assert!(content.contains("before"), "{id}: {content}");
    }
    assert!(!results[3].2, "{}", results[3].1);
    assert!(results[4].1.contains("after"), "{}", results[4].1);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "after\n"
    );
}

#[tokio::test]
async fn enforces_the_admitted_call_limit_and_checkpoint_request_contract() {
    struct TooManyInOneTurn;

    impl Provider for TooManyInOneTurn {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let events = (0..=MAX_ADMITTED_TOOL_CALLS_PER_TURN)
                .map(|index| {
                    Ok(ProviderEvent::ToolCallStarted {
                        id: format!("call-{index}"),
                        name: "read_file".to_owned(),
                    })
                })
                .collect::<Vec<Result<_, ProviderError>>>();
            Box::pin(stream::iter(events))
        }
    }

    let runtime = Runtime::new(TooManyInOneTurn, "gpt-test", 256).unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::ProviderProtocol,
            ..
        })
    ));

    struct EmptyCheckpoint {
        turn: Mutex<usize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for EmptyCheckpoint {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            if last_user_text(&request) == Some(SLICE_CONTINUATION_NOTICE) {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "finished after the missed report".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            if is_checkpoint_request(&request) {
                return Box::pin(stream::iter([Ok(ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 3,
                        cache_read_input_tokens: 1,
                        cache_write_input_tokens: 2,
                        output_tokens: 5,
                        reasoning_tokens: None,
                    }),
                })]));
            }

            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            let mut events = Vec::with_capacity(MAX_TOOL_CALLS_PER_TURN * 3 + 1);
            for index in 0..MAX_TOOL_CALLS_PER_TURN {
                let id = format!("empty-checkpoint-{current}-{index}");
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "unknown".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: "{}".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        EmptyCheckpoint {
            turn: Mutex::new(0),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::Usage {
            usage: TokenUsage {
                input_tokens: 3,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 2,
                output_tokens: 5,
                ..
            }
        }
    )));
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{:?}",
        events.last()
    );
    let requests = requests.lock().unwrap();
    let continuation = requests.last().unwrap();
    let messages = continuation.messages();
    assert_eq!(
        messages[messages.len() - 3..],
        [
            Message::user(SLICE_CHECKPOINT_NOTICE),
            Message::assistant(EMPTY_TURN_PLACEHOLDER),
            Message::user(SLICE_CONTINUATION_NOTICE),
        ]
    );
}

#[tokio::test]
async fn a_budget_final_turn_after_a_report_gets_no_continuation_notice() {
    struct ReportThenFinal {
        calls: Mutex<usize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for ReportThenFinal {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            if is_checkpoint_request(&request)
                || request.tool_choice() == qq_provider::ToolChoice::None
            {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "report".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let mut calls = self.calls.lock().unwrap();
            let first = *calls;
            *calls += MAX_TOOL_CALLS_PER_TURN;
            let mut events = Vec::new();
            for index in first..first + MAX_TOOL_CALLS_PER_TURN {
                let id = format!("call-{index}");
                let (name, json) = if index == first {
                    ("write_file", r#"{"path":"progress.txt","content":"x"}"#)
                } else {
                    ("read_file", r#"{"path":"note.txt"}"#)
                };
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: json.to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let report_turn =
        u32::try_from(MAX_TOOL_CALLS_PER_SLICE / MAX_TOOL_CALLS_PER_TURN).unwrap() + 1;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        ReportThenFinal {
            calls: Mutex::new(0),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("long task")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Auto)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_limits(
                RunLimits {
                    max_model_turns: Some(report_turn + 1),
                    ..RunLimits::default()
                },
                None,
            ),
        )
        .collect::<Vec<_>>()
        .await;
    let requests = requests.lock().unwrap();
    let report_at = requests.iter().position(is_checkpoint_request).unwrap();
    let final_request = &requests[report_at + 1];
    assert_eq!(
        final_request.tool_choice(),
        qq_provider::ToolChoice::None,
        "the final response asks for no tool calls"
    );
    assert!(
        !final_request.tools().is_empty(),
        "the final response keeps its tools declared"
    );
    assert_eq!(
        final_request.messages().last(),
        Some(&Message::user(BUDGET_FINAL_RESPONSE_NOTICE))
    );
    assert_eq!(final_request.system(), requests[0].system());
    assert!(
        !final_request
            .messages()
            .contains(&Message::user(SLICE_CONTINUATION_NOTICE)),
        "a tool-free turn is never told tools are available again"
    );
    let notices = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::AssistantTurnCompleted { notice, .. } => Some(*notice),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices.last(),
        Some(&None),
        "the final turn records no continuation: {notices:?}"
    );
    assert!(notices.contains(&Some(runtime::TurnNotice::Report)));
}

#[tokio::test(start_paused = true)]
async fn an_unmetered_empty_checkpoint_is_retried_as_a_transient_fault() {
    struct SwallowedCheckpoint {
        calls: Mutex<usize>,
        checkpoint_attempts: Mutex<usize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for SwallowedCheckpoint {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            if is_checkpoint_request(&request) {
                let mut attempts = self.checkpoint_attempts.lock().unwrap();
                *attempts += 1;
                if *attempts == 1 {
                    return Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]));
                }
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "report after retry".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            if last_user_text(&request) == Some(SLICE_CONTINUATION_NOTICE) {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let mut calls = self.calls.lock().unwrap();
            let first = *calls;
            *calls += MAX_TOOL_CALLS_PER_TURN;
            let mut events = Vec::new();
            for index in first..first + MAX_TOOL_CALLS_PER_TURN {
                let id = format!("call-{index}");
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "unknown".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: "{}".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        SwallowedCheckpoint {
            calls: Mutex::new(0),
            checkpoint_attempts: Mutex::new(0),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{:?}",
        events.last()
    );
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.iter().filter(|r| is_checkpoint_request(r)).count(),
        2
    );
    let last = requests.last().unwrap().messages();
    assert_eq!(
        last.iter()
            .filter(|message| **message == Message::user(SLICE_CHECKPOINT_NOTICE))
            .count(),
        1
    );
    assert!(last.contains(&Message::assistant("report after retry")));
}
