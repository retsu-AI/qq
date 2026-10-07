use super::*;

#[test]
fn assembly_pruning_stubs_old_read_only_results_and_preserves_errors() {
    let call = |id: &str, name: &str| {
        ContentBlock::tool_call(
            id.to_owned(),
            name.to_owned(),
            &serde_json::json!({"path": "src/lib.rs"}),
        )
    };
    let result = |id: &str, is_error: bool| ContentBlock::ToolResult {
        call_id: id.to_owned(),
        content: "y".repeat(500),
        is_error,
    };
    let mut context = vec![
        Message::user("start"),
        Message::new(Role::Assistant, vec![call("c1", "read_file")]),
        Message::tool_results(vec![result("c1", false)]),
        Message::new(Role::Assistant, vec![call("c2", "shell")]),
        Message::tool_results(vec![result("c2", false)]),
        Message::new(Role::Assistant, vec![call("c3", "read_file")]),
        Message::tool_results(vec![result("c3", true)]),
        // The recency window: the last four model turns stay verbatim.
        Message::new(Role::Assistant, vec![call("c4", "read_file")]),
        Message::tool_results(vec![result("c4", false)]),
        Message::assistant("a"),
        Message::assistant("b"),
        Message::assistant("c"),
    ];
    let before = context_bytes(&context);

    assert!(prune_stale_tool_results(&mut context, &HashMap::new()));

    let results = context
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Some((call_id.as_str(), content.as_str(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results[0],
        (
            "c1",
            "[pruned: read_file {\"path\":\"src/lib.rs\"} returned 500 bytes; \
             the text is no longer in context: read_file the lines you need again, \
             without if_changed_since]",
            false
        )
    );
    // Shell output is not re-derivable; it survives outside the window.
    assert_eq!(results[1].0, "c2");
    assert!(results[1].1.starts_with("yyy"));
    // Errors prune to error stubs: content stubbed, is_error preserved.
    assert!(results[2].1.starts_with("[pruned: read_file"));
    assert!(results[2].2, "the error flag must survive pruning");
    // Inside the window everything stays verbatim.
    assert!(results[3].1.starts_with("yyy"));
    assert!(context_bytes(&context) < before);
}

#[test]
fn pruning_stubs_keep_a_result_header_line() {
    let call = |id: &str, name: &str| {
        ContentBlock::tool_call(
            id.to_owned(),
            name.to_owned(),
            &serde_json::json!({"query": "needle"}),
        )
    };
    let with_header = format!(
        "search \"needle\" matches=4/4 files=3 scanned=612\n{}",
        "match\n".repeat(200)
    );
    let mut context = vec![
        Message::user("start"),
        Message::new(Role::Assistant, vec![call("c1", "search")]),
        Message::tool_results(vec![ContentBlock::ToolResult {
            call_id: "c1".to_owned(),
            content: with_header.clone(),
            is_error: false,
        }]),
        Message::new(Role::Assistant, vec![call("c2", "search")]),
        Message::tool_results(vec![ContentBlock::ToolResult {
            call_id: "c2".to_owned(),
            content: format!("searching...\n{}", "match\n".repeat(200)),
            is_error: false,
        }]),
        Message::assistant("a"),
        Message::assistant("b"),
        Message::assistant("c"),
        Message::assistant("d"),
    ];
    assert!(prune_stale_tool_results(&mut context, &HashMap::new()));
    let stubs = context
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stubs[0],
        format!(
            "search \"needle\" matches=4/4 files=3 scanned=612\n\
             [pruned: search {{\"query\":\"needle\"}} returned {} bytes; call it again if needed]",
            with_header.len()
        )
    );
    // A first line that does not follow `<tool> …` is not a header.
    assert!(stubs[1].starts_with("[pruned: search"), "{}", stubs[1]);
}

#[test]
fn a_stub_is_never_stubbed_again_but_a_result_that_merely_ends_like_one_is() {
    // CX3: a live run prunes again on every overflowing turn. Its own stubs
    // must stay as they are, or the stub names the previous stub's size and
    // disagrees with replay. A real result is judged by what it is: a large
    // one whose last line happens to read like a stub still prunes.
    let call = |id: &str, name: &str| {
        ContentBlock::tool_call(
            id.to_owned(),
            name.to_owned(),
            &serde_json::json!({"query": "needle"}),
        )
    };
    let result = |id: &str, content: String| {
        Message::tool_results(vec![ContentBlock::ToolResult {
            call_id: id.to_owned(),
            content,
            is_error: false,
        }])
    };
    let with_header = format!(
        "search \"needle\" matches=4/4 files=3 scanned=612\n{}",
        "match\n".repeat(200)
    );
    let lookalike = format!(
        "{}[pruned: search {{\"query\":\"needle\"}} returned 12 bytes; call it again if needed]",
        "match\n".repeat(200)
    );
    // Two lines, the second a valid stub line, the first far past a
    // header's bound: not a stub.
    let long_first_line = format!(
        "search {}\n[pruned: search {{\"query\":\"needle\"}} returned 1 bytes; call it again if needed]",
        "m".repeat(4 * 1024)
    );
    // A stub line whose size is not a `u64`: not a stub.
    let huge_size = format!(
        "[pruned: search {{\"query\":\"needle\"}} returned {} bytes; call it again if needed]",
        "9".repeat(2 * 1024)
    );
    let mut context = vec![
        Message::user("start"),
        Message::new(Role::Assistant, vec![call("c1", "search")]),
        result("c1", with_header.clone()),
        Message::new(Role::Assistant, vec![call("c2", "search")]),
        result("c2", "match\n".repeat(200)),
        Message::new(Role::Assistant, vec![call("c3", "search")]),
        result("c3", lookalike.clone()),
        Message::new(Role::Assistant, vec![call("c4", "search")]),
        result("c4", long_first_line.clone()),
        Message::new(Role::Assistant, vec![call("c5", "search")]),
        result("c5", huge_size.clone()),
        Message::assistant("a"),
        Message::assistant("b"),
        Message::assistant("c"),
        Message::assistant("d"),
    ];
    assert!(prune_stale_tool_results(&mut context, &HashMap::new()));
    let results = |context: &[Message]| {
        context
            .iter()
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first = results(&context);
    assert_eq!(
        first[0],
        format!(
            "search \"needle\" matches=4/4 files=3 scanned=612\n\
             [pruned: search {{\"query\":\"needle\"}} returned {} bytes; call it again if needed]",
            with_header.len()
        )
    );
    assert_eq!(
        first[1],
        "[pruned: search {\"query\":\"needle\"} returned 1200 bytes; call it again if needed]"
    );
    for (index, original) in [(2, &lookalike), (4, &huge_size)] {
        assert_eq!(
            first[index],
            format!(
                "[pruned: search {{\"query\":\"needle\"}} returned {} bytes; call it again if needed]",
                original.len()
            ),
            "result {index}"
        );
    }
    // The long first line is not a header, so the stub carries none.
    assert_eq!(
        first[3],
        format!(
            "[pruned: search {{\"query\":\"needle\"}} returned {} bytes; call it again if needed]",
            long_first_line.len()
        )
    );
    // A second pass, as a later overflowing turn makes, changes nothing.
    assert!(!prune_stale_tool_results(&mut context, &HashMap::new()));
    assert_eq!(results(&context), first);
}

#[test]
fn pruned_read_file_stubs_keep_the_window_drop_the_hash_and_name_the_reread() {
    // `read_file`'s header names the operation (`read`), not the tool. The
    // stub keeps the window and line count so the model can re-read only
    // what it needs, and drops `h:` so it cannot pass that hash back through
    // `if_changed_since`, which would return no body for text that is no
    // longer in context.
    let call = |id: &str, arguments: serde_json::Value| {
        ContentBlock::tool_call(id.to_owned(), "read_file".to_owned(), &arguments)
    };
    let body = "fn line() {}\n".repeat(60);
    let windowed = format!("read src/lib.rs L1-60/240 h:3f9a0c1d2e4b clipped=2\n{body}");
    let unscanned = format!("read big.log L1-60/- h:-\n{body}");
    // A path segment can look like a hash; only the trailing hash goes.
    let odd_path = format!("read docs/a h:0123456789ab b.md L1-60/60 h:3f9a0c1d2e4b\n{body}");
    let result = |id: &str, content: &str| ContentBlock::ToolResult {
        call_id: id.to_owned(),
        content: content.to_owned(),
        is_error: false,
    };
    let mut context = vec![
        Message::user("start"),
        Message::new(
            Role::Assistant,
            vec![
                call("c1", serde_json::json!({"path": "src/lib.rs", "limit": 60})),
                call("c2", serde_json::json!({"path": "big.log"})),
                call(
                    "c3",
                    serde_json::json!({"path": "docs/a h:0123456789ab b.md"}),
                ),
            ],
        ),
        Message::tool_results(vec![
            result("c1", &windowed),
            result("c2", &unscanned),
            result("c3", &odd_path),
        ]),
        Message::assistant("a"),
        Message::assistant("b"),
        Message::assistant("c"),
        Message::assistant("d"),
    ];

    assert!(prune_stale_tool_results(&mut context, &HashMap::new()));

    let stubs = context
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stubs[0],
        format!(
            "read src/lib.rs L1-60/240 clipped=2\n\
             [pruned: read_file {{\"limit\":60,\"path\":\"src/lib.rs\"}} returned {} bytes; \
             the text is no longer in context: read_file the lines you need again, \
             without if_changed_since]",
            windowed.len()
        )
    );
    assert_eq!(
        stubs[1],
        format!(
            "read big.log L1-60/-\n\
             [pruned: read_file {{\"path\":\"big.log\"}} returned {} bytes; \
             the text is no longer in context: read_file the lines you need again, \
             without if_changed_since]",
            unscanned.len()
        )
    );
    assert!(
        stubs[2].starts_with("read docs/a h:0123456789ab b.md L1-60/60\n"),
        "{}",
        stubs[2]
    );
}

#[test]
fn assembly_pruning_decides_from_the_stored_effect_class_not_the_tool_name() {
    let call = |id: &str, name: &str| {
        ContentBlock::tool_call(
            id.to_owned(),
            name.to_owned(),
            &serde_json::json!({"q": "x"}),
        )
    };
    let result = |id: &str| ContentBlock::ToolResult {
        call_id: id.to_owned(),
        content: "y".repeat(500),
        is_error: false,
    };
    let mut context = vec![
        Message::user("start"),
        // An external read-only tool: prunable only because the store
        // recorded its effect as read_only.
        Message::new(Role::Assistant, vec![call("c1", "mcp__docs__lookup")]),
        Message::tool_results(vec![result("c1")]),
        // A built-in read-only name without a stored effect (a row from
        // before schema 26) still prunes through the name fallback.
        Message::new(Role::Assistant, vec![call("c2", "read_file")]),
        Message::tool_results(vec![result("c2")]),
        // An external tool without a read-only effect is never pruned.
        Message::new(Role::Assistant, vec![call("c3", "mcp__docs__write")]),
        Message::tool_results(vec![result("c3")]),
        // An explicit stored effect takes precedence over legacy names.
        Message::new(Role::Assistant, vec![call("c4", "read_file")]),
        Message::tool_results(vec![result("c4")]),
        Message::assistant("a"),
        Message::assistant("b"),
        Message::assistant("c"),
        Message::assistant("d"),
    ];
    let prunable = HashMap::from([
        ((2, 0), EffectClass::ReadOnly),
        ((8, 0), EffectClass::External),
    ]);
    assert!(prune_stale_tool_results(&mut context, &prunable));
    let results = context
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                call_id, content, ..
            } => Some((call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(results[0].1.starts_with("[pruned: mcp__docs__lookup"));
    assert!(
        results[1].1.starts_with("[pruned: read_file"),
        "legacy name fallback"
    );
    assert!(
        results[2].1.starts_with("yyy"),
        "an external non-read-only result stays"
    );
    assert!(
        results[3].1.starts_with("yyy"),
        "stored effect overrides the legacy name"
    );
}

#[tokio::test]
async fn admitted_tool_calls_store_their_effect_class() {
    let (_directory, store, claimed) = claimed_store_fixture().await;
    let runtime_call =
        |ordinal: u16, provider_id: &str, name: &str, effect: EffectClass| RuntimeToolCall {
            id: ToolCallId::from_bytes([ordinal as u8; 16]),
            turn_ordinal: 1,
            call_ordinal: ordinal,
            provider_call_id: provider_id.to_owned(),
            name: name.to_owned(),
            arguments: "{}".to_owned(),
            effect,
            rejection: None,
        };
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::tool_call(
                            "p1".to_owned(),
                            "read_file".to_owned(),
                            &serde_json::json!({}),
                        ),
                        ContentBlock::tool_call(
                            "p2".to_owned(),
                            "shell".to_owned(),
                            &serde_json::json!({}),
                        ),
                    ],
                ),
                calls: vec![
                    runtime_call(1, "p1", "read_file", EffectClass::ReadOnly),
                    runtime_call(2, "p2", "shell", EffectClass::Shell),
                ],
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
                notice: None,
            },
        )
        .await
        .unwrap();
    let stored = store
        .call(Priority::Control, |connection| {
            connection
                .prepare("SELECT provider_call_id, effect FROM tool_calls ORDER BY call_ordinal")?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
        })
        .await
        .unwrap();
    assert_eq!(
        stored,
        vec![
            ("p1".to_owned(), Some("read_only".to_owned())),
            ("p2".to_owned(), Some("shell".to_owned())),
        ]
    );
}

