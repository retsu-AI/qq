use super::*;

#[tokio::test]
async fn session_summaries_carry_the_latest_run_activity_while_running() {
    let (_directory, store, claimed) = claimed_store_fixture().await;
    let request = SnapshotRequest::new(
        claimed.identity.workspace_id,
        Some(claimed.identity.session_id),
        4,
        4,
    );
    let idle = store.snapshot(request.clone()).await.unwrap();
    assert_eq!(idle.focused.unwrap().summary.activity, None);

    store
        .append_run_activity(&claimed, RunActivity::Reasoning)
        .await
        .unwrap();
    store
        .append_run_activity(&claimed, RunActivity::GeneratingResponse)
        .await
        .unwrap();
    let live = store.snapshot(request).await.unwrap();
    let summary = live.focused.unwrap().summary;
    assert_eq!(summary.active_run_id, Some(claimed.identity.run_id));
    assert_eq!(summary.activity, Some(RunActivity::GeneratingResponse));
    assert!(
        live.sessions
            .iter()
            .any(|session| session.activity == Some(RunActivity::GeneratingResponse))
    );
}

#[tokio::test]
async fn measured_occupancy_basis_persists_atomically_and_reloads_with_the_reservation() {
    let (directory, store, claimed) = claimed_store_fixture().await;
    let model = test_resolved_model("test/model", "test/model", 256, None);
    let shape = context_request_shape(&model);
    let static_prefix = test_static_prefix(2, Some(3));
    let basis = context_occupancy_basis(shape.digest, static_prefix, 1_000);
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::assistant("measured"),
                calls: Vec::new(),
                turn_message: None,
                context_tokens: Some(100),
                occupancy_basis: Some(basis),
                usage: Some(usage(100, 1)),
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: claimed.identity.session_id,
                input: vec![InputPart::text("continue".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    store.close().await.unwrap();
    drop(store);

    let reopened = Store::open(database_path).await.unwrap();
    let reserved = reopened.reserve_next_run(false).await.unwrap().unwrap();
    let occupancy = reserved
        .context_occupancy
        .expect("the next reservation loads occupancy in its existing query");
    assert_eq!(occupancy.context_tokens, 100);
    assert_eq!(occupancy.basis, basis);
    assert_eq!(
        compatible_context_tokens(occupancy, shape, static_prefix, 1_024),
        Some(124)
    );
    reopened
        .finish_reserved_run(&reserved, RunOutcome::Cancelled)
        .await
        .unwrap();
}

#[tokio::test]
async fn malformed_context_bases_are_cleared_while_reserving_instead_of_failing_the_run() {
    let (directory, store, claimed) = claimed_store_fixture().await;
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: claimed.identity.session_id,
                input: vec![InputPart::text("continue".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let session_id = claimed.identity.session_id;
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE sessions
                     SET context_tokens = 100,
                         context_occupancy_json = '{not-json',
                         pending_context_overflow_basis_json = '{also-not-json'
                     WHERE id = ?1",
                [session_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    store.close().await.unwrap();
    drop(store);

    let reopened = Store::open(database_path).await.unwrap();
    let reserved = reopened.reserve_next_run(false).await.unwrap().unwrap();
    assert_eq!(reserved.context_occupancy, None);
    let stored = reopened
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT context_occupancy_json,
                            pending_context_overflow_basis_json
                     FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
        })
        .await
        .unwrap();
    assert_eq!(stored, (None, None));
    reopened
        .finish_reserved_run(&reserved, RunOutcome::Cancelled)
        .await
        .unwrap();
}

#[tokio::test]
async fn interleaved_turn_framing_accepts_exact_capacity_and_rejects_one_over() {
    for one_over in [false, true] {
        let (_directory, store, claimed) = claimed_store_fixture().await;
        let message_id = MessageId::generate().unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "provider-call".to_owned(),
            name: "read_file".to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            arguments: r#"{"path":"note.txt"}"#.to_owned(),
            rejection: None,
        };
        let message = Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Text {
                    text: "a".to_owned(),
                },
                ContentBlock::tool_call(
                    call.provider_call_id.clone(),
                    call.name.clone(),
                    &serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap(),
                ),
                ContentBlock::Text {
                    text: "b".to_owned(),
                },
            ],
        );
        let measured = crate::measure_message(&message);
        let context_base = u64::try_from(MAX_CONTEXT_BYTES)
            .unwrap()
            .saturating_sub(measured)
            .saturating_add(u64::from(one_over));
        store
            .call(Priority::Control, move |connection| {
                connection.execute(
                    "UPDATE runs
                         SET context_base_bytes = ?2, context_increment_bytes = 0
                         WHERE id = ?1",
                    params![claimed.identity.run_id.to_string(), context_base],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .begin_assistant_message(&claimed, message_id, 1, TextChannel::Output, "a".to_owned())
            .await
            .unwrap();
        store
            .append_text(&claimed, message_id, TextChannel::Output, "b".to_owned())
            .await
            .unwrap();
        let result = store
            .persist_model_turn(
                &claimed,
                ModelTurnCommit {
                    turn_ordinal: 1,
                    message,
                    calls: vec![call],
                    turn_message: Some(message_id),
                    context_tokens: None,
                    occupancy_basis: None,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    accounting: None,
                    truncated: false,
                },
            )
            .await;
        let (increment, turns, calls, state): (u64, u64, u64, String) = store
            .call(Priority::Control, move |connection| {
                connection
                    .query_row(
                        "SELECT r.context_increment_bytes,
                                (SELECT COUNT(*) FROM model_turns WHERE run_id = r.id),
                                (SELECT COUNT(*) FROM tool_calls WHERE run_id = r.id),
                                m.state
                         FROM runs r JOIN messages m ON m.id = ?2
                         WHERE r.id = ?1",
                        params![claimed.identity.run_id.to_string(), message_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .map_err(|_| SessionRuntimeError::CODEC)
            })
            .await
            .unwrap();
        if one_over {
            assert_eq!(result.unwrap_err(), SessionRuntimeError::OutputTooLarge);
            assert_eq!(increment, 2);
            assert_eq!((turns, calls, state.as_str()), (0, 0, "streaming"));
        } else {
            result.unwrap();
            assert_eq!(increment, measured);
            assert_eq!((turns, calls, state.as_str()), (1, 1, "complete"));
        }
    }
}

#[tokio::test]
async fn chunk_and_event_failures_roll_back_the_entire_streaming_transaction() {
    for trigger in [
        "CREATE TRIGGER reject_message_chunk BEFORE INSERT ON message_chunks
         BEGIN SELECT RAISE(ABORT, 'injected chunk failure'); END;",
        r#"CREATE TRIGGER reject_text_event BEFORE INSERT ON events
           WHEN NEW.envelope_json LIKE '%"type":"text_appended"%'
           BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;"#,
    ] {
        let (_directory, store, claimed) = claimed_store_fixture().await;
        let before = streaming_transaction_state(&store, claimed.identity.run_id).await;
        store
            .call(Priority::Control, move |connection| {
                connection
                    .execute_batch(trigger)
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)
            })
            .await
            .unwrap();

        assert_eq!(
            store
                .begin_assistant_message(
                    &claimed,
                    MessageId::generate().unwrap(),
                    1,
                    TextChannel::Output,
                    "chunk".to_owned(),
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::CONSTRAINT
        );
        assert_eq!(
            streaming_transaction_state(&store, claimed.identity.run_id).await,
            before
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn turn_and_event_failures_roll_back_message_counter_turn_and_tool_rows() {
    for trigger in [
        "CREATE TRIGGER reject_model_turn BEFORE INSERT ON model_turns
         BEGIN SELECT RAISE(ABORT, 'injected turn failure'); END;",
        r#"CREATE TRIGGER reject_tool_event BEFORE INSERT ON events
           WHEN NEW.envelope_json LIKE '%"type":"tool_call_requested"%'
           BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;"#,
    ] {
        let (_directory, store, claimed) = claimed_store_fixture().await;
        let message_id = MessageId::generate().unwrap();
        store
            .begin_assistant_message(
                &claimed,
                message_id,
                1,
                TextChannel::Output,
                "answer".to_owned(),
            )
            .await
            .unwrap();
        let before = streaming_transaction_state(&store, claimed.identity.run_id).await;
        store
            .call(Priority::Control, move |connection| {
                connection
                    .execute_batch(trigger)
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)
            })
            .await
            .unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "provider-call".to_owned(),
            name: "read_file".to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            arguments: r#"{"path":"note.txt"}"#.to_owned(),
            rejection: None,
        };
        assert_eq!(
            store
                .persist_model_turn(
                    &claimed,
                    ModelTurnCommit {
                        turn_ordinal: 1,
                        message: Message::new(
                            Role::Assistant,
                            vec![ContentBlock::tool_call(
                                call.provider_call_id.clone(),
                                call.name.clone(),
                                &serde_json::from_str::<serde_json::Value>(&call.arguments)
                                    .unwrap()
                            )],
                        ),
                        calls: vec![call],
                        turn_message: Some(message_id),
                        context_tokens: None,
                        occupancy_basis: None,
                        usage: None,
                        estimated_cost_usd_nanos: None,
                        accounting: None,
                        truncated: false,
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::CONSTRAINT
        );
        assert_eq!(
            streaming_transaction_state(&store, claimed.identity.run_id).await,
            before
        );
        let (message_state, tool_calls): (String, u64) = store
            .call(Priority::Control, move |connection| {
                let message_state = connection.query_row(
                    "SELECT state FROM messages WHERE id = ?1",
                    [message_id.to_string()],
                    |row| row.get(0),
                )?;
                let tool_calls = connection.query_row(
                    "SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1",
                    [claimed.identity.run_id.to_string()],
                    |row| row.get(0),
                )?;
                Ok((message_state, tool_calls))
            })
            .await
            .unwrap();
        assert_eq!(message_state, "streaming");
        assert_eq!(tool_calls, 0);
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn concurrent_chunks_replay_in_committed_order_after_restart() {
    let (directory, store, claimed) = claimed_store_fixture().await;
    let database_path = directory.path().join("sessions.sqlite3");
    let message_id = MessageId::generate().unwrap();
    let started = store
        .begin_assistant_message(
            &claimed,
            message_id,
            1,
            TextChannel::Output,
            "first|".to_owned(),
        )
        .await
        .unwrap();
    let after = started.last().unwrap().cursor.sequence;
    let appends = (0..32_u8).map(|ordinal| {
        let store = store.clone();
        let claimed = claimed.clone();
        async move {
            store
                .append_text(
                    &claimed,
                    message_id,
                    TextChannel::Output,
                    format!("{ordinal:02}|"),
                )
                .await
                .unwrap()
        }
    });
    let mut committed = futures_util::future::join_all(appends).await;
    committed.sort_by_key(|event| event.cursor.sequence);
    let mut expected = "first|".to_owned();
    for event in &committed {
        let SessionEvent::TextAppended { text, .. } = &event.event else {
            panic!("append returned a non-text event")
        };
        expected.push_str(text);
    }
    let workspace_id = claimed.identity.workspace_id;
    let session_id = claimed.identity.session_id;
    store.close().await.unwrap();
    drop(store);

    let reopened = Store::open(database_path).await.unwrap();
    let replay = reopened
        .events_after(workspace_id, after, 64)
        .await
        .unwrap();
    assert_eq!(replay, committed);
    let snapshot = reopened
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    let assistant = snapshot
        .focused
        .unwrap()
        .messages
        .into_iter()
        .find(|message| message.id == message_id)
        .unwrap();
    assert_eq!(assistant.output, expected);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn overflowing_text_append_persists_no_counter_chunk_or_event() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: directory.path().to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let created = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("x".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    let message_id = MessageId::generate().unwrap();
    store
        .begin_assistant_message(&claimed, message_id, 1, TextChannel::Output, "x".to_owned())
        .await
        .unwrap();
    let run_id = claimed.identity.run_id;
    let before = store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET context_base_bytes = ?2,
                                     context_increment_bytes = 1
                     WHERE id = ?1",
                params![run_id.to_string(), MAX_CONTEXT_BYTES - 1],
            )?;
            let chunks =
                connection.query_row("SELECT COUNT(*) FROM message_chunks", [], |row| {
                    row.get::<_, u64>(0)
                })?;
            let events = connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                row.get::<_, u64>(0)
            })?;
            Ok((chunks, events))
        })
        .await
        .unwrap();

    assert_eq!(
        store
            .append_text(&claimed, message_id, TextChannel::Output, "y".to_owned(),)
            .await
            .unwrap_err(),
        SessionRuntimeError::OutputTooLarge
    );

    let after = store
        .call(Priority::Control, move |connection| {
            let increment = connection.query_row(
                "SELECT context_increment_bytes FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get::<_, u64>(0),
            )?;
            let chunks =
                connection.query_row("SELECT COUNT(*) FROM message_chunks", [], |row| {
                    row.get::<_, u64>(0)
                })?;
            let events = connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                row.get::<_, u64>(0)
            })?;
            Ok((increment, chunks, events))
        })
        .await
        .unwrap();
    assert_eq!(after, (1, before.0, before.1));
}

#[test]
fn interrupted_compaction_commits_no_marker_and_can_be_retried() {
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
    // A compaction run crashed mid-summarization: still marked running,
    // and — because summary and marker commit atomically with the run's
    // completion — no session_compactions row exists.
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, active_run_id,
                                  model, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'S', 'running', ?3, 'test/model', 1, 1)",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                run_id.to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, kind, created_at_ms,
                              started_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::from_bytes([4; 16]).to_string(),
                MessageId::from_bytes([5; 16]).to_string(),
                MessageId::from_bytes([6; 16]).to_string(),
            ],
        )
        .unwrap();

    recover_interrupted_runs(&mut connection, store_id).unwrap();

    assert_eq!(
        connection
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "interrupted"
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        0,
        "a crashed compaction must leave no marker"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id.to_string()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "idle"
    );

    // The command can simply be retried.
    let applied = execute_command(
        &mut connection,
        store_id,
        CommandId::from_bytes([9; 16]),
        SessionCommand::CompactSession { session_id },
        None,
        &WorkspaceGrantSeed::default(),
    )
    .unwrap();
    assert!(matches!(
        applied.receipt.outcome,
        CommandOutcome::CompactionQueued { session_id: queued, .. }
            if queued == session_id
    ));
}

#[test]
fn cost_uses_the_context_tier_and_cache_rates() {
    let pricing = ModelPricing {
        input_usd_nanos_per_token: 1,
        output_usd_nanos_per_token: 2,
        cache_read_usd_nanos_per_token: Some(1),
        cache_write_usd_nanos_per_token: Some(2),
        context_tier: Some(qq_protocol::ModelPricingTier {
            above_input_tokens: 10,
            input_usd_nanos_per_token: 10,
            output_usd_nanos_per_token: 20,
            cache_read_usd_nanos_per_token: Some(3),
            cache_write_usd_nanos_per_token: Some(4),
        }),
        provenance: "test".to_owned(),
    };
    assert_eq!(
        run_cost(
            TokenUsage {
                input_tokens: 8,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 3,
                reasoning_tokens: None,
            },
            &pricing,
        ),
        Some(8 * 10 + 2 * 3 + 4 + 3 * 20)
    );
}

#[test]
fn prompt_titles_are_compact_and_bounded() {
    assert_eq!(
        prompt_title("  Fix the login\n\tredirect  "),
        "Fix the login redirect"
    );
    assert_eq!(
        prompt_title(&"x".repeat(49)),
        format!("{}...", "x".repeat(48))
    );
    assert_eq!(prompt_title("\0\u{1b}\u{202e}\u{2066}"), "New session");
}
