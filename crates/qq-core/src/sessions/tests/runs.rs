use super::*;

#[tokio::test]
async fn creates_root_and_child_sessions_in_one_workspace_snapshot() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let root = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: root_id,
    } = root.outcome
    else {
        panic!("unexpected receipt")
    };
    let child = create_session(&runtime, workspace_id, Some(root_id)).await;
    let CommandOutcome::SessionCreated {
        session_id: child_id,
    } = child.outcome
    else {
        panic!("unexpected receipt")
    };

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(child_id),
            include_sessions: Vec::new(),
            session_limit: 32,
            message_limit: 32,
        })
        .await
        .unwrap();

    assert_eq!(snapshot.sessions.len(), 2);
    assert_eq!(snapshot.focused.unwrap().summary.parent_id, Some(root_id));
}

#[tokio::test]
async fn snapshots_include_extra_session_bodies_without_evicting_the_focus() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("unexpected receipt")
        };
        ids.push(session_id);
    }
    let (other_directory, _) = (tempfile::tempdir().unwrap(), ());
    let (other_workspace, _) = resolve_workspace(&runtime, other_directory.path()).await;
    let CommandOutcome::SessionCreated {
        session_id: foreign,
    } = create_session(&runtime, other_workspace, None)
        .await
        .outcome
    else {
        panic!("unexpected receipt")
    };
    let missing = SessionId::from_bytes([0xee; 16]);

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(ids[0]),
            // The focused id is skipped in `included`; foreign and unknown
            // sessions are dropped rather than failing the request.
            include_sessions: vec![ids[2], ids[0], foreign, missing, ids[1]],
            session_limit: 8,
            message_limit: 8,
        })
        .await
        .unwrap();
    assert_eq!(snapshot.focused.as_ref().unwrap().summary.id, ids[0]);
    assert_eq!(
        snapshot
            .included
            .iter()
            .map(|body| body.summary.id)
            .collect::<Vec<_>>(),
        vec![ids[2], ids[1]]
    );

    let too_many = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: vec![ids[0]; qq_protocol::MAX_INCLUDED_SESSIONS + 1],
            session_limit: 8,
            message_limit: 8,
        })
        .await;
    assert!(matches!(
        too_many,
        Err(SessionRuntimeError::InvalidPageLimit)
    ));
}

#[tokio::test]
async fn only_the_first_prompt_names_a_session() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };

    for prompt in ["New session", "Do not replace the first title"] {
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
    assert_eq!(snapshot.focused.unwrap().summary.title, "New session");
}

#[tokio::test]
async fn retries_return_the_original_durable_receipt() {
    let (directory, runtime) = test_runtime().await;
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::ResolveWorkspace {
        path: directory.path().to_str().unwrap().to_owned(),
    };

    let first = runtime.command(command_id, command.clone()).await.unwrap();
    let retry = runtime.command(command_id, command).await.unwrap();

    assert_eq!(retry, first);
    assert_eq!(
        runtime
            .command(
                command_id,
                SessionCommand::ResolveWorkspace {
                    path: "/different".to_owned(),
                },
            )
            .await
            .unwrap_err(),
        SessionRuntimeError::IdempotencyConflict
    );
}

#[tokio::test]
async fn reasoning_replays_without_entering_transcript_or_model_context() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ReasoningLoader {
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
    let after_creation = created.committed_through;
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: after_creation,
        })
        .unwrap();

    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("first prompt".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let observed = collect_through_finished(&mut events).await;
    let reasoning = observed
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::ReasoningStarted { kind, .. } => Some(("started", *kind, None)),
            SessionEvent::ReasoningDelta { kind, text, .. } => {
                Some(("delta", *kind, Some(text.as_str())))
            }
            SessionEvent::ReasoningCompleted { kind, .. } => Some(("completed", *kind, None)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reasoning.first().unwrap().0, "started");
    assert_eq!(reasoning.last().unwrap().0, "completed");
    let reasoning_deltas = reasoning
        .iter()
        .filter_map(|(event, kind, text)| {
            if *event == "delta" {
                Some((*kind, text.unwrap()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(reasoning_deltas.len(), 3);
    assert!(
        reasoning_deltas[..2]
            .iter()
            .all(|(kind, _)| *kind == qq_provider::ReasoningKind::Summary)
    );
    assert_eq!(
        reasoning_deltas[..2]
            .iter()
            .map(|(_, text)| *text)
            .collect::<String>(),
        "private rationale ".repeat(64)
    );
    assert_eq!(
        reasoning_deltas[2],
        (
            qq_provider::ReasoningKind::ExposedThinking,
            "late rationale"
        )
    );
    let buffered_text = observed
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::TextAppended { text, .. } if text == "wer"))
        .unwrap();
    let later_reasoning = observed
        .iter()
        .position(|event| {
            matches!(
                event.event,
                SessionEvent::ReasoningStarted {
                    kind: qq_provider::ReasoningKind::ExposedThinking,
                    ..
                }
            )
        })
        .unwrap();
    assert!(buffered_text < later_reasoning);

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
    let messages = snapshot.focused.unwrap().messages;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].output, "first prompt");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].output, "answer");
    assert!(
        messages
            .iter()
            .all(|message| !message.output.contains("private rationale"))
    );

    // A fresh subscription reads the same reasoning transitions from the
    // durable event log, in the same positions as the live subscriber.
    let mut replay = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: after_creation,
        })
        .unwrap();
    let replayed = tokio::time::timeout(Duration::from_secs(2), async {
        let mut replayed = Vec::new();
        for _ in 0..observed.len() {
            replayed.push(replay.next().await.unwrap().unwrap());
        }
        replayed
    })
    .await
    .unwrap();
    assert_eq!(replayed, observed);

    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("second prompt".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let next_context = request_texts(&requests[1]);
    assert!(next_context.iter().any(|text| text == "answer"));
    assert!(next_context.iter().any(|text| text == "second prompt"));
    assert!(
        next_context
            .iter()
            .all(|text| !text.contains("private rationale"))
    );
}

