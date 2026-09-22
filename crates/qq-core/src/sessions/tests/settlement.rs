use super::*;

/// Every settlement path shares one null guard: a run that already carries
/// an outcome is never re-settled, no second `RunFinished` is appended,
/// and the session's ownership of a *newer* run is not cleared.
#[tokio::test]
async fn settling_a_settled_run_is_a_no_op_on_every_path() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, session_id, first) = create_claimed_parent(&store, directory.path()).await;
    let finished = store
        .finish_run(
            &first,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(matches!(
        finished.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    // A newer run now owns the session; a stale settlement of the first
    // run must not steal that ownership.
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("second".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let second = store.claim_next_run(false).await.unwrap().unwrap();
    let store_id = store.store_id();
    let events_before = store
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();

    // Path 1: the started-run settlement, replayed.
    let replayed = store
        .finish_run(
            &first,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(replayed.is_empty(), "replayed finish_run published events");
    // Path 2: the recovery / panic settlement path, which previously had
    // no `outcome_json IS NULL` guard.
    let first_claim = first.clone();
    store
        .call_write(Priority::Control, move |connection| {
            let transaction = store::begin_unit(connection)?;
            let settled = settle_run(
                &transaction,
                store_id,
                &first_claim,
                RunOutcome::Interrupted,
                None,
                SettlementCause::Recovery,
            )?;
            assert!(settled.is_none());
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    // Path 3: the queued-run settlement, replayed against a started row.
    let first_identity = first.identity;
    store
        .call_write(Priority::Control, move |connection| {
            let transaction = store::begin_unit(connection)?;
            let settled = finish_queued_run_with_outcome(
                &transaction,
                store_id,
                first_identity.workspace_id,
                first_identity.session_id,
                first_identity.run_id,
                RunOutcome::Cancelled,
                now_ms(),
            )?;
            assert!(settled.is_none());
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();

    let second_id = second.identity.run_id;
    let first_id = first.identity.run_id;
    let (first_row, second_row, active, events_after) = store
        .call(Priority::Control, move |connection| {
            Ok((
                load_run(connection, first_id)?,
                load_run(connection, second_id)?,
                connection.query_row(
                    "SELECT active_run_id FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )?,
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(first_row.status, RunStatus::Completed);
    assert_eq!(first_row.outcome, Some(RunOutcome::Completed));
    assert_eq!(second_row.status, RunStatus::Running);
    assert_eq!(active, Some(second_id.to_string()));
    assert_eq!(
        events_after, events_before,
        "a replayed settlement appended events"
    );
    store
        .finish_run(
            &second,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
}

/// An auto-compaction that already committed its marker is not re-settled
/// by the original prompt's later teardown: exactly one marker, one
/// `RunFinished` for the compaction, and the committed outcome stands.
#[tokio::test]
async fn a_committed_compaction_is_not_resettled_by_the_prompts_teardown() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, session_id, original) = create_claimed_parent(&store, directory.path()).await;
    // Reset the claim to a reservation so an auto-compaction can start on
    // the prompt's behalf.
    store
        .finish_run(
            &original,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("long prompt".repeat(64)); 1],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let original = store.reserve_next_run(false).await.unwrap().unwrap();
    let (compaction, _) = store
        .start_auto_compaction(&original, test_prepared_audit(&original), None)
        .await
        .unwrap()
        .unwrap();
    let summary = "## Summary\n\nThe user asked for a long prompt.".to_owned();
    let committed = store
        .finish_compaction_run(&compaction, summary, None, TeardownComplete::nothing_ran())
        .await
        .unwrap();
    let compaction_outcome = committed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == compaction.identity.run_id => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    let events_before = store
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();

    // The prompt's teardown fails the compaction again (the shape of the
    // `finish_run(compaction); finish_prepared_run(original)` pairs in
    // execution.rs) and then the panic path sweeps the session.
    let replayed = store
        .finish_run(
            &compaction,
            execution::internal_failure("late teardown"),
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(replayed.is_empty());
    let swept = store
        .settle_panicked_execution(&original, execution::internal_failure("late panic sweep"))
        .await
        .unwrap();
    // The sweep settles only the still-queued original, never the
    // compaction.
    assert!(swept.events.iter().all(|event| !matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, .. } if *run_id == compaction.identity.run_id
    )));

    let compaction_id = compaction.identity.run_id;
    let (row, markers, finished_events) = store
        .call(Priority::Control, move |connection| {
            Ok((
                load_run(connection, compaction_id)?,
                connection.query_row(
                    "SELECT COUNT(*) FROM session_compactions WHERE run_id = ?1",
                    [compaction_id.to_string()],
                    |row| row.get::<_, u64>(0),
                )?,
                connection.query_row(
                    "SELECT COUNT(*) FROM events
                         WHERE envelope_json LIKE '%\"type\":\"run_finished\"%'
                           AND envelope_json LIKE ?1",
                    [format!("%{compaction_id}%")],
                    |row| row.get::<_, u64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(row.outcome, Some(compaction_outcome.clone()));
    assert_eq!(finished_events, 1, "compaction settled more than once");
    assert_eq!(
        markers,
        u64::from(matches!(compaction_outcome, RunOutcome::Completed))
    );
    let events_after = store
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    // Only the original's own settlement (and its parent update, if any)
    // may have been appended by the sweep.
    assert!(
        swept.events.len() as u64 == events_after - events_before,
        "sweep appended {} events but {} were persisted",
        swept.events.len(),
        events_after - events_before
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn recovery_interrupts_only_the_current_turns_message() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
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
                input: vec![InputPart::text("read".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    // Turn one: streamed text, then a completed tool call; the turn
    // committed, finalizing its message.
    let first_message = MessageId::generate().unwrap();
    store
        .begin_assistant_message(
            &claimed,
            first_message,
            1,
            TextChannel::Output,
            "Checking. ".to_owned(),
        )
        .await
        .unwrap();
    let tool_call_id = ToolCallId::generate().unwrap();
    let call = RuntimeToolCall {
        id: tool_call_id,
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        name: "read_file".to_owned(),
        effect: crate::catalog::EffectClass::ReadOnly,
        arguments: r#"{"path":"note.txt"}"#.to_owned(),
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
                        ContentBlock::Text {
                            text: "Checking. ".to_owned(),
                        },
                        ContentBlock::tool_call(
                            call.provider_call_id.clone(),
                            call.name.clone(),
                            &serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap(),
                        ),
                    ],
                ),
                calls: vec![call],
                turn_message: Some(first_message),
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    store.start_tool_call(&claimed, tool_call_id).await.unwrap();
    store
        .finish_tool_call(
            &claimed,
            tool_call_id,
            "noted\n".to_owned(),
            false,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
    // Turn two starts streaming, then the server crashes.
    let second_message = MessageId::generate().unwrap();
    store
        .begin_assistant_message(
            &claimed,
            second_message,
            2,
            TextChannel::Output,
            "So far".to_owned(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    drop(store);

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.messages.len(), 3);
    let first = focused
        .messages
        .iter()
        .find(|message| message.id == first_message)
        .unwrap();
    let second = focused
        .messages
        .iter()
        .find(|message| message.id == second_message)
        .unwrap();
    assert_eq!(
        first.state,
        MessageState::Complete,
        "the committed turn's message must survive recovery untouched"
    );
    assert_eq!(second.state, MessageState::Interrupted);
    assert_eq!(focused.runs[0].outcome, Some(RunOutcome::Interrupted));
}

#[tokio::test]
async fn recovery_interrupts_running_tools_without_reexecuting_them() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
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
                input: vec![InputPart::text("read".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    let tool_call_id = ToolCallId::generate().unwrap();
    // A mutating call crashed mid-execution: replay must surface an
    // explicit interrupted result, never re-run the side effect.
    let call = RuntimeToolCall {
        id: tool_call_id,
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "provider-call".to_owned(),
        name: "edit_file".to_owned(),
        effect: crate::catalog::EffectClass::Mutating,
        arguments: r#"{"edits":[{"path":"note.txt","old":"a","new":"b"}]}"#.to_owned(),
        rejection: None,
    };
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
                        &serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap(),
                    )],
                ),
                calls: vec![call],
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    let started = store.start_tool_call(&claimed, tool_call_id).await.unwrap();
    store.close().await.unwrap();
    drop(store);

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: started.cursor,
        })
        .unwrap();
    let recovered = collect_through_finished(&mut events).await;
    assert!(matches!(
        &recovered[0].event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.id == tool_call_id
                && tool_call.state == ToolCallState::Interrupted
                && tool_call.is_error
    ));
    assert!(matches!(
        &recovered[1].event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Interrupted,
            ..
        }
    ));
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 4,
        })
        .await
        .unwrap();
    assert_eq!(
        snapshot.focused.unwrap().tool_calls[0].state,
        ToolCallState::Interrupted
    );

    runtime
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
    let _ = collect_through_finished(&mut events).await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        requests[0].messages()[2].content(),
        [ContentBlock::ToolResult {
            call_id,
            content,
            is_error: true,
        }] if call_id == "provider-call" && content == INTERRUPTED_TOOL_RESULT
    ));
}

#[tokio::test]
async fn orphaned_tool_call_blocks_replay_with_synthesized_interrupted_results() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
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
                input: vec![InputPart::text("read".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    // Simulate the pre-fix crash window: the model turn committed with a
    // ToolCall block, but no tool_calls rows were ever written.
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    vec![ContentBlock::tool_call(
                        "orphan-call".to_owned(),
                        "read_file".to_owned(),
                        &serde_json::json!({"path": "note.txt"}),
                    )],
                ),
                calls: Vec::new(),
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    drop(store);

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.receipt.committed_through,
        })
        .unwrap();
    let _ = collect_through_finished(&mut events).await;
    runtime
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
    let _ = collect_through_finished(&mut events).await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let messages = requests[0].messages();
    assert!(matches!(
        messages[1].content(),
        [ContentBlock::ToolCall { id, .. }] if id == "orphan-call"
    ));
    assert!(matches!(
        messages[2].content(),
        [ContentBlock::ToolResult {
            call_id,
            content,
            is_error: true,
        }] if call_id == "orphan-call" && content == INTERRUPTED_TOOL_RESULT
    ));
    assert_tool_results_are_exact(messages);
}

#[tokio::test]
async fn exceeding_the_context_budget_fails_the_run_with_a_policy_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ContextBudgetLoader),
    )
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
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("fill the context".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let observed = collect_through_finished(&mut events).await;
    let finished = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        finished,
        RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Policy,
                ref message,
            }
        } if message.contains("4 MiB limit")
    ));
}

#[tokio::test]
async fn a_panicking_run_task_fails_durably_and_the_session_keeps_scheduling() {
    struct PanicOnceLoader {
        calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for PanicOnceLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                struct PanicOnceProvider {
                    calls: Arc<AtomicUsize>,
                }

                impl Provider for PanicOnceProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            panic!("injected run-task panic");
                        }
                        Box::pin(stream::iter([
                            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                                text: "recovered".to_owned(),
                            }),
                            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                        ]))
                    }
                }

                Runtime::new(PanicOnceProvider { calls }, "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(PanicOnceLoader {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    )
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

    let failed_run = queue_prompt(&runtime, session_id, "panic".to_owned()).await;
    let failed = tokio::time::timeout(
        Duration::from_secs(1),
        collect_through_finished(&mut events),
    )
    .await
    .expect("a panicking run must settle durably within one second");
    assert!(failed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Server,
                    ..
                }
            },
            ..
        } if *run_id == failed_run
    )));

    let continued_run = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
    let continued = collect_through_finished(&mut events).await;
    assert!(continued.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Completed,
            ..
        } if run_id == continued_run
    )));
}

