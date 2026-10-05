use super::*;

#[tokio::test]
async fn a_compaction_run_reports_compacting_and_nothing_else() {
    // CX4: a compaction run is one activity from start to finish. A
    // snapshot taken while the summarizer is at the model already names it,
    // and the summarizer's own provider activity is never published.
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("hello".to_owned()),
        AutoCompactScript::Text(valid_summary("work so far")),
        AutoCompactScript::Text("hello again".to_owned()),
        AutoCompactScript::Stall,
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "hi".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
    assert_eq!(
        finished_outcome(&observed, compaction),
        Some(RunOutcome::Completed)
    );
    let activities: Vec<(RunId, RunActivity)> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunActivityChanged { run_id, activity } => Some((*run_id, *activity)),
            _ => None,
        })
        .collect();
    assert_eq!(activities, [(compaction, RunActivity::Compacting)]);
    // It is reported by the transaction that starts the run, so a run
    // cancelled before its first poll still says what it was.
    let started = observed
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::RunStarted { run_id, .. } if *run_id == compaction))
        .unwrap();
    assert!(matches!(
        observed[started + 1].event,
        SessionEvent::RunActivityChanged {
            activity: RunActivity::Compacting,
            ..
        }
    ));
    assert_eq!(
        observed[started + 1].cursor.sequence,
        observed[started].cursor.sequence + 1
    );

    // A second compaction parks at the model; the snapshot says why.
    let second = queue_prompt(&harness.runtime, harness.session_id, "more".to_owned()).await;
    collect_until(&mut harness.events, finished_for(second)).await;
    let parked = compact_session(&harness.runtime, harness.session_id).await;
    collect_until(&mut harness.events, |event| {
        matches!(
            event,
            SessionEvent::RunActivityChanged { run_id, activity: RunActivity::Compacting }
                if *run_id == parked
        )
    })
    .await;
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let summary = snapshot
        .sessions
        .iter()
        .find(|session| session.id == harness.session_id)
        .unwrap();
    assert_eq!(summary.active_run_id, Some(parked));
    assert_eq!(summary.activity, Some(RunActivity::Compacting));
}

#[tokio::test]
async fn compact_session_is_refused_while_active_and_never_runs_a_summarizer_tool_call() {
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
    // machinery. Its request declares the session's tools so it shares the
    // provider cache (ADR-0056 § 5), but nothing runs: the call is rejected
    // and the model asked again. This provider then answers "done", which is
    // no summary, so the step fails closed on validation.
    let request_count_before = harness.requests.lock().unwrap().len();
    let compaction_run = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_finished(&mut harness.events).await;
    match finished_outcome(&observed, compaction_run) {
        Some(RunOutcome::Failed { failure }) => {
            assert_eq!(failure.kind, RunFailureKind::Policy, "{failure:?}");
            assert!(failure.message.contains("missing required sections"));
        }
        other => panic!("expected a policy failure, got {other:?}"),
    }
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
    assert_eq!(requests.len(), request_count_before + 2);
    let prompt = &requests[request_count_before - 1];
    let summary = &requests[request_count_before];
    assert_eq!(summary.system(), prompt.system());
    assert_eq!(summary.tools(), prompt.tools());
    // The retry answers the rejected call with its rejection.
    let retry = requests.last().unwrap();
    assert!(
        retry
            .messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.starts_with("not executed: this is a compaction request")
            ))
    );
    // It still carries the session's tool history.
    assert!(
        summary
            .messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. }
                )
            })
    );
    // The summarizer loaded through the ordinary loader path.
    assert_eq!(harness.models.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_between_run_summarizer_request_extends_the_prompt_request_it_follows() {
    // ADR-0056 § 5: the summarizer sends the prompt run's system prompt and
    // tools, and its messages are the session context the prompt run last
    // sent plus that run's reply, then the instruction. A provider prefix
    // cache therefore covers everything but the instruction.
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::ReadNoteThenText("read it".to_owned()),
        AutoCompactScript::Text(valid_summary("folded")),
    ])
    .await;
    std::fs::write(harness.workspace_path.join("note.txt"), "a short note\n").unwrap();
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "read the note".to_owned(),
    )
    .await;
    collect_until(&mut harness.events, finished_for(run)).await;
    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    assert_eq!(
        finished_outcome(&observed, compaction),
        Some(RunOutcome::Completed)
    );

    let requests = harness.requests.lock().unwrap();
    let (prompt, summarizer) = (&requests[requests.len() - 2], requests.last().unwrap());
    assert!(prompt.system().is_some());
    assert_eq!(summarizer.system(), prompt.system());
    assert_eq!(summarizer.tools(), prompt.tools());
    assert!(!summarizer.tools().is_empty());
    assert_eq!(summarizer.reasoning_effort(), prompt.reasoning_effort());
    assert_eq!(summarizer.tool_choice(), prompt.tool_choice());
    let (instruction, prefix) = summarizer.messages().split_last().unwrap();
    assert_eq!(request_texts_of(instruction), [COMPACTION_INSTRUCTION]);
    assert!(
        prefix.starts_with(prompt.messages()),
        "the summarizer must extend the prompt run's last request"
    );
    assert_eq!(
        prefix.len(),
        prompt.messages().len() + 1,
        "plus the run's final reply"
    );
}

fn request_texts_of(message: &Message) -> Vec<&str> {
    message
        .content()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_summarizer_that_calls_a_tool_is_answered_once_and_its_reply_text_is_dropped() {
    let summary = valid_summary("after one rejected call");
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::ToolCallWithText {
                text: "let me look first".to_owned(),
                tool: "read_file".to_owned(),
            },
            AutoCompactScript::Text(summary.clone()),
        ]),
    ])
    .await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "one".to_owned()).await;
    collect_until(&mut harness.events, finished_for(run)).await;
    std::fs::write(harness.workspace_path.join("canary"), "untouched").unwrap();
    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    assert_eq!(
        finished_outcome(&observed, compaction),
        Some(RunOutcome::Completed)
    );
    assert!(!observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolCallRequested { .. } | SessionEvent::ToolCallFinished { .. }
    )));

    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let stored: String = connection
        .query_row(
            "SELECT summary FROM session_compactions WHERE run_id = ?1",
            [compaction.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        stored.starts_with(&format!("{summary}\n\n{COMPACTION_RECORD_HEADER}")),
        "{stored}"
    );
    assert!(!stored.contains("let me look first"));
    let calls: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1",
            [compaction.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(calls, 0, "a summarizer call is never recorded as work");
    let requests = harness.requests.lock().unwrap();
    let retry = requests.last().unwrap();
    assert!(
        retry
            .messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.starts_with("not executed: this is a compaction request")
            ))
    );
}

#[tokio::test]
async fn a_rejected_call_turn_after_a_cut_reply_drops_the_abandoned_fragment() {
    // Turn one is cut mid-reply; turn two continues it but calls a tool and
    // is rejected; turn three writes the summary. The stored summary is
    // turn three alone: the fragment of the abandoned reply is not joined to
    // it.
    let summary = valid_summary("clean");
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::Truncated("1. Intent: abandoned frag".to_owned()),
            AutoCompactScript::ToolCallWithText {
                text: "ment".to_owned(),
                tool: "read_file".to_owned(),
            },
            AutoCompactScript::Text(summary.clone()),
        ]),
    ])
    .await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "one".to_owned()).await;
    collect_until(&mut harness.events, finished_for(run)).await;
    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    assert_eq!(
        finished_outcome(&observed, compaction),
        Some(RunOutcome::Completed)
    );
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let stored: String = connection
        .query_row(
            "SELECT summary FROM session_compactions WHERE run_id = ?1",
            [compaction.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        stored.starts_with(&format!("{summary}\n\n{COMPACTION_RECORD_HEADER}")),
        "{stored}"
    );
    assert!(!stored.contains("abandoned"));
}

#[tokio::test]
async fn a_summarizer_that_calls_tools_on_two_turns_fails_closed() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::ToolCallWithText {
                text: String::new(),
                tool: "read_file".to_owned(),
            },
            AutoCompactScript::ToolCallWithText {
                text: String::new(),
                tool: "shell".to_owned(),
            },
        ]),
        AutoCompactScript::Text("after".to_owned()),
    ])
    .await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "one".to_owned()).await;
    collect_until(&mut harness.events, finished_for(run)).await;
    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
    match finished_outcome(&observed, compaction) {
        Some(RunOutcome::Failed { failure }) => {
            assert_eq!(failure.kind, RunFailureKind::ProviderProtocol);
            assert!(
                failure.message.contains("called a tool on two turns"),
                "{failure:?}"
            );
        }
        other => panic!("expected a protocol failure, got {other:?}"),
    }
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    // Exactly the prompt, the first summarizer turn, and its one retry.
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
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
    // The stored summary is the narrative, then the record; the published
    // size is the assembly it produces.
    let summary_excerpt = summary_excerpt.unwrap();
    assert!(
        summary_excerpt.starts_with(&format!(
            "{}\n\n{COMPACTION_RECORD_HEADER}\n",
            valid_summary("hello")
        )),
        "{summary_excerpt}"
    );
    assert_eq!(
        after_bytes,
        (COMPACTION_SUMMARY_PREAMBLE.len() + 2 + summary_excerpt.len()) as u64
    );
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
        // fixed instruction. QQ renders the record itself, so the
        // instruction carries no seeded data.
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[1]);
        assert!(texts.iter().any(|text| text == "first prompt"));
        let instruction = texts.last().unwrap();
        assert_eq!(instruction, COMPACTION_INSTRUCTION);
    }

    submit_prompt(&harness, "second prompt").await;
    collect_through_finished(&mut harness.events).await;
    {
        // Assembly is now summary + verbatim span after the marker; the
        // original prompt survives only inside the summary's record.
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[2]);
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert!(texts[0].contains("--- user message #1 ---\nfirst prompt\n"));
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