#[tokio::test]
async fn cancellation_flushes_the_final_bounded_reasoning_batch_before_settlement() {
    let directory = tempfile::tempdir().unwrap();
    let buffered = Arc::new(tokio::sync::Notify::new());
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(HangingReasoningLoader {
            buffered: Arc::clone(&buffered),
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
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("reason until cancelled".to_owned())],
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
    buffered.notified().await;
    runtime.inner.cancel(run_id);

    let observed = collect_until(&mut events, finished_for(run_id)).await;
    let reasoning = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reasoning, ["first", "buffered"]);
    let buffered = position_of(
        &observed,
        |event| matches!(event, SessionEvent::ReasoningDelta { text, .. } if text == "buffered"),
    );
    let finished = position_of(&observed, |event| {
        matches!(
            event,
            SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Cancelled,
                ..
            } if *finished == run_id
        )
    });
    assert!(buffered < finished);
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn streams_committed_run_events_and_snapshots_the_result() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, initial) = resolve_workspace(&runtime, directory.path()).await;
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
                input: vec![InputPart::text("Say hello".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let finished = matches!(event.event, SessionEvent::RunFinished { .. });
            observed.push(event);
            if finished {
                break;
            }
        }
    })
    .await
    .unwrap();

    assert!(matches!(
        observed[0].event,
        SessionEvent::PromptQueued { .. }
    ));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::TextAppended { .. }))
            .count(),
        2
    );
    assert!(
        observed
            .windows(2)
            .all(|events| { events[1].cursor.sequence == events[0].cursor.sequence + 1 })
    );
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            session,
            usage: Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
                ..
            }),
            context_tokens: Some(13),
            ..
        } if session.context_tokens == Some(13)
    ));
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunContextUpdated {
            context_tokens: 13,
            ..
        }
    )));
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::SessionContextUpdated {
            context_tokens: Some(13),
            ..
        }
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ModelTurnCompleted {
            turn_ordinal: 1,
            model: ModelSelection {
                model_is_fallback: false,
                model: Some(model),
                max_output_tokens: Some(256),
                organization: None,
            },
            usage: Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
                ..
            }),
            estimated_cost_usd_nanos: Some(20_500),
            ..
        } if model == "test/model"
    )));
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 32,
            message_limit: 32,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.messages.len(), 2);
    assert_eq!(focused.messages[1].output, "hello");
    assert_eq!(focused.summary.status, SessionStatus::Idle);
    assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
    assert_eq!(focused.summary.context_tokens, Some(13));
    assert_eq!(focused.summary.estimated_cost_usd_nanos, Some(20_500));
    assert_eq!(
        focused.runs[0].usage,
        Some(TokenUsage {
            input_tokens: 10,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 5,
            reasoning_tokens: None,
        })
    );
    assert_eq!(focused.runs[0].context_tokens, Some(13));
    assert_eq!(focused.runs[0].estimated_cost_usd_nanos, Some(20_500));
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let (model, usage, cost, completed): (String, String, u64, u64) = connection
        .query_row(
            "SELECT model_json, usage_json, estimated_cost_usd_nanos, completed_at_ms
             FROM model_turns",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<ModelSelection>(&model)
            .unwrap()
            .model
            .as_deref(),
        Some("test/model")
    );
    assert_eq!(
        serde_json::from_str::<TokenUsage>(&usage).unwrap(),
        TokenUsage {
            input_tokens: 10,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 5,
            reasoning_tokens: None,
        }
    );
    assert_eq!(cost, 20_500);
    assert!(completed > 0);
    let (assistant_message, legacy_output, legacy_refusal, chunk_count): (
        String,
        String,
        String,
        u64,
    ) = connection
        .query_row(
            "SELECT m.id, m.output, m.refusal, COUNT(c.chunk_ordinal)
             FROM messages m
             LEFT JOIN message_chunks c ON c.message_id = m.id
             WHERE m.role = 'assistant'
             GROUP BY m.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(legacy_output, "");
    assert_eq!(legacy_refusal, "");
    assert_eq!(chunk_count, 2);
    let mut statement = connection
        .prepare(
            "SELECT text FROM message_chunks
             WHERE message_id = ?1 AND channel = 'output'
             ORDER BY chunk_ordinal",
        )
        .unwrap();
    let chunks = statement
        .query_map([assistant_message], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(statement);
    assert_eq!(chunks, ["hel", "lo"]);
    let (base, increment): (u64, u64) = connection
        .query_row(
            "SELECT context_base_bytes, context_increment_bytes FROM runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(base > "Say hello".len() as u64);
    assert_eq!(
        increment,
        crate::CONTEXT_MESSAGE_FRAMING_BYTES
            + crate::CONTEXT_BLOCK_FRAMING_BYTES
            + "hello".len() as u64
    );
    assert!(base + increment <= MAX_CONTEXT_BYTES as u64);
    assert!(snapshot.cursor.sequence > initial.sequence);
}

#[tokio::test]
async fn unmeasured_new_prompt_clears_stale_session_context() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(UsageSequenceLoader {
            usages: StdMutex::new(vec![
                Some(qq_provider::ProviderUsage {
                    input_tokens: 40_000,
                    cache_read_input_tokens: 12_000,
                    cache_write_input_tokens: 2_400,
                    output_tokens: 1,
                    reasoning_tokens: None,
                }),
                None,
            ]),
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

    let first_run = queue_prompt(&runtime, session_id, "measured".to_owned()).await;
    let first = collect_through_finished(&mut events).await;
    assert!(first.iter().any(|event| matches!(
        event.event,
        SessionEvent::SessionContextUpdated {
            run_id,
            context_tokens: Some(54_400),
        } if run_id == first_run
    )));

    let second_run = queue_prompt(&runtime, session_id, "unmeasured".to_owned()).await;
    let second = collect_through_finished(&mut events).await;
    assert!(second.iter().any(|event| matches!(
        event.event,
        SessionEvent::SessionContextUpdated {
            run_id,
            context_tokens: None,
        } if run_id == second_run
    )));
    assert!(second.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunContextUpdated { run_id, .. } if run_id == second_run
    )));
    assert!(second.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            session,
            run_id,
            usage: None,
            context_tokens: None,
            ..
        } if *run_id == second_run && session.context_tokens.is_none()
    )));

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
    assert_eq!(focused.summary.context_tokens, None);
    assert_eq!(focused.runs.len(), 2);
    assert_eq!(focused.runs[0].context_tokens, Some(54_400));
    assert_eq!(focused.runs[1].context_tokens, None);
    let stored_basis = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT context_occupancy_json FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(stored_basis, None);
}