#[tokio::test]
async fn a_panicking_loader_settles_the_reservation_without_run_started() {
    struct PanicBeforeStartLoader {
        loads: Arc<AtomicUsize>,
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for PanicBeforeStartLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let panic = self.loads.fetch_add(1, Ordering::SeqCst) == 0;
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                assert!(!panic, "injected loader panic before RunStarted");
                struct CountingProvider(Arc<AtomicUsize>);

                impl Provider for CountingProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        Box::pin(stream::iter([
                            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                                text: "recovered".to_owned(),
                            }),
                            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                        ]))
                    }
                }

                Runtime::new(CountingProvider(provider_calls), "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(PanicBeforeStartLoader {
            loads: Arc::new(AtomicUsize::new(0)),
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
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

    let failed_run = queue_prompt(&runtime, session_id, "panic in load".to_owned()).await;
    let failed = collect_until(&mut events, finished_for(failed_run)).await;
    assert!(failed.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunStarted { run_id, .. } if run_id == failed_run
    )));
    assert!(failed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Server,
                    ..
                }
            },
            ..
        } if *run_id == failed_run
    )));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);

    let next = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
    let continued = collect_until(&mut events, finished_for(next)).await;
    assert!(continued.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Completed,
            ..
        } if run_id == next
    )));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_cancels_running_and_queued_prompts_before_returning() {
    let directory = tempfile::tempdir().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 1;
    let runtime = SessionRuntime::open(options, Arc::new(PricedHangingLoader))
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

    let running = queue_prompt(&runtime, session_id, "run".to_owned()).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if matches!(
                event.event,
                SessionEvent::RunStarted { run_id, .. } if run_id == running
            ) {
                break;
            }
        }
    })
    .await
    .expect("the first prompt must start before shutdown");
    let queued = queue_prompt(&runtime, session_id, "queued".to_owned()).await;

    tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("shutdown must settle bounded provider work")
        .unwrap();

    let mut finished = HashMap::new();
    let mut terminal_count = 0;
    tokio::time::timeout(Duration::from_secs(1), async {
        while finished.len() < 2 {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::RunFinished {
                run_id, outcome, ..
            } = event.event
            {
                terminal_count += 1;
                finished.insert(run_id, outcome);
            }
        }
    })
    .await
    .expect("both accepted prompts must publish terminal events");
    assert_eq!(terminal_count, 2);
    assert_eq!(finished.get(&running), Some(&RunOutcome::Cancelled));
    assert_eq!(finished.get(&queued), Some(&RunOutcome::Cancelled));

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 4,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.summary.status, SessionStatus::Idle);
    assert_eq!(focused.summary.active_run_id, None);
    assert!(
        focused
            .runs
            .iter()
            .all(|run| run.status == RunStatus::Cancelled)
    );

    let error = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("too late".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error, SessionRuntimeError::Unavailable);
}