#[test]
fn capacity_accounting_measures_the_pruned_assembly_not_raw_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (mut connection, store_id) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::from_bytes([1; 16]);
    let session_id = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    connection
        .execute(
            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, model,
                                  created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'S', 'idle', 'test/model', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, kind, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'completed', 'prompt', 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::from_bytes([4; 16]).to_string(),
                MessageId::from_bytes([5; 16]).to_string(),
                MessageId::from_bytes([6; 16]).to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                                  output, created_at_ms)
             VALUES (?1, ?2, ?3, 1, 'user', 'complete', 'hi', 1)",
            params![
                MessageId::from_bytes([5; 16]).to_string(),
                session_id.to_string(),
                run_id.to_string(),
            ],
        )
        .unwrap();
    // Two early read_file turns whose results total ~6 MiB of stored
    // rows — well over the 4 MiB budget — followed by four text turns
    // that push them out of the recency window.
    for (turn, provider_id) in [(1_u8, "c1"), (2, "c2")] {
        connection
            .execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                 VALUES (?1, ?2, ?3)",
                params![
                    run_id.to_string(),
                    turn,
                    format!(
                        "[{{\"type\":\"tool_call\",\"id\":\"{provider_id}\",\
                         \"name\":\"read_file\",\"arguments\":{{\"path\":\"big.txt\"}}}}]"
                    ),
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tool_calls(id, run_id, turn_ordinal, call_ordinal,
                                        provider_call_id, name, arguments_json, state,
                                        result, is_error, requested_at_ms)
                 VALUES (?1, ?2, ?3, 0, ?4, 'read_file', '{\"path\":\"big.txt\"}',
                         'completed', ?5, 0, 1)",
                params![
                    ToolCallId::from_bytes([turn; 16]).to_string(),
                    run_id.to_string(),
                    turn,
                    provider_id,
                    "z".repeat(3 * 1024 * 1024),
                ],
            )
            .unwrap();
    }
    for turn in 3..=6 {
        connection
            .execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                 VALUES (?1, ?2, '[{\"type\":\"text\",\"text\":\"ok\"}]')",
                params![run_id.to_string(), turn],
            )
            .unwrap();
    }

    let transaction = connection.transaction().unwrap();
    let assembled = assembled_context_bytes(&transaction, session_id).unwrap();
    drop(transaction);
    assert!(
        assembled < 64 * 1024,
        "stale results must assemble as stubs, got {assembled} bytes"
    );

    // A prompt fits because the budget measures the pruned assembly, not
    // the ~6 MiB of stored result rows.
    let applied = execute_command(
        &mut connection,
        store_id,
        CommandId::from_bytes([9; 16]),
        SessionCommand::SubmitPrompt {
            session_id,
            input: vec![InputPart::text("continue".to_owned())],
            limits: qq_protocol::RunLimits::default(),
            correlation: Correlation::default(),
            output: None,
        },
        None,
        &WorkspaceGrantSeed::default(),
        CommandOrigin::Client,
    )
    .unwrap();
    assert!(matches!(
        applied.receipt.outcome,
        CommandOutcome::PromptQueued { .. }
    ));
}