#[tokio::test]
async fn cancellation_before_a_model_turn_preserves_known_session_cost() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(PricedHangingLoader),
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

    let run_id = queue_prompt(&runtime, session_id, "cancel".to_owned()).await;
    collect_until(&mut events, |event| {
        matches!(event, SessionEvent::RunStarted { run_id: started, .. } if *started == run_id)
    })
    .await;
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    let observed = collect_until(&mut events, finished_for(run_id)).await;

    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            session,
            outcome: RunOutcome::Cancelled,
            ..
        } if session.estimated_cost_usd_nanos == Some(0)
    )));
}

#[tokio::test]
async fn persists_tool_transitions_and_reconstructs_follow_up_context() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ToolLoopLoader {
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
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("inspect the note".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let observed = collect_through_finished(&mut events).await;
    let context_updates = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::SessionContextUpdated {
                context_tokens: Some(context_tokens),
                ..
            } => Some(*context_tokens),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(context_updates, [4, 6]);
    let requested = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
        .unwrap();
    let started = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        .unwrap();
    let finished = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
        .unwrap();
    assert!(requested < started && started < finished);

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
    assert_eq!(focused.tool_calls.len(), 1);
    assert_eq!(focused.tool_calls[0].state, ToolCallState::Completed);
    let result = focused.tool_calls[0].result.as_deref().unwrap();
    assert!(result.starts_with("read note.txt L1/1 h:"), "{result}");
    assert!(result.ends_with("\n1\ttool result\n"), "{result}");
    assert_eq!(
        focused.runs[0].usage,
        Some(TokenUsage {
            input_tokens: 10,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 3,
            reasoning_tokens: None,
        })
    );
    assert_eq!(focused.runs[0].context_tokens, Some(6));
    assert_eq!(focused.summary.context_tokens, Some(6));
    let completed_run = focused.runs[0].id;
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let (base, increment): (u64, u64) = connection
        .query_row(
            "SELECT context_base_bytes, context_increment_bytes
             FROM runs WHERE id = ?1",
            [completed_run.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(base > "inspect the note".len() as u64);
    assert!(increment > 0);
    assert!(base + increment <= MAX_CONTEXT_BYTES as u64);

    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("what did you read?".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let _ = collect_through_finished(&mut events).await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].messages().len(), 5);
    assert!(matches!(
        requests[2].messages()[1].content(),
        [ContentBlock::ToolCall { id, .. }] if id == "call_0"
    ));
    assert!(matches!(
        requests[2].messages()[2].content(),
        [ContentBlock::ToolResult { call_id, content, .. }]
            if call_id == "call_0" && content.ends_with("\n1\ttool result\n")
    ));
}

#[tokio::test]
async fn one_durable_run_continues_across_the_internal_tool_budget() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
    std::fs::write(directory.path().join("slice-effects.txt"), "seed").unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        Arc::new(RenewableSliceLoader {
            requests: Arc::clone(&requests),
            checkpoint_wait: None,
            metered_empty_checkpoint: false,
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = queue_prompt(&runtime, session_id, "finish a long task".to_owned()).await;

    let observed = collect_until(&mut events, finished_for(run_id)).await;
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event.event,
                SessionEvent::RunStarted { run_id: started, .. } if started == run_id
            ))
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event.event,
                SessionEvent::RunFinished { run_id: finished, .. } if finished == run_id
            ))
            .count(),
        1
    );
    assert!(matches!(
        observed.last().map(|event| &event.event),
        Some(SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        }) if *finished == run_id
    ));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
            .count(),
        crate::MAX_TOOL_CALLS_PER_SLICE
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
            .count(),
        crate::MAX_TOOL_CALLS_PER_SLICE
    );

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 16,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.runs.len(), 1);
    assert_eq!(focused.runs[0].outcome, Some(RunOutcome::Completed));
    assert_eq!(focused.tool_calls.len(), crate::MAX_TOOL_CALLS_PER_SLICE);
    assert!(
        focused
            .tool_calls
            .iter()
            .all(|call| { call.run_id == run_id && call.state == ToolCallState::Completed })
    );
    let mut provider_call_ids = focused
        .tool_calls
        .iter()
        .map(|call| call.provider_call_id.as_str())
        .collect::<Vec<_>>();
    provider_call_ids.sort_unstable();
    provider_call_ids.dedup();
    assert_eq!(provider_call_ids.len(), crate::MAX_TOOL_CALLS_PER_SLICE);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("slice-effects.txt")).unwrap(),
        "seedx",
        "the mutating call before rollover must execute exactly once"
    );
    assert!(focused.messages.iter().any(|message| {
        message.role == MessageRole::Assistant && message.output == "slice checkpoint"
    }));
    assert!(focused.messages.iter().any(|message| {
        message.role == MessageRole::Assistant && message.output == "task complete"
    }));

    {
        let recorded_requests = requests.lock().unwrap();
        let checkpoint = &recorded_requests[recorded_requests.len() - 2];
        let continuation = recorded_requests.last().unwrap();
        assert!(checkpoint.tools().is_empty());
        assert!(!continuation.tools().is_empty());
        assert!(
            continuation
                .system()
                .is_some_and(|system| system.contains(crate::SLICE_CONTINUATION_NOTICE))
        );
        assert!(continuation.messages().iter().any(|message| {
            message.content().iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text } if text == "slice checkpoint"
                )
            })
        }));
    }

    let replay_after = observed.last().unwrap().cursor;
    drop(events);
    drop(runtime);
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(RenewableSliceLoader {
            requests: Arc::clone(&requests),
            checkpoint_wait: None,
            metered_empty_checkpoint: false,
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: replay_after,
        })
        .unwrap();
    let follow_up = queue_prompt(&runtime, session_id, "confirm completion".to_owned()).await;
    let replayed = collect_until(&mut events, finished_for(follow_up)).await;
    assert!(matches!(
        replayed.last().map(|event| &event.event),
        Some(SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        }) if *finished == follow_up
    ));
    let requests = requests.lock().unwrap();
    let replay_request = requests.last().unwrap();
    assert!(replay_request.messages().iter().any(|message| {
        message.content().iter().any(|block| {
            matches!(
                block,
                ContentBlock::Text { text } if text == "slice checkpoint"
            )
        })
    }));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("slice-effects.txt")).unwrap(),
        "seedx",
        "replay and follow-up context must not repeat the mutation"
    );
}