#[tokio::test]
async fn subscribers_converge_and_replay_from_an_intermediate_cursor() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let request = SubscribeRequest {
        workspace_id,
        after: created.committed_through,
    };
    let mut first = runtime.subscribe(request).unwrap();
    let mut second = runtime.subscribe(request).unwrap();

    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("converge".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        collect_through_finished(&mut first),
        collect_through_finished(&mut second),
    );
    assert_eq!(first, second);
    assert!(first.len() > 2);

    let split = first.len() / 2;
    let mut replay = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: first[split - 1].cursor,
        })
        .unwrap();
    let replayed = tokio::time::timeout(Duration::from_secs(2), async {
        let mut replayed = Vec::new();
        for _ in split..first.len() {
            replayed.push(replay.next().await.unwrap().unwrap());
        }
        replayed
    })
    .await
    .unwrap();

    assert_eq!(replayed, first[split..]);
}

#[tokio::test]
async fn scheduler_store_failure_disables_runtime_and_existing_subscribers() {
    let (directory, runtime) = test_runtime().await;
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
    runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("persist me".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        events.next().await.unwrap().unwrap().event,
        SessionEvent::PromptQueued { .. }
    ));

    let worker = runtime.inner.store.stop_worker_for_test().unwrap();
    tokio::task::spawn_blocking(move || worker.join().unwrap())
        .await
        .unwrap();

    let mut failed = runtime.inner.failed.subscribe();
    runtime.request_schedule();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !*failed.borrow() {
            failed.changed().await.unwrap();
        }
    })
    .await
    .unwrap();

    assert_eq!(
        events.next().await,
        Some(Err(SessionRuntimeError::Unavailable))
    );
    assert_eq!(
        runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                include_sessions: Vec::new(),
                session_limit: 1,
                message_limit: 1,
            })
            .await
            .unwrap_err(),
        SessionRuntimeError::Unavailable
    );
    assert_eq!(
        runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .err(),
        Some(SessionRuntimeError::Unavailable)
    );
}