/// A store with one session and `prompts` completed prompt runs, each with a
/// final reply. Returns the connection, session, and run ids in order.
fn record_fixture(prompts: &[String]) -> (tempfile::TempDir, Connection, SessionId, Vec<RunId>) {
    let directory = tempfile::tempdir().unwrap();
    let (connection, _) = open_database(&directory.path().join("sessions.sqlite3")).unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/record')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, title, status, approval_mode,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'Record', 'idle', 'ask', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    let mut runs = Vec::new();
    let mut ordinal = 0_u64;
    for (index, prompt) in prompts.iter().enumerate() {
        let run_id = RunId::generate().unwrap();
        let user = MessageId::generate().unwrap();
        let assistant = MessageId::generate().unwrap();
        connection
            .execute(
                "INSERT INTO runs(
                     id, session_id, command_id, user_message_id, assistant_message_id,
                     status, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'completed', 1)",
                params![
                    run_id.to_string(),
                    session_id.to_string(),
                    CommandId::generate().unwrap().to_string(),
                    user.to_string(),
                    assistant.to_string(),
                ],
            )
            .unwrap();
        for (id, role, text) in [
            (user, "user", prompt.clone()),
            (assistant, "assistant", format!("reply {index}")),
        ] {
            ordinal += 1;
            connection
                .execute(
                    "INSERT INTO messages(
                         id, session_id, run_id, ordinal, turn_ordinal, role, state,
                         output, refusal, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 1, ?5, 'complete', ?6, '', 1)",
                    params![
                        id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal,
                        role,
                        text,
                    ],
                )
                .unwrap();
        }
        runs.push(run_id);
    }
    (directory, connection, session_id, runs)
}

fn insert_record_call(
    connection: &Connection,
    run_id: RunId,
    call: u32,
    name: &str,
    arguments: serde_json::Value,
    state: &str,
    result: &str,
) {
    connection
        .execute(
            "INSERT INTO tool_calls(
                 id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                 arguments_json, state, result, is_error, requested_at_ms
             ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1)",
            params![
                ToolCallId::generate().unwrap().to_string(),
                run_id.to_string(),
                call,
                format!("call-{call}"),
                name,
                arguments.to_string(),
                state,
                result,
                state == "failed",
            ],
        )
        .unwrap();
}

#[test]
fn the_compaction_record_is_exact_bounded_and_cites_what_it_omits() {
    let prompts: Vec<String> = (0..40)
        .map(|index| format!("USER {index:02} 目录 {}", "é".repeat(1_500)))
        .collect();
    let (_directory, connection, session_id, runs) = record_fixture(&prompts);
    let last = *runs.last().unwrap();
    insert_record_call(
        &connection,
        last,
        1,
        "read_file",
        serde_json::json!({"path": "src/read.rs"}),
        "completed",
        "read src/read.rs",
    );
    insert_record_call(
        &connection,
        last,
        2,
        "edit_file",
        serde_json::json!({"edits": [{"path": "src/edited.rs", "old": "a", "new": "b"}]}),
        "completed",
        "edited",
    );
    insert_record_call(
        &connection,
        last,
        3,
        "write_file",
        serde_json::json!({"path": "src/read.rs", "content": "x"}),
        "completed",
        "wrote",
    );
    insert_record_call(
        &connection,
        last,
        4,
        "edit_file",
        serde_json::json!({"edits": [{"path": "src/preview.rs"}], "dry_run": true}),
        "completed",
        "preview",
    );
    insert_record_call(
        &connection,
        last,
        5,
        "shell",
        serde_json::json!({"command": "cargo test"}),
        "failed",
        "exit status 101: error[E0433]: failed to resolve\nmore detail",
    );
    insert_record_call(
        &connection,
        last,
        6,
        "search",
        serde_json::json!({"query": "x"}),
        "failed",
        "not executed: this reply was the slice checkpoint",
    );
    let cutoff = u64::try_from(prompts.len() * 2).unwrap();

    let record = render_compaction_record(
        &connection,
        session_id,
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        context::COMPACTION_RECORD_BYTES,
    )
    .unwrap();

    assert!(
        record.len() <= context::COMPACTION_RECORD_BYTES,
        "{}",
        record.len()
    );
    assert!(record.starts_with(COMPACTION_RECORD_HEADER));
    // The newest prompts are verbatim; the oldest are cited for recall.
    assert!(record.contains(&format!("--- user message #79 ---\n{}\n", prompts[39])));
    assert!(!record.contains(&prompts[0]));
    assert!(
        record.contains("search_history searches user messages #1, #3"),
        "{record}"
    );
    // Kept messages print oldest first.
    let older = record.find("USER 38").unwrap();
    let newer = record.find("USER 39").unwrap();
    assert!(older < newer);
    assert!(record.contains("Last assistant reply, to user message #79:\nreply 39\n"));
    // Written files are modified, read-only files are listed once, a dry run
    // modifies nothing.
    assert!(
        record.contains("Files modified:\n- src/edited.rs\n- src/read.rs\n"),
        "{record}"
    );
    assert!(!record.contains("Files read, not modified"));
    assert!(!record.contains("src/preview.rs"));
    // Failures keep the first line verbatim; runtime rejections are not
    // failures.
    assert!(record.contains(
        "- shell: exit status 101: error[E0433]: failed to resolve (user message #79 turn 1)"
    ));
    assert!(!record.contains("more detail"));
    assert!(!record.contains("slice checkpoint"));

    // A small window shrinks the record proportionally, on a char boundary.
    let small = render_compaction_record(
        &connection,
        session_id,
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        record_budget(Some(16 * 1024)),
    )
    .unwrap();
    assert_eq!(record_budget(Some(16 * 1024)), 8 * 1024);
    assert_eq!(record_budget(None), context::COMPACTION_RECORD_BYTES);
    assert!(small.len() <= 8 * 1024, "{}", small.len());
    assert!(
        small.contains("USER 39"),
        "the newest message survives a small record"
    );
    assert!(small.contains("search_history searches user messages"));

    // One message larger than the whole budget is cut, not dropped.
    let (_directory, connection, session_id, _) = record_fixture(&["z".repeat(100_000)]);
    let cut = render_compaction_record(
        &connection,
        session_id,
        RecordScope::Session { cutoff_ordinal: 2 },
        context::COMPACTION_RECORD_BYTES,
    )
    .unwrap();
    assert!(cut.len() <= context::COMPACTION_RECORD_BYTES);
    assert!(cut.contains("[cut for space; search_history matches against the full message]"));

    // Nothing to record renders nothing.
    let (_directory, connection, session_id, _) = record_fixture(&[]);
    assert_eq!(
        render_compaction_record(
            &connection,
            session_id,
            RecordScope::Session { cutoff_ordinal: 0 },
            context::COMPACTION_RECORD_BYTES,
        )
        .unwrap(),
        ""
    );
}

fn insert_record_steering(
    connection: &Connection,
    session_id: SessionId,
    run: RunId,
    ordinal: u64,
    turn: u32,
    text: &str,
) {
    connection
        .execute(
            "INSERT INTO messages(
                 id, session_id, run_id, ordinal, turn_ordinal, role, state,
                 output, refusal, created_at_ms, steering
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'user', 'complete', ?6, '', 1, 1)",
            params![
                MessageId::generate().unwrap().to_string(),
                session_id.to_string(),
                run.to_string(),
                ordinal,
                turn,
                text,
            ],
        )
        .unwrap();
}

#[test]
fn an_in_run_record_holds_the_replaced_turns_steering_files_and_failures() {
    let (_directory, connection, session_id, runs) = record_fixture(&["do the task".to_owned()]);
    let run = runs[0];
    // Turns 1, 2, then a gap to 5 and 6. A cutoff of 2 keeps turns 5 and 6;
    // replay drops the steering applied up to the first kept turn (5), so
    // the record must hold exactly that steering.
    for turn in [1, 2, 5, 6] {
        connection
            .execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                 VALUES (?1, ?2, '[]')",
                params![run.to_string(), turn],
            )
            .unwrap();
    }
    for (ordinal, turn, text) in [
        (11, 1, "steer one"),
        (12, 3, "steer in the gap"),
        (13, 5, "steer before kept"),
        (14, 6, "steer later"),
    ] {
        insert_record_steering(&connection, session_id, run, ordinal, turn, text);
    }
    for (turn, name, arguments, state, result) in [
        (
            1,
            "write_file",
            serde_json::json!({"path": "a.rs", "content": ""}),
            "completed",
            "ok",
        ),
        (
            2,
            "shell",
            serde_json::json!({"command": "make"}),
            "failed",
            "\nmake: *** [all] Error 2",
        ),
        (
            5,
            "write_file",
            serde_json::json!({"path": "kept.rs", "content": ""}),
            "completed",
            "ok",
        ),
    ] {
        connection
            .execute(
                "INSERT INTO tool_calls(
                     id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                     arguments_json, state, result, is_error, requested_at_ms
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, 1)",
                params![
                    ToolCallId::generate().unwrap().to_string(),
                    run.to_string(),
                    turn,
                    format!("call-{turn}"),
                    name,
                    arguments.to_string(),
                    state,
                    result,
                    state == "failed",
                ],
            )
            .unwrap();
    }
    let render = |turn_cutoff| {
        render_compaction_record(
            &connection,
            session_id,
            RecordScope::Run {
                run_id: run,
                turn_cutoff,
            },
            context::COMPACTION_RECORD_BYTES,
        )
        .unwrap()
    };

    let record = render(2);
    assert!(record.starts_with(COMPACTION_RECORD_HEADER));
    for kept in ["steer one", "steer in the gap", "steer before kept"] {
        assert_eq!(record.matches(kept).count(), 1, "{kept}: {record}");
    }
    assert!(record.contains("--- steering before turn 5 ---\nsteer before kept\n"));
    assert!(!record.contains("steer later"));
    // The prompt and the last reply stay verbatim in the kept transcript.
    assert!(!record.contains("do the task"));
    assert!(!record.contains("Last assistant reply"));
    assert!(record.contains("Files modified:\n- a.rs\n"));
    assert!(!record.contains("kept.rs"));
    // The first non-empty line of a failure is kept.
    assert!(
        record.contains("- shell: make: *** [all] Error 2 (turn 2)"),
        "{record}"
    );

    // With no kept turn, replay drops steering through the cutoff only, and
    // the record matches it: nothing is duplicated into a later turn.
    let record = render(6);
    assert!(record.contains("steer later"));
    assert!(record.contains("kept.rs"));
}

