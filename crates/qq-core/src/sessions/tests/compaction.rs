use super::*;

#[tokio::test]
async fn compact_session_is_refused_while_active_and_rejects_undeclared_provider_tools() {
    let mut harness = session_management_harness().await;
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("do work".to_owned())],
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
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;

    // Idle-only: refused with the same error DeleteSession uses while a
    // run is active.
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CompactSession {
                    session_id: harness.session_id,
                },
            )
            .await
            .unwrap_err(),
        SessionRuntimeError::SessionActive
    );

    respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    collect_through_finished(&mut harness.events).await;

    // Idle now: the compaction queues and executes through the ordinary
    // machinery. Its request declares no tools, so a provider tool call
    // is a protocol violation and cannot create a transient second turn.
    let request_count_before = harness.requests.lock().unwrap().len();
    let compaction_run = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderProtocol,
                    ..
                }
            },
            ..
        } if *run_id == compaction_run
    )));
    assert!(
        !observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::PromptQueued { .. }
                | SessionEvent::AssistantMessageStarted { .. }
                | SessionEvent::ToolCallRequested { .. }
                | SessionEvent::ToolApprovalRequested { .. }
        )),
        "an internal run must publish no transcript or tool events"
    );
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), request_count_before + 1);
    assert!(requests.last().unwrap().tools().is_empty());
    // The summarizer loaded through the ordinary loader path.
    assert_eq!(harness.models.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn compaction_runs_account_usage_and_cost_but_join_no_transcript() {
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
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("say hello".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.messages.len(), 2);
    assert_eq!(focused.summary.context_tokens, Some(13));
    let cost_before = focused.summary.estimated_cost_usd_nanos.unwrap();

    let compaction_run = compact_session(&runtime, session_id).await;
    let observed = collect_through_compacted(&mut events).await;

    // Usage and cost account like any run.
    let usage = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { run_id, usage, .. } if *run_id == compaction_run => {
                Some(*usage)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        usage,
        Some(TokenUsage {
            input_tokens: 10,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 5,
            reasoning_tokens: None,
        })
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ModelTurnCompleted {
            run_id,
            turn_ordinal: 1,
            model: ModelSelection { model: Some(model), .. },
            usage: Some(TokenUsage { input_tokens: 10, output_tokens: 5, .. }),
            estimated_cost_usd_nanos: Some(20_500),
        } if *run_id == compaction_run && model == "test/model"
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            session,
            run_id,
            context_tokens: Some(13),
            ..
        } if *run_id == compaction_run && session.context_tokens.is_none()
    )));
    let (before_bytes, after_bytes, summary_excerpt, context_tokens) = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCompacted {
                session,
                before_bytes,
                after_bytes,
                summary,
            } => Some((
                *before_bytes,
                *after_bytes,
                summary.clone(),
                session.context_tokens,
            )),
            _ => None,
        })
        .unwrap();
    assert!(before_bytes > 0);
    assert_eq!(
        after_bytes,
        (COMPACTION_SUMMARY_PREAMBLE.len() + 2 + valid_summary("hello").len()) as u64
    );
    assert_eq!(summary_excerpt, Some(valid_summary("hello")));
    assert_eq!(context_tokens, None);

    // The transcript is untouched: no new message rows, one more run,
    // cost increased, session idle again.
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(focused.messages.len(), 2);
    assert_eq!(focused.runs.len(), 2);
    assert_eq!(focused.summary.status, SessionStatus::Idle);
    assert_eq!(focused.summary.context_tokens, None);
    assert_eq!(focused.runs[1].context_tokens, Some(13));
    assert!(focused.summary.estimated_cost_usd_nanos.unwrap() > cost_before);
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let stored_basis: Option<String> = connection
        .query_row(
            "SELECT context_occupancy_json FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_basis, None);
}

#[tokio::test]
async fn assembly_after_compaction_is_summary_plus_verbatim_span_and_recompaction_folds() {
    let mut harness = scripted_runs_harness(ApprovalMode::Ask, vec![]).await;
    submit_prompt(&harness, "first prompt").await;
    collect_through_finished(&mut harness.events).await;

    compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;
    {
        // The summarization request is the assembled context plus the
        // fixed instruction, with the file list seeded mechanically.
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[1]);
        assert!(texts.iter().any(|text| text == "first prompt"));
        let instruction = texts.last().unwrap();
        assert!(instruction.starts_with("Summarize this conversation"));
        assert!(instruction.contains("Files touched"));
        assert!(instruction.contains("(none recorded)"));
    }

    submit_prompt(&harness, "second prompt").await;
    collect_through_finished(&mut harness.events).await;
    {
        // Assembly is now summary + verbatim span after the marker; the
        // original prompt survives only inside the summary.
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[2]);
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert_eq!(texts[1], "second prompt");
        assert!(!texts.iter().any(|text| text == "first prompt"));
    }

    // Recompaction summarizes the prior summary plus the span since.
    compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;
    {
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[3]);
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert!(texts.iter().any(|text| text == "second prompt"));
        assert!(
            texts
                .last()
                .unwrap()
                .starts_with("Summarize this conversation")
        );
    }

    submit_prompt(&harness, "third prompt").await;
    collect_through_finished(&mut harness.events).await;
    {
        // Only the newest summary replays; prior summaries are folded in,
        // not stacked.
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        assert_eq!(texts.len(), 2);
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert_eq!(texts[1], "third prompt");
    }

    // Bounded history: both compactions are retained for future rollback.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let compactions: u32 = connection
        .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(compactions, 2);
    drop(connection);
    harness.runtime.shutdown().await.unwrap();
    assert_assembly_matches_reference(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );
}

#[test]
fn compaction_instruction_bounds_large_utf8_file_lists() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/bounded-instruction')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, title, status, approval_mode,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'Bounded', 'idle', 'ask', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    for ordinal in 0..100 {
        let path = format!("目录/{ordinal:03}/{}", "é".repeat(4_000));
        connection
            .execute(
                "INSERT INTO session_files(session_id, path, content_hash, updated_at_ms)
                 VALUES (?1, ?2, 'hash', 1)",
                params![session_id.to_string(), path],
            )
            .unwrap();
    }

    let instruction = compaction_instruction(&connection, session_id).unwrap();

    assert!(instruction.len() <= context::COMPACTION_INSTRUCTION_BYTES);
    assert!(instruction.contains("目录/"));
    assert!(instruction.contains("additional paths omitted"));
    assert!(instruction.is_char_boundary(instruction.len()));
}

#[tokio::test]
async fn prompts_below_the_context_threshold_never_auto_compact() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Text("second answer".to_owned()),
    ])
    .await;
    for prompt in ["one", "two"] {
        let run_id = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
        // Only the prompt itself runs: nothing claims ahead of it and no
        // compaction commits.
        assert!(observed.iter().all(|event| match &event.event {
            SessionEvent::RunStarted {
                run_id: started, ..
            } => *started == run_id,
            SessionEvent::SessionCompacted { .. } => false,
            _ => true,
        }));
    }
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
}

/// F05: the prompt that triggered automatic compaction is retried from the
/// stored placeholder, but its attachment must be the bytes the first
/// attempt read, not a re-read of a file that changed while the summary ran.
#[tokio::test]
async fn an_over_budget_prompt_keeps_its_first_attachment_bytes_across_auto_compaction() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("done".to_owned()),
        AutoCompactScript::Text("done again".to_owned()),
    ])
    .await;
    std::fs::write(harness.workspace_path.join("a.txt"), "ALPHA_ORIGINAL\n").unwrap();
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    // Over budget with an attachment: the claim resolves the file, plans
    // Compact, summarizes, then reloads the placeholder and retries.
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![
                    InputPart::text("y".repeat(MAX_PROMPT_BYTES - 64)),
                    InputPart::WorkspaceFile {
                        path: "a.txt".to_owned(),
                        expected_hash: None,
                        range: None,
                    },
                ],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: prompt, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    // Change the file as soon as compaction starts, before the retry.
    let observed = collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id != prompt),
    )
    .await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunStarted { run_id, .. } if *run_id != prompt
    )));
    std::fs::write(harness.workspace_path.join("a.txt"), "ALPHA_MODIFIED\n").unwrap();
    collect_until(&mut harness.events, finished_for(prompt)).await;

    {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let after = request_texts(&requests[2]);
        let sent = after.last().unwrap();
        assert!(sent.contains("ALPHA_ORIGINAL"), "{sent}");
        assert!(!sent.contains("ALPHA_MODIFIED"), "{sent}");
    }

    // And the follow-up reconstructs the same bytes from the store.
    let third = queue_prompt(&harness.runtime, harness.session_id, "after".to_owned()).await;
    collect_until(&mut harness.events, finished_for(third)).await;
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(&requests[3]);
    let replayed = texts
        .iter()
        .find(|text| text.contains("<attached-file path=\"a.txt\""))
        .expect("the attached prompt is replayed");
    assert!(replayed.contains("ALPHA_ORIGINAL"), "{replayed}");
}