#[tokio::test]
async fn saturated_cancellation_read_and_start_wait_without_failing_the_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let queued = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("retry the read".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    // Hold the worker on the cancellation read and saturate the control
    // lane behind it. The supervisor's reserved start must wait for a
    // capacity wake rather than be told to retry or fail the run.
    let (read_entered, release_read) = store::hold_cancellation_read(run_id);
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    runtime.request_schedule();
    tokio::time::timeout(Duration::from_secs(2), read_entered)
        .await
        .unwrap()
        .unwrap();
    let saturated = saturate_control_lane(&runtime).await;
    release_read.send(()).unwrap();
    // The start attempt is now queued behind a full lane and cannot make
    // progress until capacity returns.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(matches!(
        runtime
            .inner
            .store
            .call(Priority::Control, |_| Ok(()))
            .await,
        Err(SessionRuntimeError::Overloaded)
    ));
    saturated.release().await;
    let observed = collect_until(&mut events, finished_for(run_id)).await;

    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunStarted { run_id: started, .. } if started == run_id
    )));
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        } if finished == run_id
    )));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(!*runtime.inner.failed.borrow());
}

#[tokio::test]
async fn permanent_cancellation_read_failure_settles_only_that_run() {
    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 2;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let first_session = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: first_session,
    } = first_session.outcome
    else {
        panic!("unexpected receipt")
    };
    let second_session = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: second_session,
    } = second_session.outcome
    else {
        panic!("unexpected receipt")
    };
    let first = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: first_session,
                input: vec![InputPart::text("fail initialization".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: first_run, ..
    } = first.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let second = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: second_session,
                input: vec![InputPart::text("must not load".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: second_run, ..
    } = second.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    store::fail_cancellation_reads(first_run, [SessionRuntimeError::CONSTRAINT]);
    let (second_read, release_second) = store::hold_cancellation_read(second_run);
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: second.receipt.committed_through,
        })
        .unwrap();
    runtime.request_schedule();
    tokio::time::timeout(Duration::from_secs(1), second_read)
        .await
        .expect("both reservations must transfer to supervisors")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let run = runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    load_run(connection, first_run)
                })
                .await
                .unwrap();
            if run.outcome.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the permanent read error must settle its own run");
    assert!(!*runtime.inner.failed.borrow());
    release_second.send(()).unwrap();
    let observed = collect_until(&mut events, finished_for(second_run)).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.inner.permits.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both supervisors must release their permits");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(observed.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunStarted { run_id, .. } if run_id == first_run
    )));
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunStarted { run_id, .. } if run_id == second_run
    )));
    let first = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, first_run)
        })
        .await
        .unwrap();
    assert!(matches!(
        &first.outcome,
        Some(RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                message,
            }
        }) if message.contains("failed to read reserved-run cancellation state")
    ));
    let second = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, second_run)
        })
        .await
        .unwrap();
    assert_eq!(second.outcome, Some(RunOutcome::Completed));
    assert!(!*runtime.inner.failed.borrow());
}

#[tokio::test]
async fn held_start_cannot_race_past_a_sibling_runtime_failure() {
    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 2;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let first = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: first_session,
    } = first.outcome
    else {
        panic!("unexpected receipt")
    };
    let second = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: second_session,
    } = second.outcome
    else {
        panic!("unexpected receipt")
    };
    let retrying = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: first_session,
                input: vec![InputPart::text("wait at start".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: retrying_run,
        ..
    } = retrying.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let failing = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: second_session,
                input: vec![InputPart::text("fail initialization".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: failing_run,
        ..
    } = failing.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let (start_entered, release_start) = store::hold_failing_reserved_start(retrying_run);
    let (read_entered, release_read) = store::hold_cancellation_read(failing_run);
    store::fail_cancellation_reads(failing_run, [SessionRuntimeError::CONSTRAINT]);
    store::fail_reserved_settlements(failing_run, [SessionRuntimeError::CONSTRAINT]);
    runtime.request_schedule();
    tokio::time::timeout(Duration::from_secs(1), start_entered)
        .await
        .expect("the first supervisor must enter the reserved-start attempt")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), read_entered)
        .await
        .expect("the sibling supervisor must own its reservation")
        .unwrap();
    release_read.send(()).unwrap();
    let mut failed = runtime.inner.failed.subscribe();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !*failed.borrow() {
            failed.changed().await.unwrap();
        }
    })
    .await
    .expect("the sibling terminal-settlement failure must fail the runtime");
    release_start.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.inner.permits.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both supervisors must settle before releasing their permits");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let events = runtime
        .inner
        .store
        .events_after(workspace_id, failing.receipt.committed_through.sequence, 16)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::RunStarted { .. }))
    );
    let retrying = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, retrying_run)
        })
        .await
        .unwrap();
    assert!(matches!(
        retrying.outcome,
        Some(RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                ..
            }
        })
    ));
    let failing = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, failing_run)
        })
        .await
        .unwrap();
    assert_eq!(failing.status, RunStatus::Queued);
    assert_eq!(failing.outcome, None);
}