#[test]
fn small_records_stay_within_budget_and_end_on_a_line() {
    let prompts: Vec<String> = (0..12)
        .map(|index| format!("prompt {index} {}", "x".repeat(900)))
        .collect();
    let (_directory, connection, session_id, runs) = record_fixture(&prompts);
    insert_record_steering(&connection, session_id, runs[11], 100, 2, "steer late");
    for call in 1..=200 {
        insert_record_call(
            &connection,
            runs[11],
            call,
            if call % 2 == 0 {
                "read_file"
            } else {
                "write_file"
            },
            serde_json::json!({"path": format!("src/{call:04}/{}.rs", "p".repeat(80)), "content": ""}),
            "completed",
            "ok",
        );
    }
    for call in 201..=260 {
        insert_record_call(
            &connection,
            runs[11],
            call,
            "shell",
            serde_json::json!({"command": "x"}),
            "failed",
            &format!("error {call}: {}", "e".repeat(200)),
        );
    }
    let cutoff = 200;
    for window in [
        0, 1_024, 2_048, 4_096, 6_000, 16_384, 32_768, 131_072, 1_000_000,
    ] {
        let budget = record_budget(Some(window));
        let record = render_compaction_record(
            &connection,
            session_id,
            RecordScope::Session {
                cutoff_ordinal: cutoff,
            },
            budget,
        )
        .unwrap();
        assert!(
            record.len() <= budget,
            "{window}: {} > {budget}",
            record.len()
        );
        if record.is_empty() {
            assert!(
                budget < 2 * 1024,
                "{window}: only a tiny budget renders nothing"
            );
            continue;
        }
        assert!(record.starts_with(COMPACTION_RECORD_HEADER), "{window}");
        assert!(record.ends_with('\n'), "{window}: {record}");
        // The newest user message (the late steering) always survives.
        assert!(record.contains("steer late"), "{window}: {record}");
    }
    let record = render_compaction_record(
        &connection,
        session_id,
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        record_budget(Some(16_384)),
    )
    .unwrap();
    // Lists that do not fit say how much they left out.
    assert!(record.contains(" more\n"), "{record}");
    assert!(record.contains(" older\n"), "{record}");
    // Session-scope steering is labelled with its prompt and turn.
    let full = render_compaction_record(
        &connection,
        session_id,
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        context::COMPACTION_RECORD_BYTES,
    )
    .unwrap();
    assert!(
        full.contains("--- steering during user message #23 turn 2 ---\nsteer late\n"),
        "{full}"
    );
}

#[test]
fn the_summarizer_output_cap_is_the_run_cap_bounded_by_the_window() {
    // No window: the run's own cap, which used to be clamped at 8 192.
    assert_eq!(context::summarizer_output_tokens(32_768, None), 32_768);
    // A large window allows the run cap up to an eighth of the window.
    assert_eq!(
        context::summarizer_output_tokens(16_384, Some(272_000)),
        16_384
    );
    assert_eq!(
        context::summarizer_output_tokens(128_000, Some(272_000)),
        34_000
    );
    // A small window keeps at least 8 192, as before, when the run allows it.
    assert_eq!(
        context::summarizer_output_tokens(16_384, Some(16_384)),
        8_192
    );
    assert_eq!(
        context::summarizer_output_tokens(1_024, Some(16_384)),
        1_024
    );
}