#[tokio::test]
async fn storage_overflow_compacts_before_the_queued_prompt() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("done".to_owned()),
        AutoCompactScript::Text("done again".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let prompt = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(prompt)).await;

    // The compaction claims first; the prompt stays queued and runs
    // right after it.
    let compaction = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } if *run_id != prompt => Some(*run_id),
            _ => None,
        })
        .expect("an auto-compaction run must start before the prompt");
    let compaction_finished = position_of(&observed, |event| {
        matches!(
            event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == compaction
        )
    });
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == prompt),
    );
    let prompt_finished = position_of(&observed, |event| {
        matches!(
            event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == prompt
        )
    });
    assert!(compaction_finished < compacted);
    assert!(compacted < prompt_started);
    assert!(prompt_started < prompt_finished);

    {
        // The summarization request ends with the fixed instruction, and
        // the prompt then runs on the compacted assembly.
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let summarize = request_texts(&requests[1]);
        assert!(
            summarize
                .last()
                .unwrap()
                .starts_with("Summarize this conversation")
        );
        let after = request_texts(&requests[2]);
        assert!(after[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert!(after[0].contains("the summary"));
        assert_eq!(after[after.len() - 1], "y".repeat(MAX_PROMPT_BYTES));
    }

    // The run row records automatic provenance.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let (auto, context_base_bytes, context_increment_bytes): (bool, Option<i64>, i64) = connection
        .query_row(
            "SELECT auto_compaction, context_base_bytes, context_increment_bytes
             FROM runs WHERE id = ?1",
            [compaction.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(auto);
    assert!(context_base_bytes.is_some_and(|bytes| bytes > 0));
    assert_eq!(
        context_increment_bytes,
        i64::try_from(
            crate::CONTEXT_MESSAGE_FRAMING_BYTES
                + crate::CONTEXT_BLOCK_FRAMING_BYTES
                + valid_summary("the summary").len() as u64,
        )
        .unwrap()
    );

    // No re-trigger: the assembly shrank below the threshold, so the
    // next prompt runs directly.
    let third = queue_prompt(&harness.runtime, harness.session_id, "after".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(third)).await;
    assert!(observed.iter().all(|event| match &event.event {
        SessionEvent::RunStarted { run_id, .. } => *run_id == third,
        SessionEvent::SessionCompacted { .. } => false,
        _ => true,
    }));
}

#[tokio::test]
async fn compaction_sends_and_persists_the_effective_output_cap() {
    for (configured, expected) in [(1_024, 1_024), (4_096, 2_048)] {
        let mut harness = auto_compact_harness_with_limits(
            vec![
                AutoCompactScript::Text(over_threshold_output()),
                AutoCompactScript::Text(valid_summary("the summary")),
                AutoCompactScript::Text("done".to_owned()),
            ],
            None,
            configured,
        )
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;
        let prompt = queue_prompt(
            &harness.runtime,
            harness.session_id,
            "y".repeat(MAX_PROMPT_BYTES),
        )
        .await;
        let observed = collect_until(&mut harness.events, finished_for(prompt)).await;
        let compaction = observed
            .iter()
            .find_map(|event| match event.event {
                SessionEvent::RunStarted { run_id, .. } if run_id != prompt => Some(run_id),
                _ => None,
            })
            .expect("the automatic compaction must start");

        {
            let requests = harness.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1].max_output_tokens(), expected);
            assert!(requests[1].tools().is_empty());
        }
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ModelTurnCompleted {
                run_id,
                model: ModelSelection {
                    model_is_fallback: false,
                    max_output_tokens: Some(cap),
                    ..
                },
                ..
            } if *run_id == compaction && *cap == expected
        )));
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                include_sessions: Vec::new(),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let persisted = snapshot
            .focused
            .unwrap()
            .runs
            .into_iter()
            .find(|run| run.id == compaction)
            .and_then(|run| run.resolved_model)
            .expect("the compaction must retain its resolved model audit");
        assert_eq!(persisted.max_output_tokens, expected);
    }
}

#[tokio::test]
async fn saturated_reserved_reload_waits_after_auto_compaction() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("done".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let queued = harness
        .runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("y".repeat(MAX_PROMPT_BYTES))],
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
    let saturated = saturate_control_lane(&harness.runtime).await;
    harness.runtime.request_schedule();
    tokio::time::sleep(Duration::from_millis(50)).await;
    saturated.release().await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;

    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        } if finished == run_id
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
    assert!(!*harness.runtime.inner.failed.borrow());
}

#[tokio::test]
async fn permanent_reserved_reload_failure_settles_only_that_prompt() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("next completed".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let queued = harness
        .runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("y".repeat(MAX_PROMPT_BYTES))],
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
    store::fail_reserved_reloads(run_id, [SessionRuntimeError::CONSTRAINT]);
    harness.runtime.request_schedule();
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;

    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Server,
                    message,
                }
            },
            ..
        } if *finished == run_id && message.contains("failed to reload the reserved prompt")
    )));
    assert!(!*harness.runtime.inner.failed.borrow());
    let next = queue_prompt(&harness.runtime, harness.session_id, "next".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(next)).await;
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Completed,
            ..
        } if finished == next
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
    assert!(!*harness.runtime.inner.failed.borrow());
}

#[tokio::test]
async fn a_context_overflow_failure_compacts_before_the_next_prompt() {
    // The provider rejects the first prompt as exceeding the model
    // context window while the session is still under the byte
    // threshold. The failure must be loud (a failed run outcome naming
    // the overflow) and the next prompt must compact first instead of
    // hitting the same wall.
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("recovered".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(first)).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Failed { failure },
                ..
            } if *run_id == first
                && failure.kind == RunFailureKind::ProviderContextExceeded
        )),
        "the overflow must surface as a failed run outcome"
    );

    let second = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    // A compaction run claims ahead of the retried prompt.
    let compaction = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } if *run_id != second => Some(*run_id),
            _ => None,
        })
        .expect("a compaction must run before the retried prompt");
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == second),
    );
    assert!(compacted < prompt_started);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == compaction
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
}

#[tokio::test]
async fn unknown_provider_identity_still_compacts_a_known_overflow_before_the_retry() {
    // Custom/LiteLLM deployments and dynamic AWS region chains resolve
    // without a request-shape identity. That disables occupancy reuse,
    // but a provider-reported overflow must still compact exactly once
    // instead of re-sending the same request every retry.
    let mut harness = auto_compact_harness_with_loader(AutoCompactLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        scripts: vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text(valid_summary("the summary")),
            AutoCompactScript::Text("recovered".to_owned()),
        ],
        loads: StdMutex::new(0),
        context_window: None,
        max_output_tokens: 256,
        provider_identity: false,
    })
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(first)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed { failure },
            ..
        } if *run_id == first && failure.kind == RunFailureKind::ProviderContextExceeded
    )));

    let second = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == second),
    );
    assert!(compacted < prompt_started);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        3,
        "overflow, compaction, retry: the known overflow is never re-sent"
    );

    // Reuse stays disabled: no occupancy basis is persisted for an
    // unknown identity even after a measured turn.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let (occupancy, pending): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT context_occupancy_json, pending_context_overflow_basis_json
             FROM sessions WHERE id = ?1",
            [harness.session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(occupancy, None);
    assert_eq!(pending, None);
}