#[tokio::test]
async fn fatal_settlement_stops_an_already_started_sibling_before_provider_poll() {
    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 2;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let started_session = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: started_session,
    } = started_session.outcome
    else {
        panic!("unexpected receipt")
    };
    let failing_session = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: failing_session,
    } = failing_session.outcome
    else {
        panic!("unexpected receipt")
    };
    let started = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: started_session,
                input: vec![InputPart::text("start but do not poll".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: started_run,
        ..
    } = started.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let failing = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: failing_session,
                input: vec![InputPart::text("fail settlement".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: failing_run,
        ..
    } = failing.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let (_initial_read, release_initial_read) = store::hold_cancellation_read(started_run);
    let (post_start_read, release_post_start_read) = store::hold_cancellation_read(started_run);
    let (failing_read, release_failing_read) = store::hold_cancellation_read(failing_run);
    store::fail_cancellation_reads(failing_run, [SessionRuntimeError::CONSTRAINT]);
    store::fail_reserved_settlements(failing_run, [SessionRuntimeError::CONSTRAINT]);
    release_initial_read.send(()).unwrap();
    runtime.request_schedule();
    tokio::time::timeout(Duration::from_secs(1), post_start_read)
        .await
        .expect("the first run must commit start before provider polling")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), failing_read)
        .await
        .expect("the sibling supervisor must own its reservation")
        .unwrap();
    release_failing_read.send(()).unwrap();
    let mut failed = runtime.inner.failed.subscribe();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !*failed.borrow() {
            failed.changed().await.unwrap();
        }
    })
    .await
    .expect("the sibling terminal-settlement failure must fail the runtime");
    release_post_start_read.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.inner.permits.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both supervisors must stop and return their permits");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let events = runtime
        .inner
        .store
        .events_after(workspace_id, failing.receipt.committed_through.sequence, 16)
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunStarted { run_id, .. } if run_id == started_run
    )));
    assert!(events.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunStarted { run_id, .. } if run_id == failing_run
    )));
    let started = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, started_run)
        })
        .await
        .unwrap();
    assert!(matches!(
        started.outcome,
        Some(RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                ..
            }
        })
    ));
    let failing = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, failing_run)
        })
        .await
        .unwrap();
    assert_eq!(failing.status, RunStatus::Queued);
    assert_eq!(failing.outcome, None);
}

#[tokio::test]
async fn poisoned_cancellation_registry_settles_before_start_and_returns_the_permit() {
    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 1;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let queued = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("poisoned registry".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let poisoned = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = runtime.inner.cancellations.lock().unwrap();
        panic!("poison cancellation registry for test");
    }));
    assert!(poisoned.is_err());
    runtime.request_schedule();
    let mut failed = runtime.inner.failed.subscribe();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !*failed.borrow() {
            failed.changed().await.unwrap();
        }
        while runtime.inner.permits.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("poisoned registration must settle and release its permit");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let run = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, run_id)
        })
        .await
        .unwrap();
    assert!(matches!(
        run.outcome,
        Some(RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                ..
            }
        })
    ));
    let preparing: Option<String> = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT preparing_run_id FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(preparing, None);
    let events = runtime
        .inner
        .store
        .events_after(workspace_id, queued.receipt.committed_through.sequence, 8)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::RunStarted { .. }))
    );
}

#[tokio::test]
async fn permanent_prestart_settlement_failure_is_recovered_on_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(database_path.clone());
    options.max_active_runs = 1;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let queued = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("recover me".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET context_compaction_attempted = 1 WHERE id = ?1",
                [run_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store::fail_cancellation_reads(run_id, [SessionRuntimeError::CONSTRAINT]);
    store::fail_reserved_settlements(run_id, [SessionRuntimeError::CONSTRAINT]);
    runtime.request_schedule();
    let mut failed = runtime.inner.failed.subscribe();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !*failed.borrow() {
            failed.changed().await.unwrap();
        }
        while runtime.inner.permits.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the permanent settlement failure must release its permit");
    let (status, preparing): (String, Option<String>) = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT r.status, s.preparing_run_id
                     FROM runs r JOIN sessions s ON s.id = r.session_id
                     WHERE r.id = ?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(preparing.as_deref(), Some(run_id.to_string().as_str()));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    runtime.inner.store.close().await.unwrap();
    drop(runtime);

    let recovered_calls = Arc::new(AtomicUsize::new(0));
    let recovered = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&recovered_calls),
        }),
    )
    .await
    .unwrap();
    let mut events = recovered
        .subscribe(SubscribeRequest {
            workspace_id,
            after: queued.receipt.committed_through,
        })
        .unwrap();
    let observed = collect_until(&mut events, finished_for(run_id)).await;
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        } if finished == run_id
    )));
    assert_eq!(recovered_calls.load(Ordering::SeqCst), 1);
    let (attempted, preparing): (bool, Option<String>) = recovered
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT r.context_compaction_attempted, s.preparing_run_id
                     FROM runs r JOIN sessions s ON s.id = r.session_id
                     WHERE r.id = ?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert!(attempted);
    assert_eq!(preparing, None);
}

#[tokio::test]
async fn queues_follow_ups_without_reordering_conversation_context() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions {
            database_path: directory.path().join("sessions.sqlite3"),
            max_active_runs: 1,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            grant_authority: None,
            approval_reviewer: None,
        },
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
    )
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

    for prompt in ["first", "second"] {
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
    }
    let mut finished = 0;
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap().event, SessionEvent::RunFinished { .. }) {
                finished += 1;
                if finished == 2 {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1].messages(),
        [
            Message::user("first"),
            Message::assistant("answer"),
            Message::user("second"),
        ]
    );
}