#[test]
fn a_summary_that_echoes_a_record_keeps_only_its_narrative() {
    let narrative = valid_summary("kept");
    assert_eq!(
        narrative_of(format!(
            "{narrative}\n\n{COMPACTION_RECORD_HEADER}\nUser messages, verbatim:\nstale"
        )),
        narrative
    );
    assert_eq!(narrative_of(narrative.clone()), narrative);
    // Prose that mentions the header mid-line is not a record.
    let mentions = format!("{narrative}\nSee {COMPACTION_RECORD_HEADER} below.");
    assert_eq!(narrative_of(mentions.clone()), mentions);
    // A narrative fits only if the largest record still fits beside it.
    assert!(
        validate_compaction_summary(&format!(
            "{}\n{}",
            valid_summary("x"),
            "s".repeat(MAX_CONTEXT_BYTES - context::COMPACTION_RECORD_BYTES)
        ))
        .unwrap_err()
        .contains("4 MiB")
    );
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
    // The summarizer asks for the run's own resolved cap. It used to clamp
    // at 8 192, which cut every long summary into a continuation turn
    // (ADR-0056).
    for (configured, expected) in [(1_024, 1_024), (16_384, 16_384)] {
        // The prior answer leaves the prompt run over the storage backstop
        // (its reserve is the configured cap at 32 B/token plus the
        // compaction envelope) while the summarizer request, which reserves
        // the effective cap, still fits.
        let prior = MAX_CONTEXT_BYTES - 100 * 1024 - usize::try_from(configured).unwrap() * 32;
        let mut harness = auto_compact_harness_with_limits(
            vec![
                AutoCompactScript::Text("x".repeat(prior)),
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
            assert_eq!(requests[1].tools(), requests[0].tools());
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
    // The watermark already covers the history, so the threshold seam has
    // nothing to move and the injected failure hits the compaction reload.
    mark_prune_seam(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );

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
    mark_prune_seam(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );
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
                    ContentBlock::ToolResult { content, .. } if content.contains("\n[pruned: ")
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
    // A transport fault before any summary text streamed: the run's own
    // recovery re-issues the turn until its allowance is spent, then the
    // compaction settles paused (no marker committed).
    assert!(failed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Paused { .. },
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
        4 + usize::from(crate::MAX_TURN_RETRIES),
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
        // One loaded provider serves the failed summarizer step (six sends:
        // the fault and its turn retries) and then the downgraded prompt.
        AutoCompactScript::Sequence(
            std::iter::repeat_n(
                AutoCompactScript::Fail,
                1 + usize::from(crate::MAX_TURN_RETRIES),
            )
            .chain([AutoCompactScript::Text("recovered".to_owned())])
            .collect(),
        ),
        AutoCompactScript::Text("must not be polled".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "big ask".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let second = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
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
    // The fold is exhausted and the provider already rejected this shape:
    // the prompt is admitted from the (absent) summary alone rather than
    // refused (RR6), and its downgraded request reaches the provider once.
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    // One overflow, the failed summarizer's attempt plus its turn retries,
    // then the downgraded prompt. The known-overflowing shape itself was
    // never resent.
    let requests = harness.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3 + usize::from(crate::MAX_TURN_RETRIES),
        "the second prompt must not repeat a provider-known overflow"
    );
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE));
    assert!(!texts.iter().any(|text| text == "big ask"));
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
async fn a_prompt_still_over_budget_after_compacting_runs_from_the_summary_alone() {
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(20 * 1024 * 4)),
            // Pathological summarizer: the summary is as large as the
            // transcript it replaces. Validation rejects it, so no marker
            // commits and the retry is still past the model window.
            AutoCompactScript::Text(valid_summary(&"s".repeat(20 * 1024 * 4))),
            AutoCompactScript::Text("recovered".to_owned()),
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
    // ...and the prompt then runs from the summary alone (RR6): no marker
    // ever committed, so the opening carries the notice without a summary,
    // and the 80 KiB run is not in the request.
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == second => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let texts = request_texts(requests.last().unwrap());
    assert_eq!(texts.len(), 2, "{}", texts.len());
    assert_eq!(texts[0], crate::sessions::SUMMARY_ONLY_NOTICE);
    assert_eq!(texts[1], "y".repeat(10 * 1024 * 4));
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
        // One loaded provider serves the failed summarizer step (six sends:
        // the fault and its turn retries) and then the downgraded prompt.
        AutoCompactScript::Sequence(
            std::iter::repeat_n(
                AutoCompactScript::Fail,
                1 + usize::from(crate::MAX_TURN_RETRIES),
            )
            .chain([AutoCompactScript::Text("done".to_owned())])
            .collect(),
        ),
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
    // The summarizer's transport fault was retried and then paused: nothing
    // committed...
    let compaction = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } if *run_id != second => Some(*run_id),
            _ => None,
        })
        .unwrap();
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Paused { .. }, .. }
            if *run_id == compaction
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    // ...and the unchanged overflowing prompt is not refused after that one
    // attempt: it runs from the summary alone (RR6). No marker exists, so
    // the opening is the bare notice and the over-threshold run is absent.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 3 + usize::from(crate::MAX_TURN_RETRIES));
    let texts = request_texts(requests.last().unwrap());
    assert_eq!(texts[0], crate::sessions::SUMMARY_ONLY_NOTICE);
    assert!(!texts.iter().any(|text| text.starts_with("grow")));
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
    // The attempt did not shrink the assembly; the prompt runs from the
    // summary alone instead of looping or failing (RR6).
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
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
        if prompt == "five" {
            // A seam before the last prompt: assembly stubs up to it.
            mark_prune_seam(
                &harness.workspace_path.join("sessions.sqlite3"),
                harness.session_id,
            );
        }
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
    // The old read is a stub: its header without the hash, then the tool,
    // arguments, size and how to get the text back.
    let (header, stub) = results[0].split_once('\n').unwrap_or_else(|| {
        panic!(
            "stale read-only result must be stubbed, got {:?}",
            results[0]
        )
    });
    assert!(header.starts_with("read note.txt L"), "{header}");
    assert!(
        !header.contains(" h:"),
        "a pruned read must not offer its hash: {header}"
    );
    assert!(
        stub.starts_with("[pruned: read_file {\"path\":\"note.txt\"} returned"),
        "stale read-only result must be stubbed, got {:?}",
        results[0]
    );
    assert!(stub.ends_with("without if_changed_since]"));
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
             OPEN PROBLEMS: a\n5) Next step: run it"
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
    assert!(missing.contains("Open problems"));
    assert!(missing.contains("Next step"));
    assert!(!missing.contains("Intent"));
    // Body text mentioning a heading word does not satisfy the section.
    let prose = validate_compaction_summary(
        "1. Intent: fix the open problems: they matter\n2. Decisions and constraints: none\n\
         3. Work state: done\n5. Next step: ship",
    )
    .unwrap_err();
    assert_eq!(
        prose,
        "compaction summary is missing required sections: Open problems"
    );
    // The previous six-section format is not a valid new narrative: a fold
    // must rewrite it into the new sections.
    let old = validate_compaction_summary(&old_format_summary("x")).unwrap_err();
    assert_eq!(
        old,
        "compaction summary is missing required sections: Open problems, Next step"
    );
    // Regression: a markdown heading on its own line with the body beneath
    // it is how models answer the numbered instruction; every section was
    // present in the live store yet all six were reported missing.
    assert!(
        validate_compaction_summary(
            "## 1. Intent\n\nThe user wants QQ to be reliable.\n\n\
             ## 2. Decisions and constraints\n\n- Use the live store.\n\n\
             ## 3. Work state\n\n**Done:**\n- read docs\n\n\
             ## 4. Open problems\n\n- none\n\n\
             ## 5. Next step\n\n1. go find why failure is so high"
        )
        .is_ok(),
        "bare markdown headings without a colon are accepted"
    );
    assert!(
        validate_compaction_summary(
            "**Intent**\nx\n### Decisions and constraints ###\ny\nWork state\nz\n\
             Open problems\na\nNext step\nb"
        )
        .is_ok(),
        "bold, closed atx, and plain headings are accepted"
    );
    // A heading word followed by other prose is still body text.
    let prose_heading = validate_compaction_summary(
        "Intent was unclear\n2. Decisions and constraints: none\n3. Work state: done\n\
         4. Open problems: none\n5. Next step: ship",
    )
    .unwrap_err();
    assert_eq!(
        prose_heading,
        "compaction summary is missing required sections: Intent"
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
async fn a_summary_split_across_truncated_turns_is_joined_without_a_seam() {
    // Regression: the summarizer's turns were joined with a newline even
    // when the provider cut the previous turn mid-token, so a heading split
    // across the cut (`Decis` + `ions and constraints:`) never matched and
    // the compaction failed as a policy error.
    let summary = valid_summary("joined");
    let cut = summary.find("Decis").unwrap() + "Decis".len();
    let second_cut = summary.find("Open").unwrap() + "Op".len();
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::Truncated(summary[..cut].to_owned()),
            AutoCompactScript::Truncated(summary[cut..second_cut].to_owned()),
            AutoCompactScript::Text(summary[second_cut..].to_owned()),
        ]),
        AutoCompactScript::Text("after".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, "one".to_owned()).await;
    collect_until(&mut harness.events, finished_for(first)).await;

    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    let outcome = finished_outcome(&observed, compaction);
    assert!(
        matches!(outcome, Some(RunOutcome::Completed)),
        "{outcome:?}"
    );
    // The prompt plus three summarizer requests: two continuations.
    assert_eq!(harness.requests.lock().unwrap().len(), 4);

    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let stored: String = connection
        .query_row(
            "SELECT summary FROM session_compactions WHERE run_id = ?1",
            [compaction.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        stored.starts_with(&format!("{summary}\n\n{COMPACTION_RECORD_HEADER}\n")),
        "truncated turns are concatenated verbatim, then the record: {stored}"
    );

    // The joined summary is what the next prompt sees.
    let next = queue_prompt(&harness.runtime, harness.session_id, "two".to_owned()).await;
    collect_until(&mut harness.events, finished_for(next)).await;
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
    assert!(texts[0].contains("Decisions and constraints: joined"));
    assert!(texts[0].contains("Open problems: joined"));
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
async fn repeated_compactions_keep_every_user_message_verbatim_without_the_model_retyping_them() {
    // ADR-0056: the record, not the model, carries user messages. This
    // summarizer writes only a fixed narrative and never repeats a user
    // message; every message must still be verbatim in the latest summary
    // after several folds, and history stays bounded.
    struct ForgetfulLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for ForgetfulLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(ForgetfulProvider { requests }, "test-model", 4_096)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ForgetfulProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for ForgetfulProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let texts = request_texts(&request);
            self.requests.lock().unwrap().push(request);
            let summarizing = texts
                .last()
                .is_some_and(|text| text.starts_with("Summarize this conversation"));
            let text = if summarizing {
                // Echo the prior record too, as a careless model might; QQ
                // must drop it and render its own.
                let echoed = texts
                    .first()
                    .and_then(|text| text.find(COMPACTION_RECORD_HEADER).map(|at| &text[at..]))
                    .unwrap_or_default()
                    .to_owned();
                format!("{}\n\n{echoed}", valid_summary("narrative only"))
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
        Arc::new(ForgetfulLoader {
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
        "USER 多字节 «quoted» text survives\n  with indentation",
    ];
    for (round, prompt) in seeded.iter().enumerate() {
        let run = queue_prompt(&runtime, session_id, (*prompt).to_owned()).await;
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
    for (index, fact) in seeded.iter().enumerate() {
        let ordinal = index * 2 + 1;
        assert!(
            texts[0].contains(&format!("--- user message #{ordinal} ---\n{fact}\n")),
            "user message {ordinal} must survive {} folds verbatim; got {}",
            seeded.len(),
            texts[0]
        );
    }
    // One record, rendered by QQ: the echoed copy was dropped.
    assert_eq!(texts[0].matches(COMPACTION_RECORD_HEADER).count(), 1);
    assert_eq!(texts[0].matches("1. Intent: narrative only").count(), 1);
    // Only the latest summary and the verbatim span are assembled.
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
async fn an_old_format_summary_folds_into_the_new_format_even_when_the_record_is_larger() {
    // A session compacted before ADR-0056 holds a small six-section
    // summary that hid a long first message. The next record restores that
    // message from rows, so the assembly grows past what the old summary
    // left. Shrinkage is required of the narrative only, so the fold still
    // commits, in the new format, with every user message exact.
    let hidden = format!("hidden{} end", " alpha".repeat(5_000));
    let span = [
        format!("second{} end", " beta".repeat(1_800)),
        format!("third{} end", " gamma".repeat(1_500)),
    ];
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Text(old_format_summary("old")),
        AutoCompactScript::Text("second answer".to_owned()),
        AutoCompactScript::Text("third answer".to_owned()),
        AutoCompactScript::Text(valid_summary("folded")),
        AutoCompactScript::Text("after".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, hidden.clone()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
    // Write the summary the old code would have committed: six sections and
    // no record, as rows on disk from before the upgrade look.
    let old = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(old)).await;
    assert!(matches!(
        finished_outcome(&observed, old),
        Some(RunOutcome::Failed { .. })
    ));
    let database = harness.workspace_path.join("sessions.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO session_compactions(
                 session_id, run_id, summary, cutoff_ordinal,
                 before_bytes, after_bytes, created_at_ms
             ) VALUES (?1, ?2, ?3, 2, 0, 0, 1)",
            params![
                harness.session_id.to_string(),
                old.to_string(),
                old_format_summary("old"),
            ],
        )
        .unwrap();
    for prompt in &span {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.clone()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let before: u64 = {
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        assert!(
            texts[0].contains("4. Files touched: old"),
            "the old summary replays"
        );
        texts.iter().map(|text| text.len() as u64).sum()
    };

    let fold = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    assert_eq!(
        finished_outcome(&observed, fold),
        Some(RunOutcome::Completed)
    );
    let (before_bytes, after_bytes) = observed
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
    assert!(before_bytes > COMPACTION_SHRINKAGE_FLOOR_BYTES as u64);
    assert!(
        after_bytes > before_bytes,
        "the fixture must make the record outgrow the old assembly: {before_bytes} -> {after_bytes} ({before})"
    );

    let next = queue_prompt(&harness.runtime, harness.session_id, "next".to_owned()).await;
    collect_until(&mut harness.events, finished_for(next)).await;
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].contains("5. Next step: folded"));
    assert!(!texts[0].contains("Files touched: old"));
    // The message the old summary covered is recovered from rows, and the
    // two after it are verbatim too.
    for (ordinal, text) in [(1, &hidden), (3, &span[0]), (5, &span[1])] {
        assert!(
            texts[0].contains(&format!("--- user message #{ordinal} ---\n{text}\n")),
            "user message {ordinal} must be exact after the fold"
        );
    }
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
        // The compacted assembly keeps the user's message and the last reply
        // verbatim in the record, and lists the file read, but not the tool
        // result; the tool is the only route back to the note's text.
        let texts = request_texts(request);
        assert!(texts[0].contains("folded"), "{texts:?}");
        assert!(
            texts[0].contains("--- user message #1 ---\nnever touch src/legacy/parser.rs\n"),
            "{texts:?}"
        );
        assert!(
            texts[0].contains("Files read, not modified:\n- note.txt\n"),
            "{texts:?}"
        );
        assert!(
            !texts[0].contains("keep src/legacy/parser.rs untouched"),
            "{texts:?}"
        );
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
    // Live pruning produces the stub assembly would: the read header without
    // its hash, and the re-read hint (AP2).
    for stub in results.iter().filter(|r| r.contains("[pruned")) {
        let (header, tail) = stub.split_once('\n').expect("a read stub keeps its header");
        assert!(header.starts_with("read note.txt L"), "{header}");
        assert!(!header.contains(" h:"), "{header}");
        assert!(tail.ends_with("without if_changed_since]"), "{tail}");
    }
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

/// The previous request's messages are a prefix of the next run's first
/// request: what the provider cache needs (ADR-0056 § 6).
fn assert_extends(previous: &ModelRequest, next: &ModelRequest) {
    assert_eq!(previous.system(), next.system());
    assert_eq!(previous.tools(), next.tools());
    let previous = previous.messages();
    let next = next.messages();
    assert!(next.len() > previous.len());
    for (index, (previous, next)) in previous.iter().zip(next).enumerate() {
        assert_eq!(
            previous.content(),
            next.content(),
            "message {index} was rewritten"
        );
    }
}

#[tokio::test]
async fn each_run_extends_the_previous_runs_last_request_until_a_seam() {
    // CX3: between seams assembly stubs nothing new, so every run's first
    // request is the previous run's last request plus the new turn, even
    // once old reads fall outside the recency window. Before CX3 the sixth
    // prompt's request rewrote the first read to a stub.
    let mut harness = auto_compact_harness(
        std::iter::repeat_with(|| AutoCompactScript::ReadNoteThenText("ok".to_owned()))
            .take(7)
            .collect(),
    )
    .await;
    std::fs::write(harness.workspace_path.join("note.txt"), "n".repeat(600)).unwrap();
    for index in 0..7 {
        let run = queue_prompt(
            &harness.runtime,
            harness.session_id,
            format!("read {index}"),
        )
        .await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let requests = harness.requests.lock().unwrap();
    // Two requests per run: the read, then the answer after its result.
    assert_eq!(requests.len(), 14);
    for run in 1..7 {
        assert_extends(&requests[2 * run - 1], &requests[2 * run]);
    }
    assert!(
        requests.iter().flat_map(|request| request.messages()).flat_map(Message::content).all(
            |block| !matches!(block, ContentBlock::ToolResult { content, .. } if content.contains("[pruned"))
        ),
        "nothing is stubbed without a seam"
    );
}

#[tokio::test]
async fn a_live_prune_moves_the_watermark_and_the_next_run_extends_it() {
    // CX3: the live overflow prune is a seam. The run that stubbed its old
    // reads records the watermark before it sends the stubbed request, so
    // the next run assembles the same stubs and extends that request.
    let turns = 8;
    let mut harness = auto_compact_harness_with_limits(
        vec![
            AutoCompactScript::ReadNoteRepeatedly {
                turns,
                text: "done".to_owned(),
            },
            AutoCompactScript::Text("again".to_owned()),
        ],
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
    assert_eq!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    );
    let watermark: (Option<u64>, Option<u32>) =
        Connection::open(harness.workspace_path.join("sessions.sqlite3"))
            .unwrap()
            .query_row(
                "SELECT prune_through_ordinal, prune_through_turn FROM sessions WHERE id = ?1",
                [harness.session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert!(
        watermark.0.is_some() && watermark.1.is_some_and(|turn| turn >= 1),
        "{watermark:?}"
    );

    let next = queue_prompt(&harness.runtime, harness.session_id, "and now".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(next)).await;
    assert_eq!(
        finished_outcome(&observed, next),
        Some(RunOutcome::Completed)
    );
    let requests = harness.requests.lock().unwrap();
    let (previous, first) = (&requests[requests.len() - 2], &requests[requests.len() - 1]);
    assert!(
        previous
            .messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult { content, .. } if content.contains("[pruned")
            ))
    );
    assert_extends(previous, first);
}

#[tokio::test]
async fn the_proactive_threshold_stubs_stale_reads_before_it_compacts() {
    // CX3: inside the last tenth of the window the first seam is stubbing,
    // not a summarizer. Old 12 KiB reads are re-derivable; once they are
    // stubs the prompt fits and sends without compacting.
    let turns = CONTEXT_PRUNE_KEEP_TURNS + 3;
    let mut scripts: Vec<AutoCompactScript> = (0..turns)
        .map(|_| AutoCompactScript::ReadNoteThenText("ok".to_owned()))
        .collect();
    scripts.push(AutoCompactScript::Text("fits now".to_owned()));
    // Seven ~12.6 KiB reads are ~22k estimated tokens: inside the last
    // tenth of a 24k window only for the final prompt.
    let mut harness = auto_compact_harness_with_window(scripts, Some(24 * 1024)).await;
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
        assert_eq!(
            finished_outcome(&observed, run),
            Some(RunOutcome::Completed),
            "run {index}"
        );
    }
    let last = queue_prompt(&harness.runtime, harness.session_id, "last".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(last)).await;
    assert_eq!(
        finished_outcome(&observed, last),
        Some(RunOutcome::Completed)
    );
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. })),
        "stubbing alone brought the prompt under the threshold"
    );
    let requests = harness.requests.lock().unwrap();
    let sent = requests.last().unwrap();
    let stubbed = sent
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter(|block| matches!(block, ContentBlock::ToolResult { content, .. } if content.contains("[pruned")))
        .count();
    // Each run is two assistant turns (the read, then the answer); the
    // last four turns are kept, and the oldest of them is an answer, so the
    // newest three reads stay verbatim and every older read is a stub.
    assert_eq!(stubbed, turns - 3);
    // The stubs are durable: the run after it assembles the same request.
    let watermark: Option<u64> = Connection::open(harness.workspace_path.join("sessions.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT prune_through_ordinal FROM sessions WHERE id = ?1",
            [harness.session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(watermark.is_some());
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
        if index + 1 == turns {
            // A seam before the last prompt rewrites its history to stubs.
            mark_prune_seam(
                &harness.workspace_path.join("sessions.sqlite3"),
                harness.session_id,
            );
        }
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
                ContentBlock::ToolResult { content, .. } if content.contains("\n[pruned: ")
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
    // exactly the input the failed one did. The prompt then runs from the
    // summary alone (RR6) and, when the provider rejects even that, fails
    // with the provider's own reason; the fold's cutoff never moved backwards.
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
            // summary) is rejected; the retry then runs from the summary
            // alone and the provider rejects that too.
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
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderContextExceeded,
                    ..
                }
            }
        ),
        "{outcome:?}"
    );
    // Two seeds; step one; prompt three's rejected send; step two; the
    // summary-only retry. The known-overflowing shape was never resent, and
    // the downgraded request carried the summary in place of "b".
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    let steps = summarizer_requests(&requests);
    assert_eq!(steps.len(), 2, "{steps:?}");
    let downgraded = request_texts(requests.last().unwrap());
    assert!(downgraded[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE));
    assert!(downgraded[0].contains("step one"));
    assert!(
        !downgraded
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
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

/// A harness whose one loaded provider answers the run's turns with shell
/// calls (mutating, so never stubbed) and every summarizer request with a
/// valid summary, under `full` approval so no gate waits.
async fn in_run_compaction_harness(turns: usize, window: u32, summary: &str) -> AutoCompactHarness {
    auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellRepeatedlyWithSummaries {
                turns,
                text: "task complete".to_owned(),
                summary: valid_summary(summary),
            }],
            loads: StdMutex::new(0),
            context_window: Some(window),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await
}