#[tokio::test]
async fn cancellation_at_the_slice_checkpoint_has_one_cancelled_terminal() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
    std::fs::write(directory.path().join("slice-effects.txt"), "seed").unwrap();
    let checkpoint_wait = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(RenewableSliceLoader {
            requests: Arc::clone(&requests),
            checkpoint_wait: Some(Arc::clone(&checkpoint_wait)),
            metered_empty_checkpoint: false,
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = queue_prompt(&runtime, session_id, "finish a long task".to_owned()).await;

    tokio::time::timeout(Duration::from_secs(30), checkpoint_wait.notified())
        .await
        .expect("the run must reach its tool-free checkpoint request");
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    let observed = collect_until(&mut events, finished_for(run_id)).await;

    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event.event,
                SessionEvent::RunFinished { run_id: finished, .. } if finished == run_id
            ))
            .count(),
        1
    );
    assert!(matches!(
        observed.last().map(|event| &event.event),
        Some(SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Cancelled,
            ..
        }) if *finished == run_id
    ));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
            .count(),
        crate::MAX_TOOL_CALLS_PER_SLICE
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
            .count(),
        crate::MAX_TOOL_CALLS_PER_SLICE
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("slice-effects.txt")).unwrap(),
        "seedx"
    );
    let requests = requests.lock().unwrap();
    assert!(requests.last().unwrap().tools().is_empty());
    assert!(!requests.iter().any(|request| {
        request
            .system()
            .is_some_and(|system| system.contains(crate::SLICE_CONTINUATION_NOTICE))
    }));
}

#[tokio::test]
async fn cancellation_after_durable_tool_result_records_unavailable_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(BlockingCheckpointLoader { requests }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = queue_prompt(&runtime, session_id, "inspect the note".to_owned()).await;

    let mut observed = collect_until(&mut events, |event| {
        matches!(event, SessionEvent::ToolCallFinished { .. })
    })
    .await;
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    observed.extend(collect_until(&mut events, finished_for(run_id)).await);

    let finished = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
        .expect("tool result is durable");
    let checkpoint = observed.iter().position(|event| {
        matches!(
            &event.event,
            SessionEvent::CheckpointReviewed {
                outcome: qq_protocol::CheckpointOutcome::Unavailable,
                feedback,
                ..
            } if feedback.contains("verdict was durably recorded") && feedback.contains("cancelled")
        )
    });
    let checkpoint = checkpoint.unwrap_or_else(|| {
        panic!("cancelled unreviewed result has durable checkpoint status; observed={observed:#?}")
    });
    let terminal = observed
        .iter()
        .position(|event| {
            matches!(
                event.event,
                SessionEvent::RunFinished {
                    run_id: finished,
                    outcome: RunOutcome::Cancelled,
                    ..
                } if finished == run_id
            )
        })
        .expect("run is cancelled");
    assert!(finished < checkpoint && checkpoint < terminal);
}

