use super::*;

#[tokio::test]
async fn executes_read_tools_and_returns_results_in_request_order() {
    struct ToolLoopProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for ToolLoopProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "read".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "read".to_owned(),
                        json: r#"{"path":"note.txt"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "read".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "list".to_owned(),
                        name: "tree".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "list".to_owned(),
                        json: r#"{"path":".","depth":1}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "list".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "contents\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        ToolLoopProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { calls, .. } => Some(calls.len()),
                _ => None,
            })
            .sum::<usize>(),
        2
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(tool_names(&requests[0]), ["read_file", "tree", "search"]);
    let system = requests[0]
        .system()
        .expect("agent runs set a system prompt");
    assert!(!system.contains("edit_file"));
    assert!(!system.contains(directory.path().to_str().unwrap()));
    let result_message = &requests[1].messages()[2];
    assert!(matches!(
        result_message.content(),
        [
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error: false,
            },
            ContentBlock::ToolResult {
                call_id: second_id,
                content: second_content,
                is_error: false,
            }
        ] if call_id == "read"
            && content.starts_with("read note.txt L1/1 h:")
            && content.ends_with("\n1\tcontents\n")
            && second_id == "list"
            && second_content == "tree . depth=1 entries=1/1 files=1 dirs=0\nnote.txt 9\n"
    ));
}