#[tokio::test]
async fn one_run_spanning_several_windows_compacts_its_own_turns_and_completes() {
    // F03: a run whose transcript outgrows the window at a later turn used to
    // fail with "compaction runs only between prompts". Its mutating results
    // cannot be stubbed, so stubbing does not save it. Now the loop
    // summarizes its own earlier turns at the boundary and continues in the
    // same run; each in-run compaction is a durable internal run with a
    // marker scoped to the prompt run.
    let turns = 48;
    // Each shell turn adds ~2.2 KiB of bounded result (~550 tokens); 48 of
    // them are ~26k tokens of transcript, past a 16k window twice over once
    // the system prompt and tool schemas are counted.
    let mut harness = in_run_compaction_harness(turns, 16 * 1024, "work so far").await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
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
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    // Exactly one user-visible run: every other RunStarted is an internal
    // compaction, and each of those settled Completed with a SessionCompacted.
    let started: Vec<RunId> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } => Some(*run_id),
            _ => None,
        })
        .collect();
    let compactions: Vec<RunId> = started.iter().copied().filter(|id| *id != run).collect();
    assert!(
        compactions.len() >= 2,
        "a 36k-token run in a 16k window needs more than one in-run compaction: {}",
        compactions.len()
    );
    let compacted_events = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        .count();
    assert_eq!(compacted_events, compactions.len());
    for compaction in &compactions {
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
                if run_id == compaction
        )));
    }
    // CX4: the prompt run reports `Compacting` before each of its
    // compaction runs starts and `WaitingForProvider` after it finishes; the
    // compaction run itself reports only `Compacting`.
    let activities: Vec<(RunId, RunActivity)> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunActivityChanged { run_id, activity } => Some((*run_id, *activity)),
            _ => None,
        })
        .collect();
    for compaction in &compactions {
        let position = |wanted: &dyn Fn(&SessionEvent) -> bool| {
            observed
                .iter()
                .position(|event| wanted(&event.event))
                .unwrap()
        };
        let started = position(
            &|event| matches!(event, SessionEvent::RunStarted { run_id, .. } if run_id == compaction),
        );
        let finished = position(
            &|event| matches!(event, SessionEvent::RunFinished { run_id, .. } if run_id == compaction),
        );
        let prompt_activity_before =
            observed[..started]
                .iter()
                .rev()
                .find_map(|event| match &event.event {
                    SessionEvent::RunActivityChanged { run_id, activity } if *run_id == run => {
                        Some(*activity)
                    }
                    _ => None,
                });
        assert_eq!(prompt_activity_before, Some(RunActivity::Compacting));
        // The session's summary names the prompt run's activity, so the
        // compaction run's start and finish both say `compacting` (the
        // headless `completed_after_in_run_compaction` golden pins this).
        for at in [started, finished] {
            let (SessionEvent::RunStarted { session, .. }
            | SessionEvent::RunFinished { session, .. }) = &observed[at].event
            else {
                unreachable!()
            };
            assert_eq!(session.activity, Some(RunActivity::Compacting));
        }
        // Its own `compacting` commits with its start, so no cancel between
        // the two can leave a started compaction run unreported.
        assert!(
            matches!(
                &observed[started + 1],
                SessionEventEnvelope { run_id: Some(id), event: SessionEvent::RunActivityChanged { activity: RunActivity::Compacting, .. }, cursor, .. }
                    if id == compaction && cursor.sequence == observed[started].cursor.sequence + 1
            ),
            "{:?}",
            observed[started + 1].event
        );
        // The compaction is recorded right after its run finishes, under
        // that run's id, before the prompt run moves on (the golden's order).
        assert!(
            matches!(
                &observed[finished + 1],
                SessionEventEnvelope { run_id: Some(id), event: SessionEvent::SessionCompacted { .. }, .. }
                    if id == compaction
            ),
            "{:?}",
            observed[finished + 1].event
        );
        let prompt_activity_after =
            observed[finished..]
                .iter()
                .find_map(|event| match &event.event {
                    SessionEvent::RunActivityChanged { run_id, activity } if *run_id == run => {
                        Some(*activity)
                    }
                    _ => None,
                });
        assert_eq!(prompt_activity_after, Some(RunActivity::WaitingForProvider));
        let own: Vec<RunActivity> = activities
            .iter()
            .filter(|(run_id, _)| run_id == compaction)
            .map(|(_, activity)| *activity)
            .collect();
        assert_eq!(own, [RunActivity::Compacting]);
    }
    // An in-run summary sends the run's own system prompt and tools, and
    // its messages are a prefix of the request the run sent just before it
    // (the turn that overflowed is cut at the boundary), so a provider
    // prefix cache covers all but the instruction (ADR-0056 § 5).
    {
        let requests = harness.requests.lock().unwrap();
        let index = requests
            .iter()
            .position(|request| {
                request_texts(request)
                    .last()
                    .is_some_and(|text| text.starts_with("The task above is still in progress"))
            })
            .expect("an in-run summary request");
        let (summary, before) = (&requests[index], &requests[index - 1]);
        assert_eq!(summary.system(), before.system());
        assert_eq!(summary.tools(), before.tools());
        assert_eq!(summary.reasoning_effort(), before.reasoning_effort());
        let (_, prefix) = summary.messages().split_last().unwrap();
        assert!(
            before.messages().starts_with(prefix),
            "the in-run summarizer must send a prefix of the run's last request"
        );
        assert_eq!(
            prefix.first(),
            before.messages().first(),
            "it keeps the prompt"
        );
    }
    // An in-run summary declares the run's tools and carries its tool
    // calls and results.
    {
        let requests = harness.requests.lock().unwrap();
        let summary = requests
            .iter()
            .find(|request| {
                request_texts(request)
                    .last()
                    .is_some_and(|text| text.starts_with("The task above is still in progress"))
            })
            .expect("an in-run summary request");
        assert!(!summary.tools().is_empty());
        assert!(
            summary
                .messages()
                .iter()
                .flat_map(Message::content)
                .any(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. }
                    )
                })
        );
    }
    // Every request the model saw fit the window by the estimate (system
    // prompt and tool schemas included, as the loop counts them).
    let results = {
        let requests = harness.requests.lock().unwrap();
        for (index, request) in requests.iter().enumerate() {
            let bytes = crate::measure_messages(request.messages())
                + request.system().map_or(0, |system| system.len() as u64)
                + request
                    .tools()
                    .iter()
                    .map(|tool| {
                        (tool.name().len()
                            + tool.description().len()
                            + tool.input_schema().get().len()) as u64
                    })
                    .sum::<u64>();
            // Each request is judged with the output reserve it actually
            // carried: the run's own, or the summarizer's larger one.
            let reserve = u64::from(request.max_output_tokens());
            assert!(
                crate::sessions::context::estimate_tokens(bytes) + reserve <= 16 * 1024,
                "request {index} ({bytes} bytes + {reserve} reserve) was sent over the window"
            );
        }
        // The final request carries the prompt, a summary, and only the turns
        // since the last compaction verbatim — not all 48 shell results.
        let last = requests.last().unwrap();
        let texts = request_texts(last);
        assert_eq!(texts.first().map(String::as_str), Some("do the task"));
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE)),
            "{texts:?}"
        );
        let results = last
            .messages()
            .iter()
            .flat_map(Message::content)
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .count();
        assert!(
            results >= CONTEXT_PRUNE_KEEP_TURNS && results < turns / 2,
            "{results} results in the last request"
        );
        results
    };

    // All 24 shell calls ran exactly once and are durable.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let calls: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1 AND state = 'completed'",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(calls, turns as u64);
    // The markers are scoped to the prompt run; no between-run marker exists.
    let (scoped, unscoped): (u64, u64) = connection
        .query_row(
            "SELECT SUM(scope_run_id = ?1), SUM(scope_run_id IS NULL) FROM session_compactions",
            [run.to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<u64>>(0)?.unwrap_or(0),
                    row.get::<_, Option<u64>>(1)?.unwrap_or(0),
                ))
            },
        )
        .unwrap();
    assert!((1..=3).contains(&scoped), "{scoped}");
    assert_eq!(unscoped, 0);
    drop(connection);

    // Replay renders the same shape the live run saw: prompt, summary, then
    // the retained turns; the reference oracle agrees.
    let store = harness.runtime.inner.store.clone();
    let session_id = harness.session_id;
    let context = store
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction().unwrap();
            load_model_context(&transaction, session_id, u64::MAX)
        })
        .await
        .unwrap();
    let replayed_results = context
        .iter()
        .flat_map(Message::content)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .count();
    assert_eq!(
        replayed_results, results,
        "replay renders what the live run last saw"
    );
    let summary_text = |messages: &[Message]| {
        messages
            .iter()
            .flat_map(Message::content)
            .find_map(|block| match block {
                ContentBlock::Text { text }
                    if text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE) =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
    };
    // The live splice used the stored summary, so it is byte-identical to
    // replay.
    let replayed = summary_text(&context).expect("replay renders the in-run summary");
    let live = {
        let requests = harness.requests.lock().unwrap();
        summary_text(requests.last().unwrap().messages()).unwrap()
    };
    assert_eq!(live, replayed);
    harness.runtime.shutdown().await.unwrap();
    drop(store);
    assert_assembly_matches_reference(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );
}