#[tokio::test]
async fn follow_up_after_an_empty_completed_turn_reaches_the_provider() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions {
            database_path: directory.path().join("sessions.sqlite3"),
            max_active_runs: 1,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            grant_authority: None,
            approval_reviewer: None,
        },
        Arc::new(EmptyThenTextLoader {
            requests: Arc::clone(&requests),
        }),
    )
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

    submit_prompt_to(&runtime, session_id, "hello").await;
    let first = collect_through_finished(&mut events).await;
    assert!(first.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));

    submit_prompt_to(&runtime, session_id, "continue").await;
    let second = collect_through_finished(&mut events).await;
    assert!(
        second.iter().all(|event| !matches!(
            &event.event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Failed { failure },
                ..
            } if failure.message.contains("must not be empty")
        )),
        "follow-up failed with empty-conversation: {second:?}"
    );
    assert!(second.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1].messages(),
        [
            Message::user("hello"),
            Message::assistant(crate::EMPTY_TURN_PLACEHOLDER),
            Message::user("continue"),
        ]
    );
}

#[tokio::test]
async fn reservation_is_publicly_queued_exclusive_and_cancel_wins_before_start() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let command_id = CommandId::generate().unwrap();
    let resolved = store
        .command(
            command_id,
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let queued = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("wait".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let second = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("later".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let claimed = store.reserve_next_run(false).await.unwrap().unwrap();
    assert_eq!(claimed.identity.run_id, run_id);
    let synchronous = store
        .call(Priority::Control, |connection| {
            connection
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
        })
        .await
        .unwrap();
    assert_eq!(
        synchronous, 2,
        "authoritative start and terminal commits must run under FULL"
    );
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    assert!(
        store
            .events_after(workspace_id, second.receipt.committed_through.sequence, 100,)
            .await
            .unwrap()
            .is_empty()
    );
    let snapshot = store
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 4,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.summary.status, SessionStatus::Queued);
    assert_eq!(focused.summary.active_run_id, None);
    assert_eq!(focused.summary.queued_prompts, 2);
    assert!(
        focused
            .runs
            .iter()
            .all(|run| run.status == RunStatus::Queued)
    );
    assert!(focused.messages.iter().all(|message| {
        message.role != MessageRole::User || message.state == MessageState::Queued
    }));
    assert_eq!(
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession { session_id },
            )
            .await
            .err(),
        Some(SessionRuntimeError::SessionActive)
    );
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();

    assert!(
        store
            .start_reserved_run(&claimed, test_prepared_audit(&claimed), None)
            .await
            .unwrap()
            .is_none()
    );
    let run = store
        .call(Priority::Control, move |connection| {
            load_run(connection, run_id)
        })
        .await
        .unwrap();
    assert_eq!(run.outcome, Some(RunOutcome::Cancelled));
    let second_run_id = match second.receipt.outcome {
        CommandOutcome::PromptQueued { run_id, .. } => run_id,
        _ => panic!("unexpected receipt"),
    };
    store
        .call(Priority::Control, move |connection| {
            let deleted = connection.execute(
                "DELETE FROM messages WHERE run_id = ?1 AND role = 'user'",
                [second_run_id.to_string()],
            )?;
            if deleted == 1 {
                Ok(())
            } else {
                Err(SessionRuntimeError::CONSTRAINT)
            }
        })
        .await
        .unwrap();
    assert_eq!(
        store.reserve_next_run(false).await.err(),
        Some(SessionRuntimeError::CODEC)
    );
    let (synchronous, preparing): (i64, Option<String>) = store
        .call(Priority::Control, move |connection| {
            let synchronous =
                connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
            let preparing = connection.query_row(
                "SELECT preparing_run_id FROM sessions WHERE id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            Ok((synchronous, preparing))
        })
        .await
        .unwrap();
    assert_eq!(
        synchronous, 2,
        "reservation failures must restore FULL before returning"
    );
    assert_eq!(
        preparing, None,
        "the failed recoverable transaction must roll back its pointer"
    );
}

#[tokio::test]
async fn delayed_old_panic_cannot_settle_a_newer_auto_compaction() {
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };

    let old = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("old".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: old_id, .. } = old.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let old = store.reserve_next_run(false).await.unwrap().unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: old_id },
        )
        .await
        .unwrap();

    let new = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("new".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: new_id, .. } = new.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let new = store.reserve_next_run(false).await.unwrap().unwrap();
    let (compaction, _) = store
        .start_auto_compaction(&new, test_prepared_audit(&new), None)
        .await
        .unwrap()
        .unwrap();

    let settlement = store
        .settle_panicked_execution(
            &old,
            execution::internal_failure("delayed panic from an older scheduler task"),
        )
        .await
        .unwrap();
    assert!(settlement.events.is_empty());
    assert_eq!(settlement.run_ids, vec![old_id]);
    let (active, newer, session_state) = store
        .call(Priority::Control, move |connection| {
            Ok((
                load_run(connection, compaction.identity.run_id)?,
                load_run(connection, new_id)?,
                connection.query_row(
                    "SELECT active_run_id, preparing_run_id
                         FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(active.status, RunStatus::Running);
    assert_eq!(active.outcome, None);
    assert_eq!(newer.status, RunStatus::Queued);
    assert_eq!(newer.outcome, None);
    assert_eq!(
        session_state.0,
        Some(compaction.identity.run_id.to_string())
    );
    assert_eq!(session_state.1, Some(new_id.to_string()));

    store
        .finish_run(
            &compaction,
            execution::internal_failure("test cleanup"),
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .finish_reserved_run(&new, RunOutcome::Cancelled)
        .await
        .unwrap();
}

#[tokio::test]
async fn recovery_ignores_legacy_overflow_evidence_without_spending_a_second_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let queued = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("known provider overflow".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let original = store.reserve_next_run(false).await.unwrap().unwrap();
    let audit = test_prepared_audit(&original);
    // Version 17 stored only a resolved model here. It lacks the exact
    // static-prefix/request-byte basis required by version 18 and must
    // not suppress a provider request after recovery.
    let mut legacy_model = test_resolved_model("test/model", "test-model", 256, None);
    legacy_model.version = qq_protocol::ResolvedModelVersion::new(1).unwrap();
    legacy_model.request_shape = None;
    let pending = serde_json::to_string(&legacy_model).unwrap();
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE sessions
                     SET pending_context_overflow_model_json = ?2
                     WHERE id = ?1",
                params![session_id.to_string(), pending],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (compaction, started) = store
        .start_auto_compaction(&original, audit, None)
        .await
        .unwrap()
        .unwrap();
    store.close().await.unwrap();
    drop(store);

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: started.cursor,
        })
        .unwrap();
    let observed = collect_until(&mut events, finished_for(run_id)).await;

    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Interrupted,
            ..
        } if finished == compaction.identity.run_id
    )));
    assert!(
        observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Completed,
                ..
            } if finished == run_id
        )),
        "unknown legacy overflow evidence must fall back to provider execution"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let (attempted, auto_runs, preparing, legacy_pending, basis_pending): (
        bool,
        u32,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT r.context_compaction_attempted,
                            (SELECT COUNT(*) FROM runs c
                             WHERE c.auto_compaction_for_run_id = r.id),
                            s.preparing_run_id,
                            s.pending_context_overflow_model_json,
                            s.pending_context_overflow_basis_json
                     FROM runs r JOIN sessions s ON s.id = r.session_id
                     WHERE r.id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert!(attempted);
    assert_eq!(auto_runs, 1);
    assert_eq!(preparing, None);
    assert!(legacy_pending.is_some());
    assert_eq!(basis_pending, None);
}