#[tokio::test]
async fn pruned_history_still_compacts_a_known_overflow_before_the_retry() {
    // Once the transcript holds more than CONTEXT_PRUNE_KEEP_TURNS
    // assistant turns, assembly stubs old read-only results. That
    // rewrite makes byte-monotonic occupancy reuse impossible, but the
    // pruned request is still an uncertain repeat of a provider-reported
    // overflow and must compact rather than poll.
    let mut scripts = vec![AutoCompactScript::ReadNoteThenText("read it".to_owned())];
    scripts.extend((0..CONTEXT_PRUNE_KEEP_TURNS).map(|_| AutoCompactScript::Text("ok".to_owned())));
    scripts.extend([
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("recovered".to_owned()),
    ]);
    let mut harness = auto_compact_harness(scripts).await;
    std::fs::write(harness.workspace_path.join("note.txt"), "n".repeat(600)).unwrap();

    for prompt in std::iter::once("read the note")
        .chain(std::iter::repeat_n("more", CONTEXT_PRUNE_KEEP_TURNS))
    {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let overflow = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(overflow)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed { failure },
            ..
        } if *run_id == overflow && failure.kind == RunFailureKind::ProviderContextExceeded
    )));
    let requests_before_retry = harness.requests.lock().unwrap().len();
    {
        let requests = harness.requests.lock().unwrap();
        let overflowing = requests.last().unwrap();
        assert!(
            overflowing
                .messages()
                .iter()
                .flat_map(Message::content)
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult { content, .. } if content.starts_with("[pruned")
                )),
            "the overflowing request must already carry pruned history"
        );
    }

    let retry = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(retry)).await;
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == retry),
    );
    assert!(compacted < prompt_started);
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        requests_before_retry + 2,
        "compaction then the retry: the known overflow is never re-sent"
    );
}

#[tokio::test]
async fn failed_manual_compaction_does_not_mask_provider_overflow_evidence() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Fail,
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("recovered".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let failed_compaction = compact_session(&harness.runtime, harness.session_id).await;
    let failed = collect_until(&mut harness.events, finished_for(failed_compaction)).await;
    assert!(failed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed { .. },
            ..
        } if *run_id == failed_compaction
    )));

    let retry = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(retry)).await;
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == retry),
    );
    assert!(compacted < prompt_started);
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        4,
        "the retry must compact instead of repeating the known overflow"
    );
}

#[tokio::test]
async fn cancelled_queued_prompt_does_not_mask_provider_overflow_evidence() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("recovered".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let queued = harness
        .runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("cancel this".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: cancelled, ..
    } = queued.receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    harness
        .runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: cancelled },
        )
        .await
        .unwrap();

    let retry = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(retry)).await;
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == retry),
    );
    assert!(compacted < prompt_started);
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        3,
        "the retry must compact instead of repeating the known overflow"
    );
}

#[tokio::test]
async fn provider_overflow_evidence_survives_restart_until_compaction_commits() {
    let mut harness = auto_compact_harness(vec![AutoCompactScript::ContextOverflow]).await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(first)).await;
    let after = observed.last().unwrap().cursor;
    let workspace_id = harness.workspace_id;
    let session_id = harness.session_id;
    let database_path = harness.workspace_path.join("sessions.sqlite3");
    harness.runtime.close().await.unwrap();
    drop(harness.runtime);

    let connection = Connection::open(&database_path).unwrap();
    let pending: Option<String> = connection
        .query_row(
            "SELECT pending_context_overflow_basis_json FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(pending.is_some());
    drop(connection);

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(AutoCompactLoader {
            requests: Arc::clone(&requests),
            scripts: vec![
                AutoCompactScript::Text(valid_summary("the summary")),
                AutoCompactScript::Text("recovered".to_owned()),
            ],
            loads: StdMutex::new(0),
            context_window: None,
            max_output_tokens: 256,
            provider_identity: true,
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
    let retry = queue_prompt(&runtime, session_id, "retry after restart".to_owned()).await;
    let observed = collect_until(&mut events, finished_for(retry)).await;
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == retry),
    );
    assert!(compacted < prompt_started);
    assert_eq!(requests.lock().unwrap().len(), 2);

    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let pending: Option<String> = connection
        .query_row(
            "SELECT pending_context_overflow_basis_json FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, None);
}

#[tokio::test]
async fn failed_overflow_recovery_never_resends_the_known_overflowing_prompt() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Fail,
        AutoCompactScript::Text("must not be polled".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    message,
                },
            },
            ..
        } if *run_id == second && message.contains("provider previously rejected")
    )));
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        2,
        "the second prompt must not repeat a provider-known overflow"
    );
}

#[tokio::test]
async fn auto_compaction_panic_settles_its_exact_prompt_reservation() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Panic,
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let prompt = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(prompt)).await;
    let compaction = observed
        .iter()
        .find_map(|event| match event.event {
            SessionEvent::RunStarted { run_id, .. } if run_id != prompt => Some(run_id),
            _ => None,
        })
        .expect("the automatic compaction must start before panicking");
    for run_id in [compaction, prompt] {
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                run_id: finished,
                outcome: RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::Server,
                        ..
                    }
                },
                ..
            } if *finished == run_id
        )));
    }
    assert!(
        harness
            .runtime
            .inner
            .store
            .unfinished_run_ids()
            .await
            .unwrap()
            .is_empty()
    );

    let created = create_session(&harness.runtime, harness.workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: next_session,
    } = created.outcome
    else {
        panic!("unexpected receipt")
    };
    let next = queue_prompt(&harness.runtime, next_session, "try again".to_owned()).await;
    let continued = collect_until(&mut harness.events, finished_for(next)).await;
    assert!(
        continued.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Completed,
                ..
            } if run_id == next
        )),
        "subsequent scheduling did not recover: {continued:#?}"
    );
}

#[tokio::test]
async fn exceeding_the_hard_budget_compacts_once_and_the_prompt_proceeds() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("x".repeat(MAX_CONTEXT_BYTES - 100 * 1024)),
        AutoCompactScript::Text(valid_summary("the summary")),
        AutoCompactScript::Text("done".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    // Context plus prompt exceeds the hard budget. Submission is
    // admitted (previously this was rejected outright); the claim
    // compacts once, re-checks, and the prompt proceeds.
    let second = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_prompt_still_over_budget_after_compacting_fails_with_the_policy_outcome() {
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(20 * 1024 * 4)),
            // Pathological summarizer: the summary is as large as the
            // transcript it replaces. Validation rejects it, so no marker
            // commits and the retry is still past the model window.
            AutoCompactScript::Text(valid_summary(&"s".repeat(20 * 1024 * 4))),
        ],
        Some(32 * 1024),
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(10 * 1024 * 4),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    // The one attempt happened and was rejected for not shrinking...
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed { failure: RunFailure { kind: RunFailureKind::Policy, message } },
            ..
        } if *run_id != second && message.contains("did not shrink")
    )));
    // ...and the prompt then fails with the context policy failure
    // without reaching the model.
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == second => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        outcome,
        RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Policy,
                ref message,
            }
        } if message.contains("selected model")
    ));
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn oversized_compaction_summary_fails_without_committing_a_marker() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("seed answer".to_owned()),
        AutoCompactScript::Text(format!(
            "{}\n{}",
            valid_summary("oversized"),
            "s".repeat(MAX_CONTEXT_BYTES)
        )),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(compaction)).await;

    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    message,
                },
            },
            ..
        } if *run_id == compaction && message.contains("4 MiB")
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    let connection = Connection::open(harness._directory.path().join("sessions.sqlite3")).unwrap();
    let markers: u64 = connection
        .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(markers, 0);
}

#[tokio::test]
async fn a_failed_auto_compaction_does_not_strand_the_queued_prompt() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Fail,
        AutoCompactScript::Text("done".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    // The summarizer failed and committed nothing...
    let compaction = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } if *run_id != second => Some(*run_id),
            _ => None,
        })
        .unwrap();
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Failed { .. }, .. }
            if *run_id == compaction
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    // ...and the unchanged overflowing prompt fails closed after that one
    // attempt instead of reaching the provider.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    ..
                },
            },
            ..
        } if *run_id == second
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_compaction_that_does_not_shrink_the_assembly_never_loops() {
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(20 * 1024 * 4)),
            // The summary itself stays past the threshold: the guard must
            // reject the prompt after the single attempt.
            AutoCompactScript::Text(valid_summary(&"s".repeat(20 * 1024 * 4))),
            AutoCompactScript::Text("done".to_owned()),
        ],
        Some(32 * 1024),
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(10 * 1024 * 4),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    let compactions = observed
        .iter()
        .filter(|event| {
            matches!(
                &event.event,
                SessionEvent::RunStarted { run_id, .. } if *run_id != second
            )
        })
        .count();
    assert_eq!(compactions, 1, "exactly one automatic attempt per prompt");
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    ..
                },
            },
            ..
        } if *run_id == second
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cancelling_the_queued_prompt_cancels_the_pending_auto_compaction() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(over_threshold_output()),
        AutoCompactScript::Stall,
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let prompt = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
    )
    .await;
    let observed = collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id != prompt),
    )
    .await;
    let SessionEvent::RunStarted {
        run_id: compaction, ..
    } = observed.last().unwrap().event
    else {
        panic!("expected the auto-compaction to start")
    };

    // Cancelling the only queued prompt cascades to the compaction that
    // was running on its behalf.
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: prompt },
        )
        .await
        .unwrap();
    let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id == prompt
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::CancellationRequested { run_id, .. } if *run_id == compaction
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    // The compaction settled cancelled and the session ended idle.
    let session = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Cancelled,
                session,
                ..
            } if *run_id == compaction => Some(session.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(session.queued_prompts, 0);
    assert_eq!(session.active_run_id, None);
}