#[tokio::test]
async fn runtime_failure_after_durable_tool_result_records_unavailable_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(BlockingCheckpointLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = queue_prompt(&runtime, session_id, "inspect the note".to_owned()).await;

    let mut observed = collect_until(&mut events, |event| {
        matches!(event, SessionEvent::ToolCallFinished { .. })
    })
    .await;
    let after = observed.last().unwrap().cursor.sequence;
    let mut settlements = runtime.inner.settlements.subscribe();
    runtime.inner.failed.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), settlements.changed())
        .await
        .expect("runtime failure settles promptly")
        .expect("settlement watch remains open");
    observed.extend(
        runtime
            .inner
            .store
            .events_after(workspace_id, after, 16)
            .await
            .unwrap(),
    );

    let tool_finished = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
        .expect("tool result is durable");
    let checkpoint = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::CheckpointReviewed {
                    outcome: qq_protocol::CheckpointOutcome::Unavailable,
                    feedback,
                    ..
                } if feedback.contains("verdict was durably recorded")
                    && feedback.contains("failed")
            )
        })
        .unwrap_or_else(|| {
            panic!("failed run records the unreviewed result; observed={observed:#?}")
        });
    let terminal = observed
        .iter()
        .position(|event| {
            matches!(
                event.event,
                SessionEvent::RunFinished {
                    run_id: finished,
                    outcome: RunOutcome::Failed { .. },
                    ..
                } if finished == run_id
            )
        })
        .expect("runtime failure settles the run");
    assert!(tool_finished < checkpoint && checkpoint < terminal);
    assert_eq!(
        runtime.shutdown().await.unwrap_err(),
        SessionRuntimeError::Unavailable
    );
}

#[tokio::test]
async fn empty_checkpoint_failure_retains_the_billed_turn_accounting() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
    std::fs::write(directory.path().join("slice-effects.txt"), "seed").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(RenewableSliceLoader {
            requests,
            checkpoint_wait: None,
            metered_empty_checkpoint: true,
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = queue_prompt(&runtime, session_id, "finish a long task".to_owned()).await;

    let observed = collect_until(&mut events, finished_for(run_id)).await;
    assert!(matches!(
        observed.last().map(|event| &event.event),
        Some(SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderResponse,
                    message,
                },
            },
            ..
        }) if *finished == run_id && message == "provider returned an empty slice checkpoint"
    ));

    let expected_usage = TokenUsage {
        input_tokens: 19,
        cache_read_input_tokens: 1,
        cache_write_input_tokens: 2,
        output_tokens: 21,
        reasoning_tokens: None,
    };
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 16,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.runs[0].usage, Some(expected_usage));
    assert_eq!(focused.runs[0].estimated_cost_usd_nanos, Some(61_700));
    let accounting = focused.summary.accounting.unwrap();
    assert_eq!(accounting.direct.usage, Some(expected_usage));
    assert_eq!(accounting.direct.estimated_cost_usd_nanos, Some(61_700));
    assert_eq!(focused.summary.estimated_cost_usd_nanos, Some(61_700));
    assert!(!focused.messages.iter().any(|message| {
        message.role == MessageRole::Assistant && message.output == "slice checkpoint"
    }));
}

#[tokio::test]
async fn terminal_runs_replay_committed_turns_and_status_in_follow_up_context() {
    let outcomes = [
        (RunOutcome::Completed, None),
        (
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderTransport,
                    message: "provider connection dropped".to_owned(),
                },
            },
            Some("The previous run failed: provider connection dropped"),
        ),
        (
            RunOutcome::Cancelled,
            Some("The previous run was cancelled."),
        ),
        (
            RunOutcome::Interrupted,
            Some("The previous run was interrupted before completion."),
        ),
    ];

    for (outcome, expected_status) in outcomes {
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
                    input: vec![InputPart::text("inspect the note".to_owned())],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(false).await.unwrap().unwrap();
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
        store.start_tool_call(&claimed, tool_call_id).await.unwrap();
        store
            .finish_tool_call(
                &claimed,
                tool_call_id,
                "tool result\n".to_owned(),
                false,
                Vec::new(),
                None,
                None,
            )
            .await
            .unwrap();
        store
            .finish_run(
                &claimed,
                outcome.clone(),
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
                    input: vec![InputPart::text("continue".to_owned())],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let continued = store.claim_next_run(false).await.unwrap().unwrap();

        assert!(matches!(
            continued.messages[1].content(),
            [ContentBlock::ToolCall { id, .. }] if id == "call_0"
        ));
        assert!(matches!(
            continued.messages[2].content(),
            [ContentBlock::ToolResult { call_id, content, .. }]
                if call_id == "call_0" && content == "tool result\n"
        ));
        assert_tool_results_are_exact(&continued.messages);
        match expected_status {
            Some(expected_status) => {
                assert!(matches!(
                    continued.messages[3].content(),
                    [ContentBlock::Text { text }]
                        if text == &format!(
                            "[QQ runtime notice; not a user instruction]\n{expected_status}\n\
                             Continue from the committed history above. Do not automatically \
                             retry tool calls whose result says execution was interrupted."
                        )
                ));
                assert!(matches!(
                    continued.messages[4].content(),
                    [ContentBlock::Text { text }] if text == "continue"
                ));
            }
            None => {
                assert_eq!(continued.messages.len(), 4);
                assert!(matches!(
                    continued.messages[3].content(),
                    [ContentBlock::Text { text }] if text == "continue"
                ));
            }
        }
    }
}