#[tokio::test]
async fn an_in_run_summarizer_keeps_the_session_context_before_its_prompt() {
    // The second prompt of a session compacts itself mid-run. Its
    // summarizer request starts with the first prompt and its reply, as
    // every turn of the run did, so the cached prefix is shared; the earlier
    // context is not summarized away.
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![
                AutoCompactScript::Text("first answer".to_owned()),
                AutoCompactScript::ShellRepeatedlyWithSummaries {
                    turns: 48,
                    text: "task complete".to_owned(),
                    summary: valid_summary("work so far"),
                },
            ],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let first = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "earlier work".to_owned(),
    )
    .await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    assert_eq!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    );
    let requests = harness.requests.lock().unwrap();
    let summary = requests
        .iter()
        .find(|request| {
            request_texts(request)
                .last()
                .is_some_and(|text| text.starts_with("The task above is still in progress"))
        })
        .expect("an in-run summary request");
    let texts = request_texts(summary);
    assert_eq!(texts[0], "earlier work");
    assert_eq!(texts[1], "first answer");
    assert_eq!(texts[2], "do the task");
}

#[tokio::test]
async fn a_dense_tokenizer_compacts_in_run_instead_of_failing_at_the_guard() {
    // ENG-940: eight production runs failed with "context is estimated at N
    // input tokens ... over the window" and `context_compaction_attempted =
    // 0`. The loop decided whether to recover from the raw bytes/4 estimate
    // (fits) while the admission guard judged the measured chain (over), so
    // recovery never ran and the guard failed the run closed. Here the
    // provider reports two bytes per token: the raw estimate of a 16k window
    // fits until ~64 KiB of transcript, the measured chain overflows at half
    // that. The run must compact its own turns on the measured figure and
    // complete; no request may reach the provider over the window by the
    // measured chain.
    let turns = 24;
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::Measured {
                script: Box::new(AutoCompactScript::ShellRepeatedlyWithSummaries {
                    turns,
                    text: "task complete".to_owned(),
                    summary: valid_summary("work so far"),
                }),
                bytes_per_token: 2,
            }],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
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
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    let compactions = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        .count();
    assert!(
        compactions >= 1,
        "the measured overflow must trigger in-run compaction"
    );
    // Every prompt-run request fit the window by the provider's own ratio.
    // The summarizer's requests are exempt: they read the overflowing
    // transcript by design and the provider adjudicates them.
    let oversized: Vec<(usize, u64)> = {
        let requests = harness.requests.lock().unwrap();
        requests
            .iter()
            .enumerate()
            .filter(|(_, request)| {
                !request_texts(request)
                    .last()
                    .is_some_and(|text| text.contains("Summarize this conversation"))
            })
            .map(|(index, request)| {
                let bytes = crate::measure_messages(request.messages())
                    + request.system().map_or(0, |system| system.len() as u64)
                    + request
                        .tools()
                        .iter()
                        .map(|tool| {
                            (tool.name().len()
                                + tool.description().len()
                                + tool.input_schema().get().len())
                                as u64
                        })
                        .sum::<u64>();
                (
                    index,
                    bytes.div_ceil(2) + u64::from(request.max_output_tokens()),
                )
            })
            .filter(|(_, measured_tokens)| *measured_tokens > 16 * 1024)
            .collect()
    };
    assert!(
        oversized.is_empty(),
        "requests over the 16384 window at two bytes per token (index, tokens): {oversized:?}"
    );
    harness.runtime.close().await.unwrap();
}

#[tokio::test]
async fn a_rejected_in_run_summary_fails_the_run_closed_without_resending_the_overflow() {
    // The summarizer returns garbage (no required sections). The compaction
    // run settles failed, no marker is written, and the prompt run fails with
    // the context diagnosis naming the summarizer — it does not poll the
    // provider with the overflowing request.
    let turns = 48;
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellRepeatedlyWithSummaries {
                turns,
                text: "task complete".to_owned(),
                summary: "not a summary".to_owned(),
            }],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
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
        matches!(
            &outcome,
            RunOutcome::Failed { failure: RunFailure { kind: RunFailureKind::Policy, message } }
                if message.contains("in-run compaction did not produce") && message.contains("summarizer failed")
        ),
        "{outcome:?}"
    );
    // One compaction run started and settled failed; nothing compacted.
    let compaction = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { run_id, .. } if *run_id != run => Some(*run_id),
            _ => None,
        })
        .expect("the in-run compaction must start");
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
    // The last provider request was the summarizer's, not a retry of the
    // overflowing turn.
    let requests = harness.requests.lock().unwrap();
    let last = request_texts(requests.last().unwrap());
    assert!(
        last.last().unwrap().contains("Summarize this conversation"),
        "{:?}",
        last.last()
    );
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let markers: u64 = connection
        .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(markers, 0);
    // The session is idle and usable afterwards.
    let session = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, session, ..
            } if *run_id == run => Some(session.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(session.active_run_id, None);
}