#[tokio::test]
async fn cancelling_a_queued_prompt_never_cancels_a_manual_compaction() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("hello".to_owned()),
        AutoCompactScript::Stall,
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "hi".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    // A user-requested compaction claims and parks at the model.
    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == compaction),
    )
    .await;

    // Queue a prompt behind it, then cancel that prompt: the manual
    // compaction must keep running.
    let prompt = queue_prompt(&harness.runtime, harness.session_id, "later".to_owned()).await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: prompt },
        )
        .await
        .unwrap();
    let observed = collect_until(&mut harness.events, finished_for(prompt)).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::CancellationRequested { run_id, .. } if *run_id == compaction
    )));

    // Clean up: cancel the parked compaction directly.
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: compaction },
        )
        .await
        .unwrap();
    let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id == compaction
    )));
}

#[tokio::test]
async fn assembly_prunes_stale_read_only_results_but_never_mutating_ones() {
    let note = "n".repeat(600);
    let written = "w".repeat(600);
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![
            vec![
                ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
                (
                    "write_file",
                    format!(r#"{{"path":"out.txt","content":"{written}"}}"#),
                ),
            ],
            vec![],
            vec![],
            vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())],
            vec![],
        ],
    )
    .await;
    std::fs::write(harness.workspace_path.join("note.txt"), &note).unwrap();

    for prompt in ["one", "two", "three", "four", "five"] {
        submit_prompt(&harness, prompt).await;
        collect_through_finished(&mut harness.events).await;
    }

    let results: Vec<String> = {
        let requests = harness.requests.lock().unwrap();
        let last = requests.last().unwrap();
        last.messages()
            .iter()
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(results.len(), 3);
    // The old read is a stub naming the tool, arguments, and size.
    assert!(
        results[0].starts_with("[pruned: read_file {\"path\":\"note.txt\"} returned"),
        "stale read-only result must be stubbed, got {:?}",
        results[0]
    );
    assert!(results[0].ends_with("call it again if needed]"));
    // The equally old mutation is never pruned: not re-derivable.
    assert!(
        !results[1].starts_with("[pruned"),
        "mutating results must never be pruned, got {:?}",
        results[1]
    );
    // The recent read stays verbatim.
    assert!(
        results[2].contains("nnnn"),
        "recent results must stay verbatim, got {:?}",
        results[2]
    );
    harness.runtime.shutdown().await.unwrap();
    assert_assembly_matches_reference(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );
}

#[test]
fn compaction_summary_validation_requires_every_section_heading() {
    assert!(validate_compaction_summary(&valid_summary("ok")).is_ok());
    assert!(
        validate_compaction_summary(
            "## Intent: x\n**Decisions and constraints:** y\n- Work state: z\n\
             FILES TOUCHED: a\n5) Errors: none\nUser messages: hello"
        )
        .is_ok(),
        "numbering, markdown markup, and case are tolerated"
    );
    assert_eq!(
        validate_compaction_summary("   \n"),
        Err("compaction produced an empty summary".to_owned())
    );
    let missing = validate_compaction_summary("1. Intent: x\n2. Work state: y").unwrap_err();
    assert!(missing.contains("Decisions and constraints"));
    assert!(missing.contains("Files touched"));
    assert!(missing.contains("Errors"));
    assert!(missing.contains("User messages"));
    assert!(!missing.contains("Intent"));
    // Body text mentioning a heading word does not satisfy the section.
    let prose = validate_compaction_summary(
        "1. Intent: fix the errors: they matter\n2. Decisions and constraints: none\n\
         3. Work state: done\n4. Files touched: none\n6. User messages: hi",
    )
    .unwrap_err();
    assert_eq!(
        prose,
        "compaction summary is missing required sections: Errors"
    );
    assert!(
        validate_compaction_summary(&format!(
            "{}\n{}",
            valid_summary("x"),
            "s".repeat(MAX_CONTEXT_BYTES)
        ))
        .unwrap_err()
        .contains("4 MiB")
    );
}