#[tokio::test]
async fn read_tools_overlap_and_cancellation_stops_in_flight_work() {
    struct ConcurrentProvider {
        turn: Mutex<usize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for ConcurrentProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "slow".to_owned(),
                        name: "__test_delay".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "slow".to_owned(),
                        json: r#"{"delay_ms":50,"result":"slow","synchronize":true}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "slow".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "fast".to_owned(),
                        name: "__test_delay".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "fast".to_owned(),
                        json: r#"{"delay_ms":1,"result":"fast","synchronize":true}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "fast".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(
        ConcurrentProvider {
            turn: Mutex::new(0),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    let requested = events
        .iter()
        .flat_map(|event| match event {
            RuntimeEvent::AssistantTurnCompleted { calls, .. } => {
                calls.iter().map(|call| call.id).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let finished = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallFinished { id, .. } => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished, [requested[1], requested[0]]);
    assert!(matches!(
        requests.lock().unwrap()[1].messages()[2].content(),
        [
            ContentBlock::ToolResult { content, .. },
            ContentBlock::ToolResult {
                content: second_content,
                ..
            }
        ] if content == "slow" && second_content == "fast"
    ));

    let workspace = workspace::Workspace::open(directory.path()).unwrap();
    let cancelled = RunCancellation::new();
    let started = tools::test_executions_started();
    let execution = tokio::spawn(tools::execute(
        workspace,
        Arc::new(workspace::FileState::default()),
        "__test_delay".to_owned(),
        r#"{"delay_ms":500,"result":"late"}"#.to_owned(),
        cancelled.clone(),
        None,
        tools::ToolTasks::default(),
        Arc::new(runtime::ShellPolicy::default()),
        Arc::default(),
    ));
    while tools::test_executions_started() == started {
        tokio::task::yield_now().await;
    }
    cancelled.cancel();
    let result = execution.await.unwrap();
    assert!(result.is_error);
    assert!(result.model_text.contains("cancelled"));
}

#[tokio::test]
async fn ab3_parallel_default_reads_recover_every_line_under_the_turn_budget() {
    struct State {
        turn: usize,
        seen: Vec<Vec<bool>>,
        results: std::collections::HashSet<String>,
        cuts: usize,
        calls: usize,
    }
    struct BatchReader(Arc<Mutex<State>>);
    impl Provider for BatchReader {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut state = self.0.lock().unwrap();
            for block in request
                .messages()
                .iter()
                .flat_map(|message| message.content())
            {
                if let ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } = block
                {
                    if !state.results.insert(call_id.clone()) {
                        continue;
                    }
                    assert!(!is_error, "{content}");
                    state.cuts += usize::from(content.contains("turn budget reached"));
                    let file = content
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let index: usize = file.strip_prefix("batch-").unwrap().parse().unwrap();
                    for row in content.lines().skip(1) {
                        if let Some((number, _)) = row.split_once('\t')
                            && let Ok(number) = number.trim().parse::<usize>()
                        {
                            assert!((1..=1_000).contains(&number));
                            state.seen[index][number - 1] = true;
                        }
                    }
                }
            }
            if state.seen.iter().all(|file| file.iter().all(|seen| *seen)) {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "all lines read".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            assert!(
                state.turn < 20,
                "bounded fixture failed to recover every line"
            );
            let turn = state.turn;
            state.turn += 1;
            let mut events = Vec::new();
            for index in 0..6 {
                if let Some(offset) = state.seen[index].iter().position(|seen| !seen) {
                    state.calls += 1;
                    let id = format!("batch-{turn}-{index}");
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "read_file".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: format!(r#"{{"path":"batch-{index}","offset":{}}}"#, offset + 1),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
                }
            }
            events.push(Ok(ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    for index in 0..6 {
        std::fs::write(
            directory.path().join(format!("batch-{index}")),
            "012345678901234567890123456789\n".repeat(1_000),
        )
        .unwrap();
    }
    let state = Arc::new(Mutex::new(State {
        turn: 0,
        seen: vec![vec![false; 1_000]; 6],
        results: Default::default(),
        cuts: 0,
        calls: 0,
    }));
    let runtime = Runtime::new(BatchReader(Arc::clone(&state)), "gpt-test", 256).unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("read all six files")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let state = state.lock().unwrap();
    assert!(state.seen.iter().all(|file| file.iter().all(|seen| *seen)));
    assert!(state.calls <= 30);
    assert!(
        state.cuts == 0 || state.calls < 30,
        "more cuts must buy fewer calls"
    );
    eprintln!(
        "AB3 parallel runtime: calls={} turn_budget_cuts={}",
        state.calls, state.cuts
    );
}

#[tokio::test]
async fn ab2_deferred_searches_do_not_trigger_a_premature_slice_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("AGENTS.md"), "instructions").unwrap();
    let turns = (0..20).map(|turn| (0..16).map(|index| {
        ("search", serde_json::json!({"query":format!("AGENTS[.]md|^__page_probe_{turn}_{index}$"),"mode":"names","regex":true}).to_string())
    }).collect()).collect();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns,
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("inspect")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Full)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).stall_exempt(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let requests = requests.lock().unwrap();
    assert!(
        !requests.iter().any(|request| request
            .messages()
            .iter()
            .any(|message| { *message == Message::user(SLICE_CHECKPOINT_NOTICE) })),
        "140 executed searches must not trigger the 256-execution slice boundary"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                RuntimeEvent::ToolCallFinished {
                    is_error: false,
                    ..
                }
            ))
            .count(),
        140
    );
}

#[tokio::test]
async fn ab2_batched_searches_defer_before_execution_and_retry_unchanged() {
    for count in [16, 64] {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..100 {
            std::fs::write(
                directory
                    .path()
                    .join(format!("{index:03}{}", "x".repeat(220))),
                "",
            )
            .unwrap();
        }
        let admitted = (tools::output::MAX_TURN_TOOL_OUTPUT_BYTES
            - count * tools::output::result_reserve_bytes("search"))
            / tools::output::SEARCH_RESERVATION_BYTES;
        let arguments = (0..count)
            .map(|index| {
                let query = if index < admitted { index } else { 0 };
                serde_json::json!({"query":format!("x|^__page_probe_{query}$"),"mode":"names","regex":true})
                    .to_string()
            })
            .collect::<Vec<_>>();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![
                    arguments
                        .iter()
                        .cloned()
                        .map(|arguments| ("search", arguments))
                        .collect(),
                    vec![("search", arguments[0].clone())],
                ],
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let requested = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { calls, .. } if !calls.is_empty() => {
                    Some(calls)
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(requested[0].len(), count);
        for (index, call) in requested[0].iter().enumerate() {
            assert_eq!(call.arguments, arguments[index]);
            if index >= MAX_TOOL_CALLS_PER_TURN {
                assert!(matches!(
                    call.rejection,
                    Some(runtime::CallRejection::Runtime(_))
                ));
            } else {
                assert_eq!(
                    call.rejection,
                    (index >= admitted).then_some(runtime::CallRejection::OutputBudget)
                );
            }
        }
        assert!(requested[1][0].rejection.is_none());
        assert_eq!(requested[1][0].arguments, requested[0][admitted].arguments);
        let completed = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished {
                    result, is_error, ..
                } => Some((result.as_str(), *is_error)),
                RuntimeEvent::ToolCallUnexecuted { message, .. } => Some((message.as_str(), true)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), count + 1);
        let pages = completed
            .iter()
            .filter(|(_, error)| !error)
            .map(|(text, _)| *text)
            .collect::<Vec<_>>();
        assert_eq!(pages.len(), admitted + 1);
        let by_id = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished {
                    id,
                    result,
                    is_error: false,
                    ..
                } => Some((*id, result.as_str())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let by_provider = requested
            .iter()
            .flat_map(|calls| calls.iter())
            .filter_map(|call| {
                by_id
                    .get(&call.id)
                    .map(|page| (call.provider_call_id.as_str(), *page))
            })
            .collect::<HashMap<_, _>>();
        let first_page = by_id[&requested[0][0].id];
        let retry_page = by_id[&requested[1][0].id];
        assert!(first_page.len() > 10 * 1024);
        assert!(first_page.lines().next().unwrap().contains("next="));
        assert_eq!(
            retry_page, first_page,
            "unchanged retry preserves the complete page and cursor"
        );
        assert!(
            pages
                .iter()
                .all(|page| page.lines().skip(1).eq(first_page.lines().skip(1)))
        );
        assert!(
            completed
                .iter()
                .filter(|(_, error)| *error)
                .all(|(text, _)| text.starts_with("turn_output_budget:")
                    || text.contains("not executed"))
        );
        let requests = requests.lock().unwrap();
        for request in requests.iter().skip(1) {
            for message in request.messages() {
                let total = message
                    .content()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { content, .. } => Some(content.len()),
                        _ => None,
                    })
                    .sum::<usize>();
                assert!(
                    total <= tools::output::MAX_TURN_TOOL_OUTPUT_BYTES,
                    "{total}"
                );
                for block in message.content() {
                    if let ContentBlock::ToolResult {
                        content,
                        is_error: false,
                        call_id,
                    } = block
                    {
                        assert_eq!(
                            content,
                            by_provider[call_id.as_str()],
                            "admitted page must remain whole"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn ab2_late_direct_search_preserves_complete_pages_and_continuation() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::iter::repeat_n("a".repeat(240), 8)
        .collect::<Vec<_>>()
        .join("/");
    std::fs::create_dir_all(directory.path().join(&root)).unwrap();
    for index in 0..10 {
        std::fs::write(
            directory
                .path()
                .join(&root)
                .join(format!("{index:02}{}", "x".repeat(200))),
            "",
        )
        .unwrap();
    }
    std::fs::write(
        directory.path().join("big.txt"),
        format!("{}\n", "b".repeat(1_900)).repeat(17),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![vec![
                ("read_file", r#"{"path":"big.txt"}"#.to_owned()),
                ("read_file", r#"{"path":"big.txt"}"#.to_owned()),
                ("read_file", r#"{"path":"big.txt"}"#.to_owned()),
                (
                    "search",
                    r#"{"query":"x{10}","regex":true,"mode":"names"}"#.to_owned(),
                ),
            ]],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let recorded = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::ToolCallFinished { result, .. } if result.starts_with("search ") => {
                Some(result.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(recorded.len(), 1);
    let mut page = {
        let captured = requests.lock().unwrap();
        let results = captured[1].messages()[2].content();
        let ContentBlock::ToolResult { content, .. } = &results[3] else {
            panic!("search result");
        };
        assert_eq!(
            content, &recorded[0],
            "live projection must retain every page row and its cursor"
        );
        assert!(!content.contains("turn budget reached"));
        assert!(content.len() > 4 * 1024);
        content.clone()
    };
    let workspace = workspace::Workspace::open(directory.path()).unwrap();
    let mut found = std::collections::BTreeSet::new();
    for _ in 0..10 {
        for path in page.lines().skip(1) {
            assert!(found.insert(path.to_owned()), "duplicate match");
        }
        let next = page
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .find_map(|f| f.strip_prefix("next="));
        let Some(next) = next else {
            break;
        };
        let result = tools::execute(
            workspace.clone(),
            Arc::new(workspace::FileState::default()),
            "search".to_owned(),
            serde_json::json!({"query":"x{10}","regex":true,"mode":"names","cursor":next})
                .to_string(),
            RunCancellation::new(),
            None,
            tools::ToolTasks::default(),
            Arc::new(runtime::ShellPolicy::default()),
            Arc::default(),
        )
        .await;
        assert!(!result.is_error, "{}", result.model_text);
        page = result.model_text;
    }
    assert_eq!(found.len(), 10);
}

#[tokio::test]
async fn a_turns_tool_output_is_capped_and_persisted_results_stay_whole() {
    struct ThreeReadsProvider {
        turn: Mutex<usize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for ThreeReadsProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current != 0 {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let mut events = Vec::new();
            for index in 0..4 {
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

    let directory = tempfile::tempdir().unwrap();
    let line = format!("{}\n", "z".repeat(63));
    for index in 0..4 {
        std::fs::write(
            directory.path().join(format!("big-{index}.txt")),
            line.repeat(420),
        )
        .unwrap();
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        ThreeReadsProvider {
            turn: Mutex::new(0),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("read")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));

    let persisted = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallFinished { result, .. } => Some(result.len()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted.len(), 4);
    for length in &persisted {
        assert!((27 * 1024..=29 * 1024).contains(length), "{length}");
    }

    let requests = requests.lock().unwrap();
    let in_context = requests[1].messages()[2]
        .content()
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult { content, .. } => content.as_str(),
            other => panic!("unexpected block {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(in_context[0].len(), persisted[0]);
    assert_eq!(in_context[1].len(), persisted[1]);
    assert_eq!(in_context[2].len(), persisted[2]);
    assert!(in_context[3].len() <= 16 * 1024, "{}", in_context[3].len());
    assert!(
        in_context[3].contains("turn budget reached"),
        "{}",
        in_context[3]
    );
    assert!(
        in_context[3].starts_with("read big-3.txt L1-420/420 h:"),
        "the header survives"
    );
    assert!(
        in_context[3].ends_with(&format!("420\t{line}")),
        "the tail survives"
    );
    let total: usize = in_context.iter().map(|content| content.len()).sum();
    assert!(
        total <= tools::output::MAX_TURN_TOOL_OUTPUT_BYTES,
        "{total}"
    );
}

#[tokio::test]
async fn mutating_calls_execute_sequentially_in_request_order() {
    struct MutatingTurnProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for MutatingTurnProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "slow".to_owned(),
                        name: "__test_mutate".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "slow".to_owned(),
                        json: r#"{"delay_ms":50,"result":"slow"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "slow".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "fast".to_owned(),
                        name: "__test_mutate".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "fast".to_owned(),
                        json: r#"{"delay_ms":1,"result":"fast"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "fast".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        MutatingTurnProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("mutate twice")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Auto)),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    let requested = events
        .iter()
        .flat_map(|event| match event {
            RuntimeEvent::AssistantTurnCompleted { calls, .. } => {
                calls.iter().map(|call| call.id).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let finished = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallFinished { id, result, .. } => Some((*id, result.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finished.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        requested
    );
    assert_eq!(finished[0].1, "slow");
    assert_eq!(finished[1].1, "fast");
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_calls_stream_output_deltas_before_their_result() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct ShellProvider {
        turn: Mutex<usize>,
    }

    impl Provider for ShellProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "run".to_owned(),
                        name: "shell".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "run".to_owned(),
                        json: r#"{"command":"echo streamed-hello"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "run".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(
        ShellProvider {
            turn: Mutex::new(0),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("run the command")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    let started = events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        .unwrap();
    let delta = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("streamed-hello")
            )
        })
        .expect("shell output must stream as deltas");
    let finished = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEvent::ToolCallFinished { result, is_error: false, .. }
                    if result.contains("streamed-hello") && result.starts_with("shell exit=0 ")
            )
        })
        .expect("the bounded result must follow the streamed output");
    assert!(started < delta && delta < finished);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[tokio::test]
async fn invalid_tool_argument_json_yields_a_tool_error_and_continues_the_run() {
    struct MalformedArgumentsProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for MalformedArgumentsProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "bad".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "bad".to_owned(),
                        json: r#"{"path": "#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "bad".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        MalformedArgumentsProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallUnexecuted { interrupted: false, message: result, .. }
            if result.contains("not valid JSON")
    )));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult {
            call_id,
            content,
            is_error: true,
        }] if call_id == "bad" && content.contains("not valid JSON")
    ));
}

#[tokio::test]
async fn oversized_tool_arguments_yield_a_tool_error_and_continue_the_run() {
    struct OversizedArgumentsProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for OversizedArgumentsProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                let content = "x".repeat(35 * 1024);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "big".to_owned(),
                        name: "write_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "big".to_owned(),
                        json: format!(r#"{{"path":"big.txt","content":"{content}"#),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "big".to_owned(),
                        json: format!(r#"{content}"}}"#),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "big".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        OversizedArgumentsProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_messages_in_workspace(vec![Message::user("write")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{:?}",
        events.last()
    );
    assert!(!directory.path().join("big.txt").exists());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult {
            call_id,
            content,
            is_error: true,
        }] if call_id == "big" && content.contains("64 KiB limit")
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn a_run_follows_a_yielded_command_and_stops_the_rest_when_it_ends() {
    struct ReleaseOnSecondTurn {
        script: TurnScript,
        gate: std::fs::File,
    }

    impl Provider for ReleaseOnSecondTurn {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            if self.script.requests.lock().unwrap().len() == 1 {
                use std::io::Write as _;
                (&self.gate).write_all(b"go\n").unwrap();
                let id = request
                    .messages()
                    .iter()
                    .flat_map(|message| message.content())
                    .find_map(|block| match block {
                        ContentBlock::ToolResult { content, .. } => content
                            .split("id=")
                            .nth(1)
                            .and_then(|rest| rest.split_whitespace().next())
                            .map(str::to_owned),
                        _ => None,
                    })
                    .expect("the first result names its command id");
                self.script.requests.lock().unwrap().push(request);
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "status".to_owned(),
                        name: "command_status".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "status".to_owned(),
                        json: serde_json::json!({"id": id, "wait_seconds": 30}).to_string(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "status".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            self.script.stream(request)
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    tools::create_test_fifo(&release).await;
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&release)
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let tool_tasks = tools::ToolTasks::default();
    let script = TurnScript {
            turns: vec![
                vec![(
                    "shell",
                    r#"{"command":"echo building; read token < release; echo built","yield_after_seconds":1}"#
                        .to_owned(),
                )],
                Vec::new(),
                vec![(
                    "shell",
                    r#"{"command":"echo pid:$$; exec tail -f /dev/null","yield_after_seconds":1}"#
                        .to_owned(),
                )],
            ],
            requests: Arc::clone(&requests),
        };
    let runtime = Runtime::new(ReleaseOnSecondTurn { script, gate }, "test-model", 256).unwrap();
    let events = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut stream = runtime.run_loop_with_spawner(
            vec![Message::user("build")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Full)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_tool_tasks(tool_tasks.clone()),
        );
        let mut events = Vec::new();
        let mut printed = std::collections::HashSet::new();
        while let Some(event) = stream.next().await {
            if let RuntimeEvent::ToolCallOutputDelta { id, .. } = &event
                && printed.insert(*id)
            {
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(1)).await;
                tokio::time::resume();
            }
            events.push(event);
        }
        events
    })
    .await
    .expect("a yielded command must not hold the run");
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tool_tasks.commands().all_finished(),
    )
    .await
    .expect("no yielded command outlives its run");
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let results = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallFinished { result, .. } => Some(result.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 3, "{results:?}");
    assert!(
        results[0].starts_with("shell exit=running id=c"),
        "{}",
        results[0]
    );
    assert!(results[1].starts_with("shell exit=0 "), "{}", results[1]);
    assert!(results[1].contains("built"), "{}", results[1]);
    assert!(
        results[2].starts_with("shell exit=running id=c"),
        "{}",
        results[2]
    );
    let pid = results[2]
        .split("pid:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("the command printed its pid");
    assert!(
        rustix::process::test_kill_process(pid).is_err(),
        "the run's end stops its yielded commands"
    );
}

#[tokio::test]
async fn reports_the_underlying_workspace_open_error() {
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::new(Mutex::new(None)),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(
            RunCommand::new("hello"),
            PathBuf::from("/qq-test-missing-workspace"),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.as_slice(),
        [
            RunEvent::Started,
            RunEvent::Failed {
                kind: RunFailureKind::InvalidCommand,
                message,
            }
        ] if message.contains("could not open the workspace directory")
            && message.len() > "could not open the workspace directory: ".len()
    ));
}

#[tokio::test]
async fn gate_less_runs_deny_mutating_tools_and_return_the_error_to_the_model() {
    struct MutatingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for MutatingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "__test_mutate".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: "{}".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        MutatingProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("mutate")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallDenied { message, .. }
            if message == approval::UNATTENDED_DENIED_RESULT
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult {
            content,
            is_error: true,
            ..
        }] if content == approval::UNATTENDED_DENIED_RESULT
    ));
}

#[tokio::test]
async fn calls_with_malformed_arguments_short_circuit_without_consulting_the_gate() {
    struct RecordingGate {
        consulted: Arc<AtomicBool>,
    }

    impl ToolGate for RecordingGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            self.consulted.store(true, Ordering::Release);
            Box::pin(std::future::ready(GateDecision::Deny {
                message: "the gate must not see unexecutable calls".to_owned(),
            }))
        }
    }

    struct MalformedMutatingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for MalformedMutatingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "bad".to_owned(),
                        name: "__test_mutate".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "bad".to_owned(),
                        json: r#"{"broken": "#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "bad".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let consulted = Arc::new(AtomicBool::new(false));
    let runtime = Runtime::new(
        MalformedMutatingProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("mutate")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(RecordingGate {
                consulted: Arc::clone(&consulted),
            }),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(!consulted.load(Ordering::Acquire));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallUnexecuted { interrupted: false, message: result, .. }
            if result.contains("not valid JSON")
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[tokio::test]
async fn fails_when_provider_completes_with_an_unfinished_tool_call() {
    struct ToolCallingProvider;

    impl Provider for ToolCallingProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let runtime = Runtime::new(ToolCallingProvider, "gpt-test", 256).unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events[0], RunEvent::Started);
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ActivityChanged {
            activity: RunActivity::PreparingToolCall
        }
    )));
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::ProviderProtocol,
            ..
        })
    ));
}