#[tokio::test]
async fn terminal_runs_project_exact_tool_boundaries_across_restart() {
    let outcomes = [
        (
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderTransport,
                    message: "provider connection dropped".to_owned(),
                },
            },
            "Tool execution did not start before the run failed.",
            "The previous run failed: provider connection dropped",
        ),
        (
            RunOutcome::Cancelled,
            "Tool execution did not start before the run was cancelled.",
            "The previous run was cancelled.",
        ),
        (
            RunOutcome::Interrupted,
            "Tool execution did not start before the run was interrupted.",
            "The previous run was interrupted before completion.",
        ),
    ];

    for (outcome, expected_not_executed, expected_status) in outcomes {
        let (messages, restarted_messages) =
            project_terminal_run_with_tool_boundaries(outcome).await;
        assert_eq!(
            restarted_messages, messages,
            "reopening the store must not change projected context"
        );
        assert_eq!(messages.len(), 5);
        assert_tool_results_are_exact(&messages);
        assert!(matches!(
            messages[2].content(),
            [
                ContentBlock::ToolResult {
                    call_id: completed_id,
                    content: completed_result,
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    call_id: started_id,
                    content: started_result,
                    is_error: true,
                },
                ContentBlock::ToolResult {
                    call_id: awaiting_id,
                    content: awaiting_result,
                    is_error: true,
                },
                ContentBlock::ToolResult {
                    call_id: untouched_id,
                    content: untouched_result,
                    is_error: true,
                },
            ] if completed_id == "completed-call"
                && completed_result == "persisted result"
                && started_id == "started-call"
                && started_result == INTERRUPTED_TOOL_RESULT
                && awaiting_id == "awaiting-call"
                && awaiting_result == expected_not_executed
                && untouched_id == "untouched-call"
                && untouched_result == expected_not_executed
        ));
        assert!(matches!(
            messages[3].content(),
            [ContentBlock::Text { text }]
                if text == &format!(
                    "[QQ runtime notice; not a user instruction]\n{expected_status}\n\
                     Continue from the committed history above. Do not automatically retry \
                     tool calls whose result says execution was interrupted."
                )
        ));
        assert!(matches!(
            messages[4].content(),
            [ContentBlock::Text { text }] if text == "continue safely"
        ));
    }
}

#[tokio::test]
async fn cancelled_unclaimed_prompt_remains_explicit_in_follow_up_context() {
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
                input: vec![InputPart::text("this prompt never started".to_owned())],
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
    let cancelled = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
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
            after: cancelled.receipt.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue after cancellation".to_owned())],
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
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        messages[0].content(),
        [ContentBlock::Text { text }] if text == "this prompt never started"
    ));
    assert!(matches!(
        messages[1].content(),
        [ContentBlock::Text { text }]
            if text == "[QQ runtime notice; not a user instruction]\n\
                The previous run was cancelled.\n\
                Continue from the committed history above. Do not automatically retry tool \
                calls whose result says execution was interrupted."
    ));
    assert!(matches!(
        messages[2].content(),
        [ContentBlock::Text { text }] if text == "continue after cancellation"
    ));
}