#[tokio::test]
async fn malformed_summary_fails_compaction_and_retains_the_prior_compaction() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Text(valid_summary("good summary")),
        AutoCompactScript::Text("second answer".to_owned()),
        // No section headings: rejected before any marker is written.
        AutoCompactScript::Text("just some prose about the work".to_owned()),
        AutoCompactScript::Text("third answer".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "one".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let good = compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;
    let second = queue_prompt(&harness.runtime, harness.session_id, "two".to_owned()).await;
    collect_until(&mut harness.events, finished_for(second)).await;

    let bad = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(bad)).await;
    match finished_outcome(&observed, bad) {
        Some(RunOutcome::Failed { failure }) => {
            assert_eq!(failure.kind, RunFailureKind::Policy);
            assert!(failure.message.contains("missing required sections"));
        }
        other => panic!("expected a policy failure, got {other:?}"),
    }
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );

    // The prior compaction still governs assembly: the next prompt sees
    // the good summary and the verbatim span after its marker.
    let third = queue_prompt(&harness.runtime, harness.session_id, "three".to_owned()).await;
    collect_until(&mut harness.events, finished_for(third)).await;
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
    assert!(texts[0].contains("good summary"));
    assert!(texts.iter().any(|text| text == "two"));
    assert!(!texts.iter().any(|text| text == "one"));
    drop(requests);
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let (count, run): (u32, String) = connection
        .query_row(
            "SELECT COUNT(*), MAX(run_id) FROM session_compactions WHERE session_id = ?1",
            [harness.session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(run, good.to_string());
}

#[tokio::test]
async fn rollback_restores_the_prior_compaction_then_the_verbatim_transcript() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Text(valid_summary("summary one")),
        AutoCompactScript::Text("second answer".to_owned()),
        AutoCompactScript::Text(valid_summary("summary two")),
        AutoCompactScript::Text("after first rollback".to_owned()),
        AutoCompactScript::Text("after second rollback".to_owned()),
    ])
    .await;
    for prompt in ["one", "two"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
        let _ = compact_session(&harness.runtime, harness.session_id).await;
        collect_through_compacted(&mut harness.events).await;
    }

    async fn rollback(harness: &AutoCompactHarness) -> Result<CommandReceipt, SessionRuntimeError> {
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RollbackCompaction {
                    session_id: harness.session_id,
                },
            )
            .await
    }
    let receipt = rollback(&harness).await.unwrap();
    assert_eq!(
        receipt.outcome,
        CommandOutcome::CompactionRolledBack {
            session_id: harness.session_id,
            remaining: 1,
        }
    );
    let observed = collect_until(&mut harness.events, |event| {
        matches!(event, SessionEvent::SessionCompactionRolledBack { .. })
    })
    .await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCompactionRolledBack { session, remaining: 1 }
            if session.context_tokens.is_none()
    )));
    let third = queue_prompt(&harness.runtime, harness.session_id, "three".to_owned()).await;
    collect_until(&mut harness.events, finished_for(third)).await;
    {
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        assert!(texts[0].contains("summary one"), "{texts:?}");
        assert!(!texts[0].contains("summary two"));
        assert!(texts.iter().any(|text| text == "two"));
        assert!(texts.iter().any(|text| text == "three"));
    }

    let receipt = rollback(&harness).await.unwrap();
    assert_eq!(
        receipt.outcome,
        CommandOutcome::CompactionRolledBack {
            session_id: harness.session_id,
            remaining: 0,
        }
    );
    let fourth = queue_prompt(&harness.runtime, harness.session_id, "four".to_owned()).await;
    collect_until(&mut harness.events, finished_for(fourth)).await;
    {
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        assert!(!texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert!(texts.iter().any(|text| text == "one"), "{texts:?}");
        assert!(texts.iter().any(|text| text == "four"));
    }
    assert_eq!(
        rollback(&harness).await,
        Err(SessionRuntimeError::NoCompactionToRollBack)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_cut_result_names_a_handle_the_model_can_page_and_search_exactly() {
    // 1 500 lines of ~60 bytes (~90 KiB) of shell output: over the 16
    // KiB shell bound, under the 128 KiB capture cap, so the inline
    // result is head+tail with a marker and the complete capture spills.
    // Line 1 000 carries a secret the inline preview masks.
    let command = "for n in $(seq 1 1500); do if [ $n -eq 1000 ]; then echo \
         'TOKEN=AKIAIOSFODNN7EXAMPLE and the rest of line one thousand'; \
         else printf 'line %5d zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\\n' $n; fi; done";
    // A shell loop is a Prompt-tier command; the session runs under
    // `full` so no approval is waited on.
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellThenRecall {
                command: command.to_owned(),
                recall: serde_json::json!({ "offset": 1000, "limit": 3 }),
                text: "recalled".to_owned(),
            }],
            loads: StdMutex::new(0),
            context_window: None,
            max_output_tokens: 256,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;

    let run = queue_prompt(&harness.runtime, harness.session_id, "go".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    let finished: Vec<&ToolCallSnapshot> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 2, "{finished:?}");
    let shell = finished[0].result.as_deref().unwrap();
    assert_eq!(finished[0].name, "shell");
    assert!(!finished[0].is_error, "{shell}");
    let marker = shell
        .lines()
        .find(|line| line.starts_with("…[qq: "))
        .expect("the cut result carries a marker");
    assert!(marker.contains("; full output t:shell:"), "{marker}");
    assert!(marker.contains("; read_tool_result offset="), "{marker}");
    assert!(!marker.contains("not stored"), "{marker}");
    assert!(!shell.contains("AKIA"), "the inline preview is masked");
    // The handle's call prefix is this call's id.
    let call8 = &finished[0].id.to_string()[..8];
    assert!(marker.contains(&format!("t:shell:{call8}:")), "{marker}");

    let recall = finished[1];
    assert_eq!(recall.name, "read_tool_result");
    assert!(!recall.is_error, "{:?}", recall.result);
    let page = recall.result.as_deref().unwrap();
    let header = page.lines().next().unwrap();
    assert!(header.starts_with("read_tool_result t:shell:"), "{header}");
    // The stored output is the complete shell result: its own header
    // line first, so stored line N+1 is output line N.
    assert!(header.ends_with(" L1000-1002/1501 next=1003"), "{header}");
    assert!(
        page.contains("\n1001\tTOKEN=AKIAIOSFODNN7EXAMPLE and the rest"),
        "explicit reads return exact, unmasked bytes: {page}"
    );
    {
        let requests = harness.requests.lock().unwrap();
        assert!(
            requests
                .last()
                .unwrap()
                .tools()
                .iter()
                .any(|tool| tool.name() == "read_tool_result"),
            "session runs declare read_tool_result"
        );
    }
}

#[tokio::test]
async fn rollback_is_refused_while_the_session_is_not_idle() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Text(valid_summary("summary")),
        AutoCompactScript::Stall,
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "one".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let _ = compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;
    let stalled = queue_prompt(&harness.runtime, harness.session_id, "stall".to_owned()).await;
    collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == stalled),
    )
    .await;
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RollbackCompaction {
                    session_id: harness.session_id,
                },
            )
            .await,
        Err(SessionRuntimeError::SessionActive)
    );
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: stalled },
        )
        .await
        .unwrap();
    collect_until(&mut harness.events, finished_for(stalled)).await;
}

#[tokio::test]
async fn repeated_compactions_preserve_seeded_facts_and_bound_history() {
    // A summarizer that folds the prior summary and the verbatim span
    // faithfully: every user message, exact path, decision, and error
    // string it is shown reappears under its section. Repeated
    // compactions must keep those seeded facts reachable through the
    // latest summary alone, and never retain more than the bounded
    // history.
    struct FoldingLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for FoldingLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(FoldingProvider { requests }, "test-model", 4_096)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct FoldingProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for FoldingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let texts = request_texts(&request);
            self.requests.lock().unwrap().push(request);
            let summarizing = texts
                .last()
                .is_some_and(|text| text.starts_with("Summarize this conversation"));
            let text = if summarizing {
                // Fold: carry forward every fact line from the prior
                // summary and every verbatim user message.
                let mut facts = Vec::new();
                for text in &texts[..texts.len() - 1] {
                    for line in text.lines() {
                        if line.starts_with("FACT ") || line.starts_with("- FACT ") {
                            facts.push(line.trim_start_matches("- ").to_owned());
                        }
                    }
                    if text.starts_with("USER ") {
                        facts.push(format!("FACT {text}"));
                    }
                }
                facts.sort();
                facts.dedup();
                let body = facts
                    .iter()
                    .map(|fact| format!("- {fact}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "1. Intent: see below\n{body}\n2. Decisions and constraints: see below\n\
                     {body}\n3. Work state: folded\n4. Files touched: see below\n{body}\n\
                     5. Errors: see below\n{body}\n6. User messages: see below\n{body}"
                )
            } else {
                "ack".to_owned()
            };
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta { text }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(FoldingLoader {
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

    let seeded = [
        "USER constraint: never touch src/legacy/parser.rs",
        "USER decision: use --features minimal for embedders",
        "USER error: E0433 failed to resolve: use of undeclared type `RunLimits`",
        "USER verification: cargo test --workspace passed",
    ];
    let rounds = COMPACTION_HISTORY_ROWS as usize + 2;
    for round in 0..rounds {
        let prompt = seeded[round % seeded.len()].to_owned();
        let run = queue_prompt(&runtime, session_id, prompt).await;
        collect_until(&mut events, finished_for(run)).await;
        let compaction = compact_session(&runtime, session_id).await;
        let observed = collect_through_compacted(&mut events).await;
        assert_eq!(
            finished_outcome(&observed, compaction),
            Some(RunOutcome::Completed),
            "round {round} must compact"
        );
    }

    let probe = queue_prompt(&runtime, session_id, "USER probe".to_owned()).await;
    collect_until(&mut events, finished_for(probe)).await;
    let requests = requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
    for fact in seeded {
        assert!(
            texts[0].contains(fact),
            "seeded fact {fact:?} must survive {rounds} compactions; got {}",
            texts[0]
        );
    }
    // Only the latest summary and the verbatim span are assembled: no
    // earlier user message appears verbatim outside the summary.
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.starts_with("USER "))
            .count(),
        1
    );
    drop(requests);

    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let retained: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM session_compactions WHERE session_id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, COMPACTION_HISTORY_ROWS);
}

#[tokio::test]
async fn compaction_shrinks_a_tool_heavy_assembly_at_least_eightfold() {
    // Phase 5 acceptance: a compaction of a transcript dominated by tool
    // traffic must shrink the measured assembly by at least 8x, and the
    // published before/after bytes must agree with the next assembly.
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ReadNoteThenText("read it".to_owned()),
        AutoCompactScript::ReadNoteThenText("read it again".to_owned()),
        AutoCompactScript::Text(valid_summary("the note repeats one line")),
        AutoCompactScript::Text("after".to_owned()),
    ])
    .await;
    let line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
    std::fs::write(harness.workspace_path.join("note.txt"), line.repeat(1_500)).unwrap();
    for prompt in ["one", "two"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    assert_eq!(
        finished_outcome(&observed, compaction),
        Some(RunOutcome::Completed)
    );
    let (before, after) = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCompacted {
                before_bytes,
                after_bytes,
                ..
            } => Some((*before_bytes, *after_bytes)),
            _ => None,
        })
        .unwrap();
    assert!(
        before > COMPACTION_SHRINKAGE_FLOOR_BYTES as u64,
        "the fixture must exceed the shrinkage floor: {before}"
    );
    assert!(
        before >= after * 8,
        "compaction must shrink at least 8x: {before} -> {after}"
    );

    let run = queue_prompt(&harness.runtime, harness.session_id, "three".to_owned()).await;
    collect_until(&mut harness.events, finished_for(run)).await;
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].contains("the note repeats one line"), "{texts:?}");
    assert!(!texts.iter().any(|text| text.contains(line.trim())));
}