#[tokio::test]
async fn version_sixteen_active_auto_compaction_backfills_exact_attempt_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
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
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let queued = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("resume after legacy crash".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let original = store.reserve_next_run(false).await.unwrap().unwrap();
    let (compaction, started) = store
        .start_auto_compaction(&original, test_prepared_audit(&original), None)
        .await
        .unwrap()
        .unwrap();
    store.close().await.unwrap();
    drop(store);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '16' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute("ALTER TABLE sessions DROP COLUMN preparing_run_id", [])
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN pending_context_overflow_model_json",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs DROP COLUMN context_compaction_attempted",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs DROP COLUMN auto_compaction_for_run_id",
            [],
        )
        .unwrap();
    drop(connection);

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: started.cursor,
        })
        .unwrap();
    let observed = collect_until(&mut events, finished_for(run_id)).await;

    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Interrupted,
            ..
        } if finished == compaction.identity.run_id
    )));
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        } if finished == run_id
    )));
    assert!(
        observed
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let (attempted, auto_runs, owner): (bool, u32, Option<String>) = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT original.context_compaction_attempted,
                            (SELECT COUNT(*) FROM runs auto
                             WHERE auto.auto_compaction = 1
                               AND auto.session_id = original.session_id),
                            auto.auto_compaction_for_run_id
                     FROM runs original JOIN runs auto ON auto.id = ?2
                     WHERE original.id = ?1",
                    params![run_id.to_string(), compaction.identity.run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert!(attempted);
    assert_eq!(auto_runs, 1);
    assert_eq!(owner.as_deref(), Some(run_id.to_string().as_str()));
}

#[tokio::test]
async fn slow_preparation_does_not_serialize_an_independent_permit() {
    struct FirstPreparationPausedLoader {
        loads: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for FirstPreparationPausedLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let first = self.loads.fetch_add(1, Ordering::SeqCst) == 0;
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                if first {
                    entered.notify_one();
                    release.notified().await;
                }
                struct CountingProvider(Arc<AtomicUsize>);

                impl Provider for CountingProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                            usage: None,
                        })]))
                    }
                }

                Runtime::new(CountingProvider(provider_calls), "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let loads = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 2;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(FirstPreparationPausedLoader {
            loads: Arc::clone(&loads),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let first = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: first_session,
    } = first.outcome
    else {
        panic!("unexpected receipt")
    };
    let second = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: second_session,
    } = second.outcome
    else {
        panic!("unexpected receipt")
    };
    let first = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: first_session,
                input: vec![InputPart::text("first".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: first_run, ..
    } = first.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let second = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: second_session,
                input: vec![InputPart::text("second".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: second_run, ..
    } = second.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: second.receipt.committed_through,
        })
        .unwrap();
    runtime.request_schedule();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("one loader must remain in preparation");
    let observed = collect_until(&mut events, |event| {
        matches!(event, SessionEvent::RunFinished { .. })
    })
    .await;
    let completed = observed
        .iter()
        .find_map(|event| match event.event {
            SessionEvent::RunFinished { run_id, .. } => Some(run_id),
            _ => None,
        })
        .unwrap();
    let blocked = if completed == first_run {
        second_run
    } else {
        assert_eq!(completed, second_run);
        first_run
    };
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(observed.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunStarted { run_id, .. } if run_id == blocked
    )));

    release.notify_one();
    let observed = collect_until(&mut events, finished_for(blocked)).await;
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Completed,
            ..
        } if run_id == blocked
    )));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn blocked_preparation_stays_queued_and_shutdown_waits_for_its_permit() {
    struct PausedPreparationLoader {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        loads: Arc<AtomicUsize>,
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for PausedPreparationLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            let release = Arc::clone(&self.release);
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                release.notified().await;
                struct CountingProvider(Arc<AtomicUsize>);

                impl Provider for CountingProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                            usage: None,
                        })]))
                    }
                }

                Runtime::new(CountingProvider(provider_calls), "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let loads = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 1;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(PausedPreparationLoader {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            loads: Arc::clone(&loads),
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("block during load".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("the reserved run must enter preparation");
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: queued.committed_through,
        })
        .unwrap();

    let second = queue_prompt(&runtime, session_id, "same session".to_owned()).await;
    assert!(matches!(
        events.next().await.unwrap().unwrap().event,
        SessionEvent::PromptQueued { run, .. } if run.id == second
    ));
    tokio::task::yield_now().await;
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 4,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.summary.status, SessionStatus::Queued);
    assert_eq!(focused.summary.active_run_id, None);
    assert_eq!(focused.summary.queued_prompts, 2);
    assert!(
        focused
            .runs
            .iter()
            .all(|run| run.status == RunStatus::Queued)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession { session_id },
            )
            .await
            .err(),
        Some(SessionRuntimeError::SessionActive)
    );

    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    let observed = collect_until(&mut events, finished_for(run_id)).await;
    assert!(observed.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunStarted { run_id: started, .. } if started == run_id
    )));

    let mut shutdown = Box::pin(runtime.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
            .await
            .is_err()
    );
    release.notify_waiters();
    tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("shutdown must finish after preparation exits")
        .unwrap();
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(
        runtime
            .inner
            .store
            .unfinished_run_ids()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(runtime.inner.permits.available_permits(), 1);
    for pool in &runtime.inner.child_permits {
        assert_eq!(pool.available_permits(), 1);
    }
    let preparing: Option<String> = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT preparing_run_id FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(preparing, None);
    let second = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_run(connection, second)
        })
        .await
        .unwrap();
    assert_eq!(second.outcome, Some(RunOutcome::Cancelled));
}