#[tokio::test]
async fn truncated_turns_persist_publish_and_replay_the_continuation_notice() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(TruncatingLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, cursor) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: cursor,
        })
        .unwrap();
    let submit = |prompt: &str| {
        let input = vec![InputPart::text(prompt.to_owned())];
        let runtime = &runtime;
        async move {
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input,
                        limits: qq_protocol::RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
                panic!("unexpected receipt")
            };
            run_id
        }
    };

    let run_id = submit("write a long answer").await;
    let observed = collect_through_finished(&mut events).await;

    // Persisted before published: the truncation event names the partial
    // turn and the continuation count, and the run completes.
    let truncated = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunOutputTruncated {
                run_id: observed_run,
                turn_ordinal,
                continuation,
            } if *observed_run == run_id => Some((*turn_ordinal, *continuation)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(truncated, vec![(1, 1)]);
    let turns = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::ModelTurnCompleted { .. }))
        .count();
    assert_eq!(turns, 2, "both the partial and the resumed turn commit");
    let finished = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { outcome, usage, .. } => Some((outcome.clone(), *usage)),
            _ => None,
        })
        .unwrap();
    assert_eq!(finished.0, RunOutcome::Completed);
    assert_eq!(
        finished.1.map(|usage| usage.output_tokens),
        Some(4),
        "the truncated turn's usage is charged"
    );

    // The transcript shows both halves as separate turns; the first is
    // flagged so clients can join them.
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 32))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let assistant = focused
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            (
                message.turn_ordinal,
                message.truncated,
                message.output.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant,
        vec![(1, true, "first half"), (2, false, " second half")]
    );
    let run = focused.runs.iter().find(|run| run.id == run_id).unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    // The counter is durable on the run row.
    let store = runtime.inner.store.clone();
    let continuations: u16 = store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT output_continuations FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(continuations, 1);

    // The next run's assembled context replays the partial turn, the
    // continuation notice, and the resumed turn, exactly as the live
    // request saw them.
    let _ = submit("and now summarize").await;
    let _ = collect_through_finished(&mut events).await;
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    let replayed = requests[2]
        .messages()
        .iter()
        .map(|message| {
            let text = match message.content().first() {
                Some(ContentBlock::Text { text }) => text.as_str(),
                _ => "",
            };
            (message.role(), text)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replayed,
        vec![
            (Role::User, "write a long answer"),
            (Role::Assistant, "first half"),
            (Role::User, crate::OUTPUT_TRUNCATED_CONTINUE_NOTICE),
            (Role::Assistant, " second half"),
            (Role::User, "and now summarize"),
        ]
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_uncommitted_assistant_text_stays_out_of_model_context() {
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
                input: vec![InputPart::text("begin the task".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    store
        .begin_assistant_message(
            &claimed,
            MessageId::generate().unwrap(),
            1,
            TextChannel::Output,
            "partial text from an uncommitted turn".to_owned(),
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
    let recovered = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    assert!(recovered.focused.unwrap().messages.iter().any(|message| {
        message.state == MessageState::Interrupted
            && message.output == "partial text from an uncommitted turn"
    }));
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: recovered.cursor,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue from durable work".to_owned())],
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
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        messages[0].content(),
        [ContentBlock::Text { text }] if text == "begin the task"
    ));
    assert!(matches!(
        messages[1].content(),
        [ContentBlock::Text { text }]
            if text == "[QQ runtime notice; not a user instruction]\n\
                The previous run was interrupted before completion.\n\
                Continue from the committed history above. Do not automatically retry tool \
                calls whose result says execution was interrupted."
    ));
    assert!(matches!(
        messages[2].content(),
        [ContentBlock::Text { text }] if text == "continue from durable work"
    ));
}

#[tokio::test]
async fn historical_flat_assistant_output_precedes_the_terminal_notice() {
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
                input: vec![InputPart::text("legacy prompt".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    store
        .begin_assistant_message(
            &claimed,
            MessageId::generate().unwrap(),
            1,
            TextChannel::Output,
            "legacy committed answer".to_owned(),
        )
        .await
        .unwrap();
    let finished = store
        .finish_run(
            &claimed,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderTransport,
                    message: "legacy provider failed".to_owned(),
                },
            },
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let after = finished.last().unwrap().cursor;
    store.close().await.unwrap();
    drop(store);

    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute(
            "UPDATE messages SET state = 'complete'\
             WHERE session_id = ?1 AND role = 'assistant'",
            [session_id.to_string()],
        )
        .unwrap();
    drop(connection);

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
            after,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue from the legacy store".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let _ = collect_through_finished(&mut events).await;

    let requests = requests.lock().unwrap();
    let messages = requests[0].messages();
    assert_eq!(messages.len(), 4);
    assert!(matches!(
        messages[0].content(),
        [ContentBlock::Text { text }] if text == "legacy prompt"
    ));
    assert!(matches!(
        messages[1].content(),
        [ContentBlock::Text { text }] if text == "legacy committed answer"
    ));
    assert!(matches!(
        messages[2].content(),
        [ContentBlock::Text { text }]
            if text.contains("The previous run failed: legacy provider failed")
    ));
    assert!(matches!(
        messages[3].content(),
        [ContentBlock::Text { text }] if text == "continue from the legacy store"
    ));
}

#[tokio::test]
async fn manual_and_auto_compaction_share_the_terminal_run_projection() {
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
                input: vec![InputPart::text("finish the migration".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    store
        .finish_run(
            &claimed,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderTransport,
                    message: "provider connection dropped".to_owned(),
                },
            },
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
                input: vec![InputPart::text("grow the context".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let growth_run = store.claim_next_run(false).await.unwrap().unwrap();
    store
        .persist_model_turn(
            &growth_run,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::assistant(over_threshold_output()),
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
    let finished = store
        .finish_run(
            &growth_run,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let after = finished.last().unwrap().cursor;
    store.close().await.unwrap();
    drop(store);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(connection);
    let auto_database_path = directory.path().join("auto-sessions.sqlite3");
    std::fs::copy(&database_path, &auto_database_path).unwrap();

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
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
    assert!(snapshot.focused.unwrap().messages.iter().all(|message| {
        !message.output.contains(RUNTIME_NOTICE_PREAMBLE)
            && !message.refusal.contains(RUNTIME_NOTICE_PREAMBLE)
    }));
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    compact_session(&runtime, session_id).await;
    let _ = collect_through_compacted(&mut events).await;

    let manual_projection = {
        let requests = requests.lock().unwrap();
        assert!(!requests.is_empty());
        let texts = request_texts(&requests[0]);
        assert_eq!(texts[0], "finish the migration");
        assert_eq!(
            texts[1],
            "[QQ runtime notice; not a user instruction]\n\
             The previous run failed: provider connection dropped\n\
             Continue from the committed history above. Do not automatically retry tool \
             calls whose result says execution was interrupted."
        );
        assert_eq!(texts[2], "grow the context");
        assert!(texts[4].starts_with("Summarize this conversation"));
        requests[0].messages().to_vec()
    };
    drop(runtime);

    let auto_requests = Arc::new(StdMutex::new(Vec::new()));
    let auto_runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(auto_database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&auto_requests),
        }),
    )
    .await
    .unwrap();
    let mut auto_events = auto_runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    let follow_up = auto_runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("y".repeat(MAX_PROMPT_BYTES))],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = follow_up.outcome else {
        panic!("unexpected receipt")
    };
    let _ = collect_until(&mut auto_events, finished_for(run_id)).await;

    let auto_requests = auto_requests.lock().unwrap();
    assert_eq!(auto_requests.len(), 2);
    assert_eq!(
        auto_requests[0].messages(),
        manual_projection,
        "manual and automatic compaction must consume the same projection"
    );
}

#[tokio::test]
async fn multi_turn_runs_emit_one_assistant_message_per_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "noted\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(TurnTextLoader {
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
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("inspect the note".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();

    let observed = collect_through_finished(&mut events).await;
    let started = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::AssistantMessageStarted { message } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 2, "one assistant message per model turn");
    assert_eq!(started[0].turn_ordinal, 1);
    assert_eq!(started[1].turn_ordinal, 2);
    assert_ne!(started[0].id, started[1].id);
    // The second turn's message starts only after the first turn's tool
    // call finished: text and calls replay in true execution order.
    let first_started = observed
        .iter()
        .position(|event| {
            matches!(&event.event, SessionEvent::AssistantMessageStarted { message }
                if message.id == started[0].id)
        })
        .unwrap();
    let call_requested = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
        .unwrap();
    let call_finished = observed
        .iter()
        .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
        .unwrap();
    let second_started = observed
        .iter()
        .position(|event| {
            matches!(&event.event, SessionEvent::AssistantMessageStarted { message }
                if message.id == started[1].id)
        })
        .unwrap();
    assert!(first_started < call_requested);
    assert!(call_requested < call_finished);
    assert!(call_finished < second_started);

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
    assert_eq!(focused.messages[0].role, MessageRole::User);
    assert_eq!(focused.messages[0].turn_ordinal, 0);
    assert_eq!(focused.messages[1].role, MessageRole::Assistant);
    assert_eq!(focused.messages[1].turn_ordinal, 1);
    assert_eq!(focused.messages[1].output, "Let me look. ");
    assert_eq!(focused.messages[1].state, MessageState::Complete);
    assert_eq!(focused.messages[2].turn_ordinal, 2);
    assert_eq!(focused.messages[2].output, "done");
    assert_eq!(focused.messages[2].state, MessageState::Complete);
    assert_eq!(focused.tool_calls.len(), 1);
    assert_eq!(focused.tool_calls[0].turn_ordinal, 1);
}

#[tokio::test]
async fn call_only_turns_persist_no_message_row() {
    let harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())]],
    )
    .await;
    std::fs::write(harness.workspace_path.join("note.txt"), "noted\n").unwrap();
    let mut harness = harness;
    submit_prompt(&harness, "read the note").await;
    let observed = collect_through_finished(&mut harness.events).await;
    let started = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::AssistantMessageStarted { message } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "a call-only turn must not start an assistant message"
    );
    assert_eq!(started[0].turn_ordinal, 2);

    let (workspace_id, _) = resolve_workspace(&harness.runtime, &harness.workspace_path).await;
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(
        focused.messages.len(),
        2,
        "turn one requested a call without text, so it persists no message row"
    );
    assert_eq!(focused.messages[0].role, MessageRole::User);
    assert_eq!(focused.messages[1].role, MessageRole::Assistant);
    assert_eq!(focused.messages[1].turn_ordinal, 2);
    assert_eq!(focused.messages[1].output, "done");
    assert_eq!(focused.tool_calls[0].turn_ordinal, 1);
}