#[tokio::test]
async fn search_history_recalls_compacted_transcript_with_bounded_citations() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ReadNoteThenText(
            "noted: the parser lives at src/legacy/parser.rs".to_owned(),
        ),
        AutoCompactScript::Text(valid_summary("folded")),
        AutoCompactScript::SearchHistoryThenText("LEGACY/PARSER".to_owned(), "recalled".to_owned()),
        AutoCompactScript::SearchHistoryThenText("zzz-absent".to_owned(), "none".to_owned()),
    ])
    .await;
    std::fs::write(
        harness.workspace_path.join("note.txt"),
        "keep src/legacy/parser.rs untouched\n",
    )
    .unwrap();
    let first = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "never touch src/legacy/parser.rs".to_owned(),
    )
    .await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let _ = compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;

    let second = queue_prompt(&harness.runtime, harness.session_id, "recall".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    let (result, is_error) = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == crate::runtime::SEARCH_HISTORY_TOOL =>
            {
                Some((tool_call.result.clone().unwrap(), tool_call.is_error))
            }
            _ => None,
        })
        .expect("search_history must dispatch in a session run");
    assert!(!is_error, "{result}");
    // Limit 2 caps the three transcript hits (prompt, tool result,
    // assistant text), in transcript order.
    assert!(result.starts_with("2 history match(es)"), "{result}");
    assert!(
        result.contains("[user message #1]\nnever touch"),
        "{result}"
    );
    assert!(
        result.contains("[read_file result, user message #1 turn 1 call 1]\nread "),
        "{result}"
    );
    assert!(!result.contains("noted: the parser"), "{result}");
    {
        let requests = harness.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert!(
            request
                .tools()
                .iter()
                .any(|tool| tool.name() == crate::runtime::SEARCH_HISTORY_TOOL),
            "session runs declare search_history"
        );
        // The compacted assembly no longer carries the verbatim fact; the
        // tool result is the only route back to it.
        let texts = request_texts(request);
        assert!(texts[0].contains("folded"), "{texts:?}");
        assert!(!texts[0].contains("legacy/parser"), "{texts:?}");
    }

    let third = queue_prompt(&harness.runtime, harness.session_id, "again".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(third)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == crate::runtime::SEARCH_HISTORY_TOOL
                && tool_call.result.as_deref()
                    == Some("No history matches for \"zzz-absent\".")
    )));
}

#[tokio::test]
async fn a_window_overflow_compacts_even_though_the_summarizer_reads_the_same_transcript() {
    // Regression: the summarizer request carries the very transcript that
    // overflowed the model window. Planning it against the window refused
    // every window-triggered compaction and reported "already attempted"
    // for a compaction that never started. With the summarizer planned
    // against storage only, the compaction runs and the prompt proceeds.
    let window: u32 = 32 * 1024;
    let mut harness = auto_compact_harness_with_window(
        vec![
            // ~30k estimated tokens of history: inside the window alone,
            // past it once the system prompt and the prompt below join.
            AutoCompactScript::Text("x".repeat(30 * 1024 * 4)),
            AutoCompactScript::Text(valid_summary("the summary")),
            AutoCompactScript::Text("done".to_owned()),
        ],
        Some(window),
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(&harness.runtime, harness.session_id, "y".repeat(4 * 1024)).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    let compaction = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } if *run_id != second => Some(*run_id),
            _ => None,
        })
        .expect("the window overflow must start an automatic compaction");
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == compaction
    )));
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    // Three provider requests: the seed, the summarizer, the prompt.
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        request_texts(&requests[1])
            .last()
            .unwrap()
            .starts_with("Summarize this conversation")
    );
}

#[tokio::test]
async fn manual_compaction_runs_when_the_estimate_already_exceeds_the_window() {
    // A session whose byte estimate is past the model window is exactly the
    // one a user reaches for /compact on. The summarizer must send.
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(40 * 1024 * 4)),
            AutoCompactScript::Text(valid_summary("recovered")),
        ],
        Some(32 * 1024),
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    // `SessionCompacted` follows the compaction's `RunFinished`.
    let observed = collect_until(&mut harness.events, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
            || matches!(
                event,
                SessionEvent::RunFinished { run_id, outcome: RunOutcome::Failed { .. }, .. }
                    if *run_id == compaction
            )
    })
    .await;
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == compaction => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_run_that_outgrows_the_window_stubs_its_stale_reads_instead_of_failing() {
    // Regression: mid-run overflow was a hard failure (`BetweenRunsOnly`).
    // Eight verbatim 8 KiB reads (~16k tokens) plus the prompt do not fit a
    // 16k window with a 2k output reserve, but once the reads older than the
    // recency window are stubbed in memory the run completes.
    let turns = 8;
    let mut harness = auto_compact_harness_with_limits(
        vec![AutoCompactScript::ReadNoteRepeatedly {
            turns,
            text: "done".to_owned(),
        }],
        Some(16 * 1024),
        2_048,
    )
    .await;
    std::fs::write(
        harness.workspace_path.join("note.txt"),
        format!("{}\n", "n".repeat(127)).repeat(64),
    )
    .unwrap();
    let run = queue_prompt(&harness.runtime, harness.session_id, "read it".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == run => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), turns + 1);
    // The last request carries stubs for the oldest reads and the most
    // recent CONTEXT_PRUNE_KEEP_TURNS results verbatim.
    let results: Vec<&str> = requests
        .last()
        .unwrap()
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), turns);
    let stubbed = results.iter().filter(|r| r.contains("[pruned")).count();
    assert!(
        stubbed >= 1 && stubbed <= turns - CONTEXT_PRUNE_KEEP_TURNS,
        "{stubbed}"
    );
    assert!(results.last().unwrap().contains(&"n".repeat(127)));
    // The stored rows are untouched: the persisted result text is verbatim.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let pruned_rows: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE result LIKE '%[pruned%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pruned_rows, 0);
}

#[tokio::test]
async fn a_prompt_inside_the_last_tenth_of_the_window_compacts_before_it_sends() {
    // ~30k estimated tokens of history against a 32k window: the next prompt
    // still fits, but inside the ten percent headroom. It compacts first
    // rather than waiting for the estimate to cross the window.
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(29 * 1024 * 4)),
            AutoCompactScript::Text(valid_summary("the summary")),
            AutoCompactScript::Text("done".to_owned()),
        ],
        Some(32 * 1024),
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(&harness.runtime, harness.session_id, "small".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    let compacted = position_of(&observed, |event| {
        matches!(event, SessionEvent::SessionCompacted { .. })
    });
    let prompt_started = position_of(
        &observed,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == second),
    );
    assert!(compacted < prompt_started);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn measured_occupancy_survives_assembly_pruning_and_admits_the_next_prompt() {
    // Regression: once assembly stubbed any read-only result (always, after
    // CONTEXT_PRUNE_KEEP_TURNS turns) the persisted measurement was
    // discarded and the next prompt was judged by the raw byte estimate over
    // the whole history. Here the provider reports a small measured
    // occupancy for a transcript whose bytes alone would overflow the
    // window; the prompt after pruning must still send without compacting.
    let turns = CONTEXT_PRUNE_KEEP_TURNS + 2;
    let mut scripts = Vec::new();
    for _ in 0..=turns {
        scripts.push(AutoCompactScript::ReadNoteThenTextMeasured {
            text: "ok".to_owned(),
            input_tokens: 500,
        });
    }
    let mut harness = auto_compact_harness_with_window(scripts, Some(6 * 1024)).await;
    // Each read is ~12 KiB once bounded (~3k estimated tokens). After
    // pruning to the last four turns the transcript alone is ~6k estimated
    // tokens, past a 6k window with its 256 output reserve, unless the
    // 500-token measurement is trusted.
    std::fs::write(
        harness.workspace_path.join("note.txt"),
        format!("{}\n", "n".repeat(127)).repeat(96),
    )
    .unwrap();
    for index in 0..turns {
        let run = queue_prompt(
            &harness.runtime,
            harness.session_id,
            format!("read {index}"),
        )
        .await;
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        let outcome = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    run_id, outcome, ..
                } if *run_id == run => Some(outcome.clone()),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(outcome, RunOutcome::Completed),
            "run {index}: {outcome:?}"
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. })),
            "run {index} must not compact: the measured occupancy fits"
        );
    }
    let requests = harness.requests.lock().unwrap();
    assert!(
        requests
            .last()
            .unwrap()
            .messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult { content, .. } if content.starts_with("[pruned")
            )),
        "the final request carries pruned history"
    );
}