#[test]
fn denial_results_reserve_exact_capacity_and_overflow_rolls_back() {
    for path in [
        DenialCapacityPath::Policy,
        DenialCapacityPath::Client,
        DenialCapacityPath::Timeout,
    ] {
        let result_bytes = "provider-call"
            .len()
            .saturating_add(path.result().len())
            .saturating_add(crate::CONTEXT_MESSAGE_FRAMING_BYTES as usize)
            .saturating_add(crate::CONTEXT_BLOCK_FRAMING_BYTES as usize);
        let (_directory, mut connection, store_id, claimed, tool_call_id, _) =
            denial_capacity_fixture(path, MAX_CONTEXT_BYTES - result_bytes);
        apply_denial_capacity_path(&mut connection, store_id, &claimed, tool_call_id, path)
            .unwrap();
        let (increment, state, result, events, commands) =
            denial_capacity_state(&connection, claimed.identity.run_id, tool_call_id);
        assert_eq!(increment, u64::try_from(result_bytes).unwrap());
        assert_eq!(state, "denied");
        assert_eq!(result.as_deref(), Some(path.result()));
        assert_eq!(events, 1);
        assert_eq!(
            commands,
            u64::from(matches!(path, DenialCapacityPath::Client))
        );

        let (_directory, mut connection, store_id, claimed, tool_call_id, _) =
            denial_capacity_fixture(path, MAX_CONTEXT_BYTES - result_bytes + 1);
        assert_eq!(
            apply_denial_capacity_path(&mut connection, store_id, &claimed, tool_call_id, path,)
                .unwrap_err(),
            SessionRuntimeError::OutputTooLarge
        );
        assert_eq!(
            denial_capacity_state(&connection, claimed.identity.run_id, tool_call_id),
            (0, path.initial_state().to_owned(), None, 0, 0)
        );
    }
}

#[test]
fn incremental_capacity_accepts_the_exact_limit_and_rolls_back_overflow() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (mut connection, _) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::from_bytes([1; 16]);
    let session_id = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    connection
        .execute(
            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'S', 'running', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, context_base_bytes,
                              context_increment_bytes, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, 0, 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::from_bytes([4; 16]).to_string(),
                MessageId::from_bytes([5; 16]).to_string(),
                MessageId::from_bytes([6; 16]).to_string(),
                MAX_CONTEXT_BYTES - 3,
            ],
        )
        .unwrap();

    let transaction = connection.transaction().unwrap();
    reserve_context_capacity(&transaction, run_id, 3).unwrap();
    transaction.commit().unwrap();

    let transaction = connection.transaction().unwrap();
    assert_eq!(
        reserve_context_capacity(&transaction, run_id, 1).unwrap_err(),
        SessionRuntimeError::OutputTooLarge
    );
    transaction.commit().unwrap();

    assert_eq!(
        connection
            .query_row(
                "SELECT context_increment_bytes FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        3
    );
}