#[tokio::test]
async fn an_in_run_compaction_that_cannot_start_fails_the_prompt_run_as_a_server_failure() {
    // CX4 review: the compaction run's start (its row, `RunStarted`, and its
    // `compacting` activity, one transaction) can fail in the store. The
    // provider was never asked, so the prompt run must not blame the context
    // or advise `/compact`.
    let mut harness = in_run_compaction_harness(48, 16 * 1024, "work so far").await;
    Connection::open(harness.workspace_path.join("sessions.sqlite3"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_compaction_start BEFORE INSERT ON runs
             WHEN NEW.kind = 'compaction'
             BEGIN SELECT RAISE(ABORT, 'injected start failure'); END;",
        )
        .unwrap();
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    let prompt = finished_outcome(&observed, run);
    assert!(
        matches!(
            &prompt,
            Some(RunOutcome::Failed { failure: RunFailure { kind: RunFailureKind::Server, message } })
                if message.contains("could not be persisted") && !message.contains("/compact")
        ),
        "{prompt:?}"
    );
    // No compaction run exists, and the summarizer was never asked.
    assert!(!observed.iter().any(
        |event| matches!(&event.event, SessionEvent::RunStarted { run_id, .. } if *run_id != run)
    ));
    let requests = harness.requests.lock().unwrap();
    assert!(
        !requests.iter().any(|request| request_texts(request)
            .iter()
            .any(|text| text.contains("Summarize this conversation"))),
        "the summarizer must not be polled"
    );
}

#[tokio::test]
async fn cancelling_during_in_run_compaction_settles_both_runs_once() {
    // The summarizer stalls; the user cancels the prompt run. The cascade
    // cancels the compaction (it is owned by the prompt run), both settle
    // Cancelled exactly once, no marker is written, and the session is idle.
    let turns = 48;
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellRepeatedlyWithSummaries {
                turns,
                text: "task complete".to_owned(),
                summary: STALL_SUMMARY.to_owned(),
            }],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
    )
    .await;
    let observed = collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id != run),
    )
    .await;
    let SessionEvent::RunStarted {
        run_id: compaction, ..
    } = observed.last().unwrap().event
    else {
        panic!("expected the in-run compaction to start")
    };
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: run },
        )
        .await
        .unwrap();
    // Both terminals arrive; their order depends on which task settles first.
    let mut observed = collect_until(&mut harness.events, finished_for(run)).await;
    if !observed.iter().any(|event| {
        matches!(
            &event.event,
            SessionEvent::RunFinished { run_id, .. } if *run_id == compaction
        )
    }) {
        observed.extend(collect_until(&mut harness.events, finished_for(compaction)).await);
    }
    let finished: Vec<(RunId, RunOutcome)> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } => Some((*run_id, outcome.clone())),
            _ => None,
        })
        .collect();
    assert!(
        finished
            .iter()
            .any(|(id, outcome)| *id == run && matches!(outcome, RunOutcome::Cancelled)),
        "{finished:?}"
    );
    assert!(
        finished
            .iter()
            .any(|(id, outcome)| *id == compaction && matches!(outcome, RunOutcome::Cancelled)),
        "{finished:?}"
    );
    assert_eq!(finished.iter().filter(|(id, _)| *id == run).count(), 1);
    assert_eq!(
        finished.iter().filter(|(id, _)| *id == compaction).count(),
        1
    );
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
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
    harness.runtime.shutdown().await.unwrap();
}

/// Marker text the scripted summarizer treats as "never reply".
const STALL_SUMMARY: &str = "__STALL__";

#[tokio::test]
async fn an_in_run_marker_survives_restart_and_folds_into_a_later_between_run_compaction() {
    // Run one compacts itself mid-run and completes. After a restart, a new
    // prompt's context renders run one as prompt + in-run summary + retained
    // turns (not all 48 results). When that session later compacts between
    // runs, the between-run summarizer reads the already-summarized shape,
    // and its marker supersedes the in-run one in assembly.
    let turns = 48;
    let mut harness = in_run_compaction_harness(turns, 16 * 1024, "work so far").await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == run
    )));
    let after = observed.last().unwrap().cursor;
    let workspace_id = harness.workspace_id;
    let session_id = harness.session_id;
    let database_path = harness.workspace_path.join("sessions.sqlite3");
    harness.runtime.close().await.unwrap();
    drop(harness.runtime);

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        Arc::new(AutoCompactLoader {
            requests: Arc::clone(&requests),
            scripts: vec![
                AutoCompactScript::Text("follow-up answer".to_owned()),
                AutoCompactScript::Text(valid_summary("between-run summary")),
                AutoCompactScript::Text("after compaction".to_owned()),
            ],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
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
    let follow_up = queue_prompt(&runtime, session_id, "and then?".to_owned()).await;
    collect_until(&mut events, finished_for(follow_up)).await;
    {
        let requests = requests.lock().unwrap();
        let first = &requests[0];
        let texts = request_texts(first);
        assert_eq!(texts.first().map(String::as_str), Some("do the task"));
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE)),
            "the reopened session renders run one's in-run summary: {texts:?}"
        );
        let results = first
            .messages()
            .iter()
            .flat_map(Message::content)
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .count();
        assert!(results < turns / 2, "{results}");
        assert_eq!(texts.last().map(String::as_str), Some("and then?"));
    }

    // Manual between-run compaction folds everything, in-run summary included.
    let compaction = compact_session(&runtime, session_id).await;
    let observed = collect_until(&mut events, finished_for(compaction)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == compaction
    )));
    {
        let requests = requests.lock().unwrap();
        let summarizer = request_texts(&requests[1]);
        assert!(
            summarizer
                .iter()
                .any(|text| text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE)),
            "the between-run summarizer reads the in-run summary, not 48 raw turns"
        );
    }
    let next = queue_prompt(&runtime, session_id, "continue".to_owned()).await;
    collect_until(&mut events, finished_for(next)).await;
    {
        let requests = requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        assert!(
            texts
                .first()
                .is_some_and(|text| text.starts_with(COMPACTION_SUMMARY_PREAMBLE)),
            "{texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|text| text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE)),
            "the between-run marker supersedes run one entirely: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("between-run summary"))
        );
    }
    // Rollback pops the between-run marker; the in-run marker applies again.
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::RollbackCompaction { session_id },
        )
        .await
        .unwrap();
    let store = runtime.inner.store.clone();
    let context = store
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction().unwrap();
            load_model_context(&transaction, session_id, u64::MAX)
        })
        .await
        .unwrap();
    assert!(context.iter().any(|message| {
        message.content().iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE)
        ))
    }));
    runtime.close().await.unwrap();
    drop(store);
    assert_assembly_matches_reference(&database_path, session_id);
}

#[tokio::test]
async fn a_provider_window_rejection_compacts_the_run_and_continues() {
    // RR6 (b): the estimate said the request fit, the provider said it did
    // not. The first such rejection used to fail the run with
    // `provider_context_exceeded` (and wedge the next prompt on a known
    // overflow). Now the loop treats the provider's verdict as authoritative,
    // compacts its own turns at the boundary, and re-issues the turn.
    let overflows = Arc::new(AtomicUsize::new(0));
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellRepeatedlyWithProviderOverflow {
                turns: 12,
                overflow_at: 7,
                text: "task complete".to_owned(),
                summary: valid_summary("work so far"),
                overflows: Arc::clone(&overflows),
            }],
            loads: StdMutex::new(0),
            // Wide enough that the estimate never trips on 12 tiny results;
            // only the provider's scripted verdict can trigger compaction.
            context_window: Some(200_000),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
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
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    // Exactly one provider rejection, answered by exactly one in-run
    // compaction, and every one of the 12 shell calls ran once.
    assert_eq!(overflows.load(Ordering::SeqCst), 1);
    let compactions = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        .count();
    assert_eq!(compactions, 1);
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let calls: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1 AND state = 'completed'",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(calls, 12);
    // The next prompt is admitted normally: the overflow was recovered, not
    // recorded as a failed run whose basis the next prompt must avoid.
    let next = queue_prompt(&harness.runtime, harness.session_id, "more".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(next)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == next
    )));
}

#[tokio::test]
async fn an_exhausted_fold_admits_the_prompt_with_summary_only_history() {
    // RR6 (c): after the fold ran and the retained transcript still does not
    // fit, the prompt used to be refused with "automatic compaction ran N
    // summarizer steps ... start a new session" — and every later prompt hit
    // the same wall (3 sessions wedged in the audit). Now the run starts
    // from the latest summary alone with a notice; nothing is deleted.
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            // Prompt three: step one summarizes "a" and commits; the
            // prompt's own send is rejected by the provider.
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::Text(valid_summary("step one")),
                AutoCompactScript::ContextOverflow,
            ]),
            // The retry's step two is rejected too: the fold is exhausted.
            // The retry then runs from the summary alone and completes.
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::ContextOverflow,
                AutoCompactScript::Text("recovered".to_owned()),
            ]),
        ],
        Some(window),
    )
    .await;
    for prompt in ["one", "two"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        collect_until(&mut harness.events, finished_for(run)).await;
    }
    let third = queue_prompt(&harness.runtime, harness.session_id, "three".to_owned()).await;
    collect_until(&mut harness.events, finished_for(third)).await;
    let retry = queue_prompt(&harness.runtime, harness.session_id, "retry".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(retry)).await;
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id, outcome, ..
            } if *run_id == retry => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    // Notice, then the summary, then the prompt; none of the verbatim
    // 80 KiB runs.
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE),
        "{}",
        texts[0]
    );
    assert!(texts[0].contains(COMPACTION_SUMMARY_PREAMBLE));
    assert!(texts[0].contains("step one"));
    assert_eq!(texts[1], "retry");
    assert!(
        !texts
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
    drop(requests);
    // Nothing was deleted: the session still holds every prompt.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let prompts: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND role = 'user' AND steering = 0",
            [harness.session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prompts, 4);
}