/// F06: assembling a fixed retained context must not read the archive behind
/// the compaction cutoff. Wall time is a bench receipt, not a test; here the
/// guarantee is structural: the same retained context assembles identically
/// (and agrees with the per-message reference) whatever the archive size,
/// and every assembly query is index-driven with no table scan.
#[test]
fn assembly_work_follows_the_retained_context_not_the_archive() {
    let mut assembled = Vec::new();
    for archived_runs in [10_usize, 2_000] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let (connection, session_id) =
            bench_support::seed_compacted_session(&database, archived_runs, 4, 3, 512);
        let messages = load_model_context(&connection, session_id, u64::MAX).unwrap();
        drop(connection);
        assert_assembly_matches_reference(&database, session_id);
        assembled.push(messages.len());
    }
    assert_eq!(assembled[0], assembled[1], "same retained context");

    // Every assembly query reaches its rows through an index keyed by the
    // retained window or the run id; none scans a whole table.
    let directory = tempfile::tempdir().unwrap();
    let (connection, _) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        50,
        4,
        3,
        64,
    );
    for query in [
        "SELECT t.run_id FROM messages m JOIN runs r ON r.id = m.run_id
             JOIN model_turns t ON t.run_id = r.id
         WHERE m.session_id = 's' AND m.ordinal <= 9 AND m.ordinal > 1
           AND m.role = 'user' AND m.steering = 0 AND m.state = 'complete'",
        "SELECT c.run_id FROM messages m JOIN runs r ON r.id = m.run_id
             JOIN tool_calls c ON c.run_id = r.id
         WHERE m.session_id = 's' AND m.ordinal <= 9 AND m.ordinal > 1
           AND m.role = 'user' AND m.steering = 0 AND m.state = 'complete'",
        "SELECT s.run_id FROM messages m JOIN messages s ON s.run_id = m.run_id
         WHERE m.session_id = 's' AND m.ordinal <= 9 AND m.ordinal > 1
           AND m.role = 'user' AND m.steering = 0 AND m.state = 'complete'
           AND s.steering = 1 AND s.state = 'complete'",
        "SELECT a.message_id FROM messages m JOIN message_attachments a ON a.message_id = m.id
             JOIN attachment_blobs b ON b.session_id = a.session_id AND b.blob_key = a.blob_key
         WHERE m.session_id = 's' AND m.ordinal <= 9 AND m.ordinal > 1
           AND m.role = 'user' AND m.steering = 0",
    ] {
        let plan: Vec<String> = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().all(|step| !step.starts_with("SCAN")),
            "{query}\n{plan:#?}"
        );
    }
}

/// F06: `search_history` bounds the transcript bytes it visits, walks newest
/// history first so what it did cover is the most useful, and says when it
/// stopped short. A present term near the top is still found in full.
#[test]
fn history_search_is_scan_bounded_and_newest_first() {
    let directory = tempfile::tempdir().unwrap();
    // 2_000 runs x 3 turns x 8 KiB results = ~48 MiB of transcript, well
    // past the 8 MiB budget.
    let (connection, session_id) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        2_000,
        0,
        3,
        8 * 1024,
    );
    let calling_run = RunId::from_bytes([9; 16]);

    let absent =
        search_session_history(&connection, session_id, calling_run, "no-such-term", 8).unwrap();
    assert!(absent.matches.is_empty());
    assert!(absent.truncated, "an absent term must hit the scan budget");

    // The newest prompt's needle is found; the oldest prompt's needle lies
    // behind the budget and is reported as unexamined rather than absent.
    let newest =
        search_session_history(&connection, session_id, calling_run, "needle-1999", 8).unwrap();
    assert_eq!(newest.matches.len(), 1, "{newest:?}");
    assert!(newest.matches[0].citation.starts_with("user message #"));
    let oldest =
        search_session_history(&connection, session_id, calling_run, "needle-0 ", 8).unwrap();
    assert!(oldest.matches.is_empty());
    assert!(oldest.truncated);

    // A common term returns the newest `limit` hits in transcript order.
    let common = search_session_history(&connection, session_id, calling_run, "prompt", 5).unwrap();
    assert_eq!(common.matches.len(), 5);
    let ordinals: Vec<u64> = common
        .matches
        .iter()
        .map(|hit| {
            hit.citation
                .trim_start_matches("user message #")
                .parse()
                .unwrap()
        })
        .collect();
    assert!(
        ordinals.windows(2).all(|pair| pair[0] < pair[1]),
        "{ordinals:?}"
    );
    assert!(ordinals[4] > 3_990, "newest first: {ordinals:?}");

    // A small session searches to the end and is not marked truncated.
    let directory = tempfile::tempdir().unwrap();
    let (connection, session_id) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        5,
        2,
        2,
        128,
    );
    let complete =
        search_session_history(&connection, session_id, calling_run, "no-such-term", 8).unwrap();
    assert!(complete.matches.is_empty());
    assert!(!complete.truncated);
}

/// Every provider request the summarizer sent, in order, with the message
/// bytes each carried (excluding the instruction).
fn summarizer_requests(requests: &[ModelRequest]) -> Vec<(usize, u64)> {
    requests
        .iter()
        .enumerate()
        .filter(|(_, request)| {
            request_texts(request)
                .last()
                .is_some_and(|text| text.starts_with("Summarize this conversation"))
        })
        .map(|(index, request)| {
            let messages = request.messages();
            (
                index,
                crate::measure_messages(&messages[..messages.len() - 1]),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_transcript_several_windows_long_folds_through_bounded_summarizer_steps() {
    // F04: the summarizer used to read the whole transcript. Past the
    // window the provider rejected it, the one attempt was spent, and the
    // prompt failed with "already attempted" — no route to a summary at all.
    // Now each step reads at most a window of whole prompt/run units and
    // commits a summary covering exactly that span; the next step folds the
    // summary with the next chunk until the prompt fits.
    let window: u32 = 32 * 1024;
    // Each run leaves ~20k estimated tokens in the transcript; four of them
    // are ~80k, two and a half windows.
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            AutoCompactScript::Text("c".repeat(run_bytes)),
            AutoCompactScript::Text("d".repeat(run_bytes)),
            // The final prompt's load serves every fold step and then the
            // prompt itself.
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::Text(valid_summary("step one")),
                AutoCompactScript::Text(valid_summary("step two")),
                AutoCompactScript::Text(valid_summary("step three")),
                AutoCompactScript::Text(valid_summary("step four")),
            ]),
        ],
        Some(window),
    )
    .await;
    // Each seed prompt is planned before its run grows the transcript, so
    // the first three fit; the fourth is planned at ~60k and would fold on
    // its own. Seed it through the store instead so the fold under test is
    // the final prompt's, over the whole 80k.
    for prompt in ["one", "two", "three"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    {
        let run = queue_prompt(&harness.runtime, harness.session_id, "four".to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        let _ = observed;
    }
    let last = queue_prompt(&harness.runtime, harness.session_id, "final".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(last)).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == last
        )),
        "the final prompt must proceed after the fold: {:?}",
        observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    let requests = harness.requests.lock().unwrap();
    let steps = summarizer_requests(&requests);
    assert!(
        steps.len() >= 2,
        "a multi-window transcript needs more than one step: {steps:?}"
    );
    let budget =
        crate::sessions::context::summarizer_message_byte_budget(Some(window), 256, 0, 0).unwrap();
    for (index, bytes) in &steps {
        assert!(
            *bytes <= budget,
            "summarizer request {index} carried {bytes} message bytes, over the {budget}-byte window budget"
        );
    }
    // Every request the model saw fit the window: nothing was sent that
    // the estimate said would overflow.
    for (index, request) in requests.iter().enumerate() {
        let bytes = crate::measure_messages(request.messages());
        assert!(
            crate::sessions::context::estimate_tokens(bytes) + 256 <= u64::from(window),
            "request {index} ({bytes} bytes) was sent over the window"
        );
    }
    // The final prompt's context is the last summary plus what it did not
    // cover; the raw "a" run is gone.
    let final_request = requests.last().unwrap();
    let texts = request_texts(final_request);
    assert!(texts.iter().any(|text| text.contains("step")), "{texts:?}");
    assert!(
        !texts
            .iter()
            .any(|text| text.contains(&"a".repeat(run_bytes)))
    );
}