#[tokio::test]
async fn cancellation_during_tool_and_final_checkpoint_records_unknown_spend() {
    struct Loader(bool);
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            struct Reviewer(bool);
            impl CheckpointReviewer for Reviewer {
                fn reviews_tools(&self) -> bool {
                    self.0
                }
                fn review(&self, _: CheckpointRequest) -> CheckpointFuture {
                    Box::pin(std::future::pending())
                }
            }
            let review_tools = self.0;
            Box::pin(async move {
                let runtime = Runtime::new(
                    ToolLoopProvider {
                        requests: Arc::new(StdMutex::new(Vec::new())),
                    },
                    "test",
                    256,
                )
                .unwrap()
                .with_checkpoint_reviewer(Arc::new(Reviewer(review_tools)));
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    for (review_tools, expected_phase) in [
        (true, qq_protocol::CheckpointPhase::ToolResult),
        (false, qq_protocol::CheckpointPhase::FinalCandidate),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "evidence").unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader(review_tools)),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session");
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let run_id = queue_prompt(&runtime, session_id, "inspect".into()).await;
        let mut observed = collect_until(&mut events, |event| matches!(event, SessionEvent::CheckpointStarted { phase, .. } if *phase == expected_phase)).await;
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();
        observed.extend(collect_until(&mut events, finished_for(run_id)).await);
        assert_eq!(observed.iter().filter(|event| matches!(&event.event, SessionEvent::CheckpointReviewed { phase, spend: Some(spend), outcome: qq_protocol::CheckpointOutcome::Unavailable, .. } if *phase == expected_phase && spend.usage.is_none() && spend.estimated_cost_usd_nanos.is_none())).count(), 1);
        assert!(matches!(
            observed.last().map(|event| &event.event),
            Some(SessionEvent::RunFinished {
                outcome: RunOutcome::Cancelled,
                usage: None,
                ..
            })
        ));
    }
}