/// The live splice removes every message before the first kept assistant
/// turn, including the steering and the runtime notice that preceded that
/// turn's request. Replay must drop them too, or a run that compacted right
/// after a slice checkpoint (or a steer) assembles a message the live request
/// never carried. Live context is built and spliced with the real boundary;
/// replay goes through `append_run_turns` with the matching cutoff.
#[test]
fn replay_drops_the_notice_and_steering_the_in_run_splice_removed() {
    use crate::runtime::TurnNotice;
    let assistant = |n: u32| Message::assistant(format!("turn {n}"));
    // Seven turns, four kept: turn 4 is the first kept turn. It was the
    // first turn of a new slice, and a steer joined before it.
    let mut live = vec![Message::user("prompt")];
    for n in 1..=7 {
        if n == 4 {
            live.push(Message::user("steer before four"));
            live.push(Message::user(TurnNotice::Continuation.text()));
        }
        live.push(assistant(n));
    }
    let (replace_through, replaced_turns) =
        crate::sessions::in_run_compaction_boundary(&live[1..], 4).unwrap();
    assert_eq!(replaced_turns, 3);
    let summary = Message::user(format!("{IN_RUN_COMPACTION_PREAMBLE}\n\nsummary"));
    live.splice(1..1 + replace_through, [summary]);
    // The live splice took the steer and the notice with the summarized span.
    assert!(!live.contains(&Message::user(TurnNotice::Continuation.text())));
    assert!(!live.contains(&Message::user("steer before four")));

    let turns = (1..=7)
        .map(|n| StoredTurn {
            ordinal: n,
            content_json: format!("[{{\"type\":\"text\",\"text\":\"turn {n}\"}}]"),
            truncated: false,
            notice: (n == 4).then_some(TurnNotice::Continuation),
        })
        .collect();
    let mut replayed = vec![Message::user("prompt")];
    append_run_turns(
        turns,
        HashMap::new(),
        std::collections::VecDeque::from([(4, "steer before four".to_owned())]),
        std::collections::VecDeque::new(),
        Some(InRunCompaction {
            summary: "summary".to_owned(),
            turn_cutoff: replaced_turns,
        }),
        &mut replayed,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(replayed, live);

    // A notice on a kept turn after the first one survives on both sides.
    let mut live = vec![Message::user("prompt")];
    for n in 1..=7 {
        if n == 5 {
            live.push(Message::user(TurnNotice::Report.text()));
        }
        live.push(assistant(n));
    }
    let (replace_through, replaced_turns) =
        crate::sessions::in_run_compaction_boundary(&live[1..], 4).unwrap();
    let summary = Message::user(format!("{IN_RUN_COMPACTION_PREAMBLE}\n\nsummary"));
    live.splice(1..1 + replace_through, [summary]);
    let turns = (1..=7)
        .map(|n| StoredTurn {
            ordinal: n,
            content_json: format!("[{{\"type\":\"text\",\"text\":\"turn {n}\"}}]"),
            truncated: false,
            notice: (n == 5).then_some(TurnNotice::Report),
        })
        .collect();
    let mut replayed = vec![Message::user("prompt")];
    append_run_turns(
        turns,
        HashMap::new(),
        std::collections::VecDeque::new(),
        std::collections::VecDeque::new(),
        Some(InRunCompaction {
            summary: "summary".to_owned(),
            turn_cutoff: replaced_turns,
        }),
        &mut replayed,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(replayed, live);
    assert!(replayed.contains(&Message::user(TurnNotice::Report.text())));
}

/// Delivered sub-agent answers (ADR-0054 § 4) splice like steering: the one
/// before the first kept turn went with the summarized span, after that
/// boundary's steering; one before a later kept turn survives. Replay places
/// both exactly where the live run did, and an answer delivered after the
/// run settled follows the run.
#[test]
fn replay_drops_and_keeps_delivered_answers_as_the_in_run_splice_did() {
    let assistant = |n: u32| Message::assistant(format!("turn {n}"));
    let mut live = vec![Message::user("prompt")];
    for n in 1..=7 {
        if n == 4 {
            live.push(Message::user("steer before four"));
            live.push(Message::user("answer before four"));
        }
        if n == 6 {
            live.push(Message::user("answer before six"));
        }
        live.push(assistant(n));
    }
    let (replace_through, replaced_turns) =
        crate::sessions::in_run_compaction_boundary(&live[1..], 4).unwrap();
    assert_eq!(replaced_turns, 3);
    let summary = Message::user(format!("{IN_RUN_COMPACTION_PREAMBLE}\n\nsummary"));
    live.splice(1..1 + replace_through, [summary]);
    assert!(!live.contains(&Message::user("answer before four")));
    assert!(live.contains(&Message::user("answer before six")));
    live.push(Message::user("answer after the run"));

    let turns = (1..=7)
        .map(|n| StoredTurn {
            ordinal: n,
            content_json: format!("[{{\"type\":\"text\",\"text\":\"turn {n}\"}}]"),
            truncated: false,
            notice: None,
        })
        .collect();
    let mut replayed = vec![Message::user("prompt")];
    append_run_turns(
        turns,
        HashMap::new(),
        std::collections::VecDeque::from([(4, "steer before four".to_owned())]),
        std::collections::VecDeque::from([
            (Some(4), "answer before four".to_owned()),
            (Some(6), "answer before six".to_owned()),
            (None, "answer after the run".to_owned()),
        ]),
        Some(InRunCompaction {
            summary: "summary".to_owned(),
            turn_cutoff: replaced_turns,
        }),
        &mut replayed,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(replayed, live);
}

#[test]
fn in_run_compaction_boundary_keeps_the_recent_turns_with_their_results() {
    use crate::sessions::in_run_compaction_boundary as boundary;
    let assistant = || Message::assistant("t");
    let results = || Message::tool_results(vec![]);
    // Six turns, each assistant + results. Keeping four replaces the first
    // two turns (four messages) and reports cutoff 2.
    let run: Vec<Message> = (0..6).flat_map(|_| [assistant(), results()]).collect();
    assert_eq!(boundary(&run, 4), Some((4, 2)));
    // Steering applied before the first kept turn is replaced with the
    // summarized span — assembly drops steering with `applied_before` up to
    // and including the first kept turn.
    let mut with_steer = run.clone();
    with_steer.insert(4, Message::user("steer"));
    assert_eq!(boundary(&with_steer, 4), Some((5, 2)));
    // Exactly the keep count: nothing to replace.
    let short: Vec<Message> = (0..4).flat_map(|_| [assistant(), results()]).collect();
    assert_eq!(boundary(&short, 4), None);
    assert_eq!(boundary(&[], 4), None);
    // A prior in-run summary at the head is replaced along with the turns.
    let mut folded = vec![Message::user("summary so far")];
    folded.extend((0..5).flat_map(|_| [assistant(), results()]));
    assert_eq!(boundary(&folded, 4), Some((3, 1)));
}

#[tokio::test]
async fn an_in_run_summary_cut_at_the_output_limit_is_continued_and_joined_verbatim() {
    // #97 fixed the between-run summarizer joining truncated turns with a
    // newline through a mid-token cut. The in-run summarizer takes its own
    // path (`Runtime::summarize`), so it must continue a cut reply the same
    // way and concatenate the pieces byte-for-byte; a heading split at the
    // cut still validates and the committed summary is the whole text.
    let turns = 48;
    let summary = valid_summary("continued");
    // Cut inside the word "Decisions" so the seam splits a required heading.
    let cut = summary.find("Decis").unwrap() + "Decis".len();
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellRepeatedlyWithTruncatedSummaries {
                turns,
                text: "task complete".to_owned(),
                summary: summary.clone(),
                cut,
            }],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "do the task".to_owned(),
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
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
    let compactions = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        .count();
    assert!(compactions >= 1);
    // Every stored in-run summary is the full text: heading intact, no seam.
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let mut statement = connection
        .prepare("SELECT summary FROM session_compactions WHERE scope_run_id = ?1")
        .unwrap();
    let stored: Vec<String> = statement
        .query_map([run.to_string()], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!stored.is_empty());
    for text in &stored {
        assert!(text.starts_with(&summary), "{text}");
        assert!(!text.contains("Decis\n"), "{text}");
        assert!(
            text.contains("Decisions and constraints: continued"),
            "{text}"
        );
        assert!(
            !text.contains("Decis\n"),
            "a newline was inserted at the cut: {text}"
        );
    }
    // Each summarizer exchange was two provider requests: the cut reply and
    // its continuation carrying the truncation notice.
    let requests = harness.requests.lock().unwrap();
    let continuations = requests
        .iter()
        .filter(|request| {
            request_texts(request)
                .last()
                .is_some_and(|text| text.contains("cut off at the output token limit"))
        })
        .count();
    assert_eq!(continuations, compactions);
}

/// End to end: a run reaches its slice report, keeps working, and then
/// compacts in-run (more than once). Whatever the splices removed — including
/// the report and continuation notices when their turns are summarized or are
/// the first kept turn — a follow-up assembles exactly the live context, and
/// the reference oracle agrees. (AP3a review gap G2.)
#[tokio::test]
async fn compacting_after_a_slice_report_replays_the_live_context() {
    // Enough turns to compact twice after the report at turn 17.
    let calls = 22 * crate::MAX_TOOL_CALLS_PER_TURN;
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![
                AutoCompactScript::ShellBatchesAcrossACheckpoint {
                    calls,
                    text: "task complete".to_owned(),
                },
                AutoCompactScript::Text("follow-up done".to_owned()),
            ],
            loads: StdMutex::new(0),
            // Sixteen 400-byte shell results per turn fill a 24k window a
            // few turns after the report, and again later.
            context_window: Some(24 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "do it".to_owned()).await;
    // ~350 durable tool calls take a few seconds alone; under a loaded
    // parallel suite allow more than the shared helper's 30 s.
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            let done = finished_for(run)(&event.event);
            observed.push(event);
            if done {
                break;
            }
        }
    })
    .await
    .expect("the run finishes");
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
    let compactions = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        .count();
    let live = {
        let requests = harness.requests.lock().unwrap();
        let report_at = requests
            .iter()
            .position(|request| {
                request.messages().last() == Some(&Message::user(crate::SLICE_CHECKPOINT_NOTICE))
            })
            .expect("the run reached its slice report");
        let first_compaction_after_report = requests[report_at..]
            .iter()
            .position(|request| {
                request.messages().last().is_some_and(|message| {
                    message.content().iter().any(|block| {
                        matches!(block, ContentBlock::Text { text } if text.contains("Summarize this conversation"))
                    })
                })
            });
        assert!(compactions >= 2, "{compactions}");
        assert!(
            first_compaction_after_report.is_some(),
            "the run must compact after its report"
        );
        requests
            .iter()
            .rev()
            .find(|request| {
                !request.messages().last().is_some_and(|message| {
                    message.content().iter().any(|block| {
                        matches!(block, ContentBlock::Text { text } if text.contains("Summarize this conversation"))
                    })
                })
            })
            .unwrap()
            .messages()
            .to_vec()
    };
    let follow_up =
        queue_prompt(&harness.runtime, harness.session_id, "and then?".to_owned()).await;
    collect_until(&mut harness.events, finished_for(follow_up)).await;
    let replayed = harness
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages()
        .to_vec();
    assert_eq!(&replayed[..live.len()], live.as_slice());
    assert_assembly_matches_reference(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );
}