#[tokio::test]
async fn one_run_larger_than_the_window_fails_as_irreducible_not_already_attempted() {
    // A single prompt/run unit that alone exceeds the summarizer's budget
    // cannot be cut anywhere. The step is still sent — the estimate is
    // conservative — but when the provider rejects it the prompt fails
    // naming the unit and its size, not a spent retry, and the same
    // request is never sent twice.
    let window: u32 = 32 * 1024;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(40 * 1024 * 4)),
            AutoCompactScript::ContextOverflow,
        ],
        Some(window),
    )
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let second = queue_prompt(&harness.runtime, harness.session_id, "again".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == second => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    let RunOutcome::Failed {
        failure: RunFailure { kind, message },
    } = outcome
    else {
        panic!("{outcome:?}")
    };
    assert_eq!(kind, RunFailureKind::Policy);
    assert!(
        message.contains("one earlier prompt and its run measure"),
        "{message}"
    );
    assert!(
        !message.contains("summarizer step"),
        "an oversized unit is not a spent retry: {message}"
    );
    // Seed, one summarizer attempt, nothing else.
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_bounded_step_that_fails_stops_the_fold_without_repeating_its_input() {
    // The first bounded step commits; the next prompt's step is rejected by
    // the provider. No further step may run for that prompt: it would read
    // exactly the input the failed one did. The prompt fails naming the
    // step it spent, and the fold's cutoff never moved backwards.
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            // Prompt three is planned at ~40k: over the window. Step one
            // covers "a" and commits. The remaining ~20k plus the summary
            // fit, so the prompt sends — and its own request is rejected by
            // the provider, which is a known overflow for the retry.
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::Text(valid_summary("step one")),
                AutoCompactScript::ContextOverflow,
            ]),
            // The retry compacts first: step two ("b" folded with the
            // summary) is rejected, and nothing further may be sent.
            AutoCompactScript::ContextOverflow,
        ],
        Some(window),
    )
    .await;
    for prompt in ["one", "two"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let third = queue_prompt(&harness.runtime, harness.session_id, "three".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(third)).await;
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
            .count(),
        1
    );
    let last = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(last)).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. })),
        "the second step fails and commits nothing"
    );
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == last => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(
            &outcome,
            RunOutcome::Failed { failure: RunFailure { kind: RunFailureKind::Policy, message } }
                if message.contains("1 automatic compaction step did not produce")
        ),
        "{outcome:?}"
    );
    // Two seeds; step one; prompt three's rejected send; step two. The
    // retry itself never reached the provider.
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    let steps = summarizer_requests(&requests);
    assert_eq!(steps.len(), 2, "{steps:?}");
    // The failed step read new input, not the first step's.
    let first_step = request_texts(&requests[steps[0].0]);
    let second_step = request_texts(&requests[steps[1].0]);
    assert!(
        first_step
            .iter()
            .any(|text| text.contains(&"a".repeat(run_bytes)))
    );
    assert!(
        !first_step
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
    assert!(
        second_step.iter().any(|text| text.contains("step one")),
        "{second_step:?}"
    );
    assert!(
        second_step
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
}

#[tokio::test]
async fn manual_compaction_of_an_oversized_transcript_reads_one_window_at_a_time() {
    // `/compact` on a transcript past the window used to send the whole
    // thing and fail. It now summarizes the first window of whole units; a
    // second `/compact` folds the rest.
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            AutoCompactScript::Text(valid_summary("first half")),
            AutoCompactScript::Text(valid_summary("second half")),
        ],
        Some(window),
    )
    .await;
    // Two prompts of ~20k tokens: the second is planned at ~20k + system,
    // under the 90 % proactive line, so no automatic compaction runs here.
    for prompt in ["one", "two"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let first = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(first)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == first
    )));
    let second = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(second)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == second
    )));
    let requests = harness.requests.lock().unwrap();
    let steps = summarizer_requests(&requests);
    assert_eq!(steps.len(), 2, "{steps:?}");
    let budget =
        crate::sessions::context::summarizer_message_byte_budget(Some(window), 256, 0, 0).unwrap();
    assert!(steps[0].1 <= budget, "{steps:?}");
    // The second /compact folds the first summary with the "b" run.
    let texts = request_texts(&requests[steps[1].0]);
    assert!(
        texts.iter().any(|text| text.contains("first half")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
    assert!(
        !texts
            .iter()
            .any(|text| text.contains(&"a".repeat(run_bytes)))
    );
}

#[tokio::test]
async fn a_committed_step_survives_shutdown_and_the_next_prompt_folds_from_its_marker() {
    // Step one commits and the prompt, now fitting, is sent and stalls; the
    // runtime shuts down and settles it cancelled. Step one's marker is
    // durable and its step count stays on the cancelled prompt. After
    // reopen a new prompt is planned over the summary plus the "b" run;
    // when that grows past the window, its fold starts from the marker —
    // reading the summary and "b", never "a" again.
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            // Prompt three is planned at ~40k: step one covers "a" and
            // commits; the remainder fits, so the prompt sends — and stalls.
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::Text(valid_summary("step one")),
                AutoCompactScript::Stall,
            ]),
        ],
        Some(window),
    )
    .await;
    for prompt in ["one", "two"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let prompt = queue_prompt(&harness.runtime, harness.session_id, "three".to_owned()).await;
    let observed = collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == prompt),
    )
    .await;
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
            .count(),
        1,
        "exactly one bounded step ran before the prompt fit"
    );
    let after = observed.last().unwrap().cursor;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.requests.lock().unwrap().len() < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let workspace_id = harness.workspace_id;
    let session_id = harness.session_id;
    let database_path = harness.workspace_path.join("sessions.sqlite3");
    harness.runtime.close().await.unwrap();
    drop(harness.runtime);

    let connection = Connection::open(&database_path).unwrap();
    let (status, steps, cutoff, markers): (String, u32, u64, u32) = connection
        .query_row(
            "SELECT r.status, r.context_compaction_attempted,
                    (SELECT cutoff_ordinal FROM session_compactions
                     WHERE session_id = r.session_id ORDER BY rowid DESC LIMIT 1),
                    (SELECT COUNT(*) FROM session_compactions WHERE session_id = r.session_id)
             FROM runs r WHERE r.id = ?1",
            [prompt.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        status, "cancelled",
        "shutdown settles the interrupted prompt"
    );
    assert_eq!(steps, 1);
    assert_eq!(markers, 1);
    assert_eq!(cutoff, 1, "step one's marker covers exactly prompt one");
    drop(connection);

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(AutoCompactLoader {
            requests: Arc::clone(&requests),
            scripts: vec![
                // Grows the transcript past the window again.
                AutoCompactScript::Text("c".repeat(run_bytes)),
                AutoCompactScript::Sequence(vec![
                    AutoCompactScript::Text(valid_summary("step two")),
                    AutoCompactScript::Text(valid_summary("step three")),
                ]),
            ],
            loads: StdMutex::new(0),
            context_window: Some(window),
            max_output_tokens: 256,
            provider_identity: true,
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
    let grow = queue_prompt(&runtime, session_id, "four".to_owned()).await;
    collect_until(&mut events, finished_for(grow)).await;
    let retry = queue_prompt(&runtime, session_id, "five".to_owned()).await;
    let observed = collect_until(&mut events, finished_for(retry)).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if *run_id == retry
        )),
        "{:?}",
        observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    let resumed = {
        let requests = requests.lock().unwrap();
        let steps = summarizer_requests(&requests);
        assert!(!steps.is_empty(), "the new prompt folds the remainder");
        request_texts(&requests[steps[0].0])
    };
    assert!(
        resumed.iter().any(|text| text.contains("step one")),
        "{resumed:?}"
    );
    assert!(
        resumed
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
    assert!(
        !resumed
            .iter()
            .any(|text| text.contains(&"a".repeat(run_bytes))),
        "the fold must not reread what step one already summarized"
    );
    runtime.close().await.unwrap();
}