#[tokio::test]
async fn chunks_large_deltas_and_ignores_empty_deltas() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ChunkingLoader),
    )
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
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("large".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let mut chunks = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            match event.unwrap().event {
                SessionEvent::TextAppended { text, .. } => chunks.push(text),
                SessionEvent::RunFinished { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.len() <= MAX_TEXT_CHUNK_BYTES)
    );
    assert_eq!(chunks.concat(), "é".repeat(MAX_TEXT_CHUNK_BYTES / 2 + 8));
}

#[tokio::test]
async fn rejects_cross_workspace_focus_and_oversized_pages() {
    let (directory, runtime) = test_runtime().await;
    let second = tempfile::tempdir().unwrap();
    let (first_workspace, _) = resolve_workspace(&runtime, directory.path()).await;
    let (second_workspace, _) = resolve_workspace(&runtime, second.path()).await;
    let created = create_session(&runtime, second_workspace, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };

    assert_eq!(
        runtime
            .snapshot(SnapshotRequest {
                workspace_id: first_workspace,
                focused_session_id: Some(session_id),
                include_sessions: Vec::new(),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap_err(),
        SessionRuntimeError::SessionNotFound
    );
    assert_eq!(
        runtime
            .snapshot(SnapshotRequest {
                workspace_id: first_workspace,
                focused_session_id: None,
                include_sessions: Vec::new(),
                session_limit: MAX_SNAPSHOT_SESSIONS + 1,
                message_limit: 1,
            })
            .await
            .unwrap_err(),
        SessionRuntimeError::InvalidPageLimit
    );
}

#[tokio::test]
async fn schedules_ready_sessions_fairly() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions {
            database_path: directory.path().join("sessions.sqlite3"),
            max_active_runs: 1,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            grant_authority: None,
            approval_reviewer: None,
        },
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let first = create_session(&runtime, workspace_id, None).await;
    let second = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: first_session,
    } = first.outcome
    else {
        panic!("unexpected receipt")
    };
    let CommandOutcome::SessionCreated {
        session_id: second_session,
    } = second.outcome
    else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: second.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: first_session,
                input: vec![InputPart::text("first-a".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap().event, SessionEvent::RunStarted { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    for (session_id, prompt) in [(first_session, "first-b"), (second_session, "second-a")] {
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
    }
    let mut finished = 0;
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap().event, SessionEvent::RunFinished { .. }) {
                finished += 1;
                if finished == 3 {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 3);
    assert_eq!(
        captured[0].messages().last(),
        Some(&Message::user("first-a"))
    );
    assert_eq!(
        captured[1].messages().last(),
        Some(&Message::user("second-a"))
    );
    assert_eq!(
        captured[2].messages().last(),
        Some(&Message::user("first-b"))
    );
}
