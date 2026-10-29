use super::*;

#[tokio::test]
async fn a_compaction_run_reports_compacting_and_nothing_else() {
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
    assert_eq!(harness.models.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_between_run_summarizer_request_extends_the_prompt_request_it_follows() {
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
    let summary_excerpt = summary_excerpt.unwrap();
    assert!(
        summary_excerpt.starts_with(&format!(
            "{}\n\n{COMPACTION_RECORD_HEADER}\n",
            valid_summary("hello")
        )),
        "{summary_excerpt}"
    );
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let context = load_model_context(&connection, session_id, u64::MAX).unwrap();
    assert_eq!(after_bytes, context_bytes(&context) as u64);
    assert!(request_texts_of(&context[0])[0].contains("--- user message #1 ---"));
    assert_eq!(context_tokens, None);

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
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[1]);
        assert!(texts.iter().any(|text| text == "first prompt"));
        let instruction = texts.last().unwrap();
        assert_eq!(instruction, COMPACTION_INSTRUCTION);
    }

    submit_prompt(&harness, "second prompt").await;
    collect_through_finished(&mut harness.events).await;
    {
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(&requests[2]);
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert!(texts[0].contains("--- user message #1 ---\nfirst prompt\n"));
        assert_eq!(texts[1], "second prompt");
        assert!(!texts.iter().any(|text| text == "first prompt"));
    }

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
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        assert_eq!(texts.len(), 2);
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREAMBLE));
        assert_eq!(texts[1], "third prompt");
    }

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
fn metadata_is_bounded_while_exact_inputs_are_not_cut_by_the_record_budget() {
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
    assert!(!record.contains("USER "));
    let exact = with_exact_inputs(
        &connection,
        &session_id.to_string(),
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        record.clone(),
    )
    .unwrap();
    let mut previous = 0;
    for prompt in &prompts {
        let at = exact.find(prompt).unwrap();
        assert!(at > previous);
        previous = at;
    }
    assert!(!exact.contains("omitted for space"));
    assert!(record.contains("Last assistant reply, to user message #79:\nreply 39\n"));
    assert!(
        record.contains("Files modified:\n- src/edited.rs\n- src/read.rs\n"),
        "{record}"
    );
    assert!(!record.contains("Files read, not modified"));
    assert!(!record.contains("src/preview.rs"));
    assert!(record.contains(
        "- shell: exit status 101: error[E0433]: failed to resolve (user message #79 turn 1)"
    ));
    assert!(!record.contains("more detail"));
    assert!(!record.contains("slice checkpoint"));

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
    assert!(!small.contains("USER "));
    let exact_small = with_exact_inputs(
        &connection,
        &session_id.to_string(),
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        small,
    )
    .unwrap();
    for prompt in &prompts {
        assert!(exact_small.contains(prompt));
    }

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
        let scope = RecordScope::Run {
            run_id: run,
            turn_cutoff,
        };
        let metadata = render_compaction_record(
            &connection,
            session_id,
            scope,
            context::COMPACTION_RECORD_BYTES,
        )
        .unwrap();
        with_exact_inputs(&connection, &session_id.to_string(), scope, metadata).unwrap()
    };

    let record = render(2);
    assert!(record.starts_with(COMPACTION_RECORD_HEADER));
    for kept in ["steer one", "steer in the gap", "steer before kept"] {
        assert_eq!(record.matches(kept).count(), 1, "{kept}: {record}");
    }
    assert!(record.contains("--- steering during user message #1 turn 5 ---\nsteer before kept\n"));
    assert!(!record.contains("steer later"));
    assert!(!record.contains("do the task"));
    assert!(!record.contains("Last assistant reply"));
    assert!(record.contains("Files modified:\n- a.rs\n"));
    assert!(!record.contains("kept.rs"));
    assert!(
        record.contains("- shell: make: *** [all] Error 2 (turn 2)"),
        "{record}"
    );

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
        assert!(
            !record.contains("steer late"),
            "inputs are outside the metadata budget"
        );
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
    assert!(record.contains(" more\n"), "{record}");
    assert!(record.contains(" older\n"), "{record}");
    let full = with_exact_inputs(
        &connection,
        &session_id.to_string(),
        RecordScope::Session {
            cutoff_ordinal: cutoff,
        },
        record,
    )
    .unwrap();
    assert!(full.contains("--- steering during user message #23 turn 2 ---\nsteer late\n"));
}

#[test]
fn the_summarizer_output_cap_is_the_run_cap_bounded_by_the_window() {
    assert_eq!(context::summarizer_output_tokens(32_768, None), 32_768);
    assert_eq!(
        context::summarizer_output_tokens(16_384, Some(272_000)),
        16_384
    );
    assert_eq!(
        context::summarizer_output_tokens(128_000, Some(272_000)),
        34_000
    );
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
    assert_eq!(
        narrative_of(format!(
            "{narrative}\n\n{}\nstale instructions",
            EXACT_INPUTS_HEADER
        )),
        narrative
    );
    assert_eq!(narrative_of(narrative.clone()), narrative);
    let mentions = format!("{narrative}\nSee {COMPACTION_RECORD_HEADER} below.");
    assert_eq!(narrative_of(mentions.clone()), mentions);
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
    for (configured, expected) in [(1_024, 1_024), (16_384, 16_384)] {
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
async fn manual_compaction_transport_retry_preserves_the_visible_prefix() {
    const PREFIX: &str = "RECOVERED_PREFIX: staging only; never modify production.\n";
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("session evidence".to_owned()),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::TextThenTransportFault(PREFIX.to_owned()),
            AutoCompactScript::Text(valid_summary("continued summary")),
        ]),
    ])
    .await;
    let seed = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
    collect_until(&mut harness.events, finished_for(seed)).await;
    let run = compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;
    {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "seed, faulted summary, successful continuation"
        );
        let continuation = request_texts(&requests[2]).join("\n");
        assert!(
            continuation.contains(PREFIX),
            "provider received the persisted partial reply"
        );
        assert!(
            continuation.contains(crate::TURN_RETRY_CONTINUE_NOTICE),
            "runtime explicitly asked for continuation"
        );
    }
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let summary: String = connection
        .query_row(
            "SELECT summary FROM session_compactions WHERE run_id=?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(summary.contains("continued summary"));
    assert!(
        summary.contains(PREFIX),
        "committed summary lost the prefix even though the retry continued it; stored summary: {summary}"
    );
}

#[tokio::test]
async fn manual_compaction_folds_all_portions_and_commits_only_the_final_summary() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text(format!("BEGIN {} END", "evidence ".repeat(12_000))),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text(valid_summary("PARTIAL_ONLY")),
            AutoCompactScript::Text(valid_summary("FINAL_ONLY")),
        ]),
    ])
    .await;
    let seed = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
    collect_until(&mut harness.events, finished_for(seed)).await;
    let run = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_through_compacted(&mut harness.events).await;
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let first = request_texts(&requests[2]).join("\n");
    let last = request_texts(&requests[3]).join("\n");
    assert!(first.contains("BEGIN"));
    assert!(last.contains("END"));
    assert!(last.contains("PARTIAL_ONLY"));
    drop(requests);
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let summary: String = connection
        .query_row(
            "SELECT summary FROM session_compactions WHERE run_id=?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(summary.contains("FINAL_ONLY"));
    assert!(!summary.contains("PARTIAL_ONLY"));
}

#[tokio::test]
async fn manual_fold_restart_resets_abandoned_response_recovery() {
    for transport_fault in [true, false] {
        let fragment = |text: &str| {
            if transport_fault {
                AutoCompactScript::TextThenTransportFault(text.to_owned())
            } else {
                AutoCompactScript::Truncated(text.to_owned())
            }
        };
        let allowance = usize::from(if transport_fault {
            crate::MAX_TURN_RETRIES
        } else {
            crate::MAX_OUTPUT_CONTINUATIONS
        });
        let mut script = (0..allowance)
            .map(|_| fragment("ABANDONED partial summary"))
            .collect::<Vec<_>>();
        script.extend([
            AutoCompactScript::ContextOverflow,
            fragment("RESTARTED prefix\n"),
            AutoCompactScript::Text(valid_summary("intermediate")),
            AutoCompactScript::Text(valid_summary("FINAL_RESTART")),
        ]);
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text("evidence ".repeat(12_000)),
            AutoCompactScript::Sequence(script),
        ])
        .await;
        let seed = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
        collect_until(&mut harness.events, finished_for(seed)).await;
        let run = compact_session(&harness.runtime, harness.session_id).await;
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        assert!(observed.iter().any(|event| matches!(event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if run_id == run
    )), "{observed:?}");
        let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
        let summary: String = connection
            .query_row(
                "SELECT summary FROM session_compactions WHERE run_id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(summary.contains("FINAL_RESTART"));
        assert!(!summary.contains("ABANDONED"));
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), allowance + 5);
        assert!(
            !request_texts(requests.last().unwrap())
                .join("\n")
                .contains("ABANDONED")
        );
    }
}

#[tokio::test]
async fn a_single_inherited_summary_recovers_at_the_same_cutoff() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("evidence ".repeat(12_000)),
        AutoCompactScript::Text(valid_summary("old summary")),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text(valid_summary("replacement summary")),
            AutoCompactScript::Text("recovered".to_owned()),
        ]),
    ])
    .await;
    let seed = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
    collect_until(&mut harness.events, finished_for(seed)).await;
    compact_session(&harness.runtime, harness.session_id).await;
    collect_through_compacted(&mut harness.events).await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "continue".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == run
    )));
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests[2].messages().len(), 2);
    assert_eq!(requests.len(), 5);
    assert!(request_texts(&requests[4])[0].contains("replacement summary"));
    drop(requests);
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let cutoffs = connection
        .prepare("SELECT cutoff_ordinal FROM session_compactions ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, u64>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(cutoffs.len(), 2);
    assert_eq!(cutoffs[0], cutoffs[1]);
}

#[tokio::test]
async fn inherited_context_overflow_recovers_the_same_prompt_without_manual_compaction() {
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("evidence ".repeat(12_000)),
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text(valid_summary("first portion recovered")),
            AutoCompactScript::Text(valid_summary("inherited history recovered")),
            AutoCompactScript::Text("continued automatically".to_owned()),
        ]),
        AutoCompactScript::Text("follow up".to_owned()),
    ])
    .await;
    let seed = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
    collect_until(&mut harness.events, finished_for(seed)).await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "continue".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == run
    )), "{:?}", observed.iter().filter_map(|event| match &event.event { SessionEvent::RunFinished { outcome, .. } => Some(outcome), _ => None }).collect::<Vec<_>>());
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(
            crate::measure_messages(requests[3].messages())
                < crate::measure_messages(requests[2].messages())
        );
        assert!(
            request_texts(&requests[5])
                .iter()
                .any(|text| text == "continue")
        );
    }
    let next = queue_prompt(&harness.runtime, harness.session_id, "after".to_owned()).await;
    collect_until(&mut harness.events, finished_for(next)).await;
    let requests = harness.requests.lock().unwrap();
    let replay = request_texts(requests.last().unwrap());
    assert!(
        replay
            .iter()
            .any(|text| text.contains("inherited history recovered")),
        "{replay:?}"
    );
    assert!(replay.iter().any(|text| text == "continue"));
    assert!(
        !replay
            .iter()
            .any(|text| text == &"evidence ".repeat(12_000))
    );
}

#[tokio::test]
async fn provider_overflow_can_compact_active_turns_then_the_inherited_prefix() {
    let mut steps = vec![
        AutoCompactScript::WritesRepeatedlyWithSummaries {
            turns: 12,
            text: "unused".to_owned(),
            summary: valid_summary("unused"),
        };
        12
    ];
    steps.extend([
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Text(valid_summary("active summary")),
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Text(valid_summary("inherited summary")),
        AutoCompactScript::Text("recovered".to_owned()),
    ]);
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![
                AutoCompactScript::Text("history ".repeat(12_000)),
                AutoCompactScript::Sequence(steps),
            ],
            loads: StdMutex::new(0),
            context_window: None,
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let seed = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "first task".to_owned(),
    )
    .await;
    collect_until(&mut harness.events, finished_for(seed)).await;
    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "finish the task".to_owned(),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    assert_eq!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    );
    let requests = harness.requests.lock().unwrap();
    let final_context = request_texts(requests.last().unwrap()).join("\n");
    assert!(final_context.contains("inherited summary"));
    assert!(final_context.contains("active summary"));
    drop(requests);
    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let calls: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE run_id=?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(calls, 12);
}

#[tokio::test]
async fn a_context_overflow_failure_compacts_before_the_next_prompt() {
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
    let mut scripts = vec![AutoCompactScript::ReadNoteThenText("read it".to_owned())];
    scripts.extend((0..CONTEXT_PRUNE_KEEP_TURNS).map(|_| AutoCompactScript::Text("ok".to_owned())));
    scripts.extend([
        AutoCompactScript::Sequence(vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text(valid_summary("the summary")),
            AutoCompactScript::Text("recovered".to_owned()),
        ]),
        AutoCompactScript::Text("follow up".to_owned()),
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
            outcome: RunOutcome::Completed,
            ..
        } if *run_id == overflow
    )));
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    let requests_before_retry = harness.requests.lock().unwrap().len();
    {
        let requests = harness.requests.lock().unwrap();
        let overflowing = &requests[requests.len() - 3];
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
    assert!(
        observed
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert_eq!(
        harness.requests.lock().unwrap().len(),
        requests_before_retry + 1,
        "the follow-up needs no manual retry or second compaction"
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
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");
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
    assert!(texts[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE));
    assert!(texts[0].contains("--- user message #1 ---\ngrow\n"));
    assert!(!texts[0].contains(&"x".repeat(20 * 1024 * 4)));
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
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == second
    )));
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 3 + usize::from(crate::MAX_TURN_RETRIES));
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE));
    assert!(texts[0].contains("--- user message #1 ---\ngrow\n"));
    assert!(!texts[0].contains(&"x".repeat(20 * 1024 * 4)));
    assert!(!texts.iter().any(|text| text.starts_with("grow")));
}

#[tokio::test]
async fn a_compaction_that_does_not_shrink_the_assembly_never_loops() {
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("x".repeat(20 * 1024 * 4)),
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

    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: prompt },
        )
        .await
        .unwrap();
    let mut observed = collect_until(&mut harness.events, finished_for(compaction)).await;
    if !observed
        .iter()
        .any(|event| matches!(event.event,SessionEvent::RunFinished {run_id,..} if run_id==prompt))
    {
        observed.extend(collect_until(&mut harness.events, finished_for(prompt)).await);
    }
    for run in [prompt, compaction] {
        assert!(observed.iter().any(|event|matches!(event.event,SessionEvent::RunFinished {run_id,outcome:RunOutcome::Cancelled,..} if run_id==run)),"both owner and helper settle cancelled");
        assert_eq!(observed.iter().filter(|event|matches!(event.event,SessionEvent::RunFinished {run_id,..} if run_id==run)).count(),1,"each owner/helper settles once");
    }
    assert!(
        position_of(
            &observed,
            |event| matches!(event,SessionEvent::RunFinished {run_id,..} if *run_id==compaction)
        ) < position_of(
            &observed,
            |event| matches!(event,SessionEvent::RunFinished {run_id,..} if *run_id==prompt)
        ),
        "owner reservation is released after helper drain"
    );
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
    let session = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Cancelled,
                session,
                ..
            } if *run_id == prompt => Some(session.clone()),
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

    let compaction = compact_session(&harness.runtime, harness.session_id).await;
    collect_until(
        &mut harness.events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == compaction),
    )
    .await;

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
    assert!(
        !results[1].starts_with("[pruned"),
        "mutating results must never be pruned, got {:?}",
        results[1]
    );
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
    let prose = validate_compaction_summary(
        "1. Intent: fix the open problems: they matter\n2. Decisions and constraints: none\n\
         3. Work state: done\n5. Next step: ship",
    )
    .unwrap_err();
    assert_eq!(
        prose,
        "compaction summary is missing required sections: Open problems"
    );
    let old = validate_compaction_summary(&old_format_summary("x")).unwrap_err();
    assert_eq!(
        old,
        "compaction summary is missing required sections: Open problems, Next step"
    );
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
    let command = "for n in $(seq 1 1500); do if [ $n -eq 1000 ]; then echo \
         'TOKEN=AKIAIOSFODNN7EXAMPLE and the rest of line one thousand'; \
         else printf 'line %5d zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\\n' $n; fi; done";
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
    let call8 = &finished[0].id.to_string()[..8];
    assert!(marker.contains(&format!("t:shell:{call8}:")), "{marker}");

    let recall = finished[1];
    assert_eq!(recall.name, "read_tool_result");
    assert!(!recall.is_error, "{:?}", recall.result);
    let page = recall.result.as_deref().unwrap();
    let header = page.lines().next().unwrap();
    assert!(header.starts_with("read_tool_result t:shell:"), "{header}");
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
    assert_eq!(texts[0].matches(COMPACTION_RECORD_HEADER).count(), 1);
    assert_eq!(texts[0].matches("1. Intent: narrative only").count(), 1);
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
    assert_eq!(retained, COMPACTION_HISTORY_ROWS + 1);
}

#[tokio::test]
async fn an_old_format_summary_recovers_inputs_before_a_later_fold() {
    let hidden = format!("hidden{} end", " alpha".repeat(5_000));
    let span = [
        format!("second{} end", " beta".repeat(1_800)),
        format!("third{} end", " gamma".repeat(1_500)),
    ];
    let mut harness = auto_compact_harness(vec![
        AutoCompactScript::Text("first answer".to_owned()),
        AutoCompactScript::Text(old_format_summary("old")),
        AutoCompactScript::Text("second answer ".repeat(4_000)),
        AutoCompactScript::Text("third answer ".repeat(4_000)),
        AutoCompactScript::Text(valid_summary("folded")),
        AutoCompactScript::Text("after".to_owned()),
    ])
    .await;
    let first = queue_prompt(&harness.runtime, harness.session_id, hidden.clone()).await;
    collect_until(&mut harness.events, finished_for(first)).await;
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
        after_bytes < before_bytes,
        "exact inputs remain while work evidence shrinks: {before_bytes} -> {after_bytes} ({before})"
    );

    let next = queue_prompt(&harness.runtime, harness.session_id, "next".to_owned()).await;
    collect_until(&mut harness.events, finished_for(next)).await;
    let requests = harness.requests.lock().unwrap();
    let texts = request_texts(requests.last().unwrap());
    assert!(texts[0].contains("5. Next step: folded"));
    assert!(!texts[0].contains("Files touched: old"));
    for (ordinal, text) in [(1, &hidden), (3, &span[0]), (5, &span[1])] {
        assert!(
            texts[0].contains(&format!("--- user message #{ordinal} ---\n{text}\n")),
            "user message {ordinal} must be exact after the fold"
        );
    }
}

#[tokio::test]
async fn compaction_shrinks_a_tool_heavy_assembly_at_least_eightfold() {
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
    assert!(result.starts_with("2 history match(es)"), "{result}");
    assert!(
        result.contains("[user message #1]\nnever touch"),
        "{result}"
    );
    assert!(result.contains("noted: the parser"), "{result}");
    let cursor_json = result
        .rsplit_once("next_cursor: ")
        .unwrap()
        .1
        .strip_suffix("]\n")
        .unwrap();
    let cursor = serde_json::from_str(cursor_json).unwrap();
    let page = harness
        .runtime
        .inner
        .store
        .search_history(
            harness.session_id,
            "LEGACY/PARSER".to_owned(),
            2,
            Some(cursor),
        )
        .await
        .unwrap();
    assert_eq!(page.matches.len(), 1, "{page:?}");
    assert_eq!(
        page.matches[0].citation,
        "read_file result, user message #1 turn 1 call 1"
    );
    assert!(page.matches[0].excerpt.contains("src/legacy/parser.rs"));
    assert!(page.next_cursor.is_none());
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
    let window: u32 = 32 * 1024;
    let mut harness = auto_compact_harness_with_window(
        vec![
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
    for stub in results.iter().filter(|r| r.contains("[pruned")) {
        let (header, tail) = stub.split_once('\n').expect("a read stub keeps its header");
        assert!(header.starts_with("read note.txt L"), "{header}");
        assert!(!header.contains(" h:"), "{header}");
        assert!(tail.ends_with("without if_changed_since]"), "{tail}");
    }
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
    let turns = 10;
    let mut harness = auto_compact_harness_with_limits(
        vec![
            AutoCompactScript::ReadNoteRepeatedly {
                turns,
                text: "done".to_owned(),
            },
            AutoCompactScript::Text("again".to_owned()),
        ],
        Some(24 * 1024),
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
    let after: (Option<u64>, Option<u32>) =
        Connection::open(harness.workspace_path.join("sessions.sqlite3"))
            .unwrap()
            .query_row(
                "SELECT prune_through_ordinal, prune_through_turn FROM sessions WHERE id = ?1",
                [harness.session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert_eq!(after, watermark);
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
async fn a_live_prune_classifies_earlier_runs_results_by_their_stored_effect() {
    let mut harness = auto_compact_harness_with_limits(
        vec![
            AutoCompactScript::CallThenText {
                tool: "__test_delay".to_owned(),
                arguments: serde_json::json!({
                    "delay_ms": 0,
                    "result": "d".repeat(1_800),
                })
                .to_string(),
                text: "noted".to_owned(),
            },
            AutoCompactScript::ReadNoteRepeatedly {
                turns: 8,
                text: "done".to_owned(),
            },
            AutoCompactScript::Text("again".to_owned()),
        ],
        Some(20 * 1024),
        2_048,
    )
    .await;
    std::fs::write(
        harness.workspace_path.join("note.txt"),
        format!("{}\n", "n".repeat(127)).repeat(64),
    )
    .unwrap();
    for prompt in ["remember", "read it", "and now"] {
        let run = queue_prompt(&harness.runtime, harness.session_id, prompt.to_owned()).await;
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        assert_eq!(
            finished_outcome(&observed, run),
            Some(RunOutcome::Completed),
            "{prompt}"
        );
    }
    let requests = harness.requests.lock().unwrap();
    let (previous, first) = (&requests[requests.len() - 2], &requests[requests.len() - 1]);
    assert!(
        previous
            .messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult { content, .. } if content.starts_with("[pruned: __test_delay")
            ))
    );
    assert_extends(previous, first);
}

#[tokio::test]
async fn the_prune_watermark_never_reports_success_for_a_run_without_a_prompt_row() {
    let (_directory, store, prompt) = claimed_store_fixture().await;
    store.advance_prune_watermark(&prompt, 0).await.unwrap();
    let mut compaction = prompt.panic_settlement_claim();
    compaction.identity.run_id = RunId::generate().unwrap();
    compaction.identity.kind = RunKind::Compaction;
    let session = prompt.identity.session_id.to_string();
    let run = compaction.identity.run_id.to_string();
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO runs(id, session_id, command_id, user_message_id,
                                  assistant_message_id, status, kind, created_at_ms)
                 VALUES (?1, ?2, ?1, ?1, ?1, 'running', 'compaction', 1)",
                params![run, session],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store.advance_prune_watermark(&compaction, 3).await,
        Err(SessionRuntimeError::Unavailable)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn the_proactive_threshold_stubs_stale_reads_before_it_compacts() {
    let turns = CONTEXT_PRUNE_KEEP_TURNS + 3;
    let mut scripts: Vec<AutoCompactScript> = (0..turns)
        .map(|_| AutoCompactScript::ReadNoteThenText("ok".to_owned()))
        .collect();
    scripts.push(AutoCompactScript::Text("fits now".to_owned()));
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
    assert_eq!(stubbed, turns - 3);
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
async fn a_session_without_a_window_still_sheds_stale_reads_before_the_storage_backstop() {
    let note = format!("{}\n", "n".repeat(127)).repeat(720);
    let runs = (4 * 1024 * 1024 - 512 * 1024) / (26 * 1024) + 4;
    let mut scripts: Vec<AutoCompactScript> = (0..runs)
        .map(|_| AutoCompactScript::ReadNoteThenText("ok".to_owned()))
        .collect();
    scripts.push(AutoCompactScript::Text("still fits".to_owned()));
    let mut harness = auto_compact_harness_with_limits(scripts, None, 16_384).await;
    std::fs::write(harness.workspace_path.join("note.txt"), &note).unwrap();
    for index in 0..=runs {
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
    let requests = harness.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert!(
        crate::measure_messages(last.messages()) < 4 * 1024 * 1024,
        "the last request stays under the storage backstop"
    );
    assert!(
        last.messages()
            .iter()
            .flat_map(Message::content)
            .any(|block| matches!(block, ContentBlock::ToolResult { content, .. } if content.contains("[pruned"))),
        "stale reads were stubbed at a seam"
    );
    assert!(!requests.iter().any(|request| {
        request_texts(request)
            .iter()
            .any(|text| text.contains("Summarize this conversation"))
    }));
}

#[tokio::test]
async fn a_prompt_inside_the_last_tenth_of_the_window_compacts_before_it_sends() {
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
    let turns = CONTEXT_PRUNE_KEEP_TURNS + 2;
    let mut scripts = Vec::new();
    for _ in 0..=turns {
        scripts.push(AutoCompactScript::ReadNoteThenTextMeasured {
            text: "ok".to_owned(),
            input_tokens: 500,
        });
    }
    let mut harness = auto_compact_harness_with_window(scripts, Some(6 * 1024)).await;
    std::fs::write(
        harness.workspace_path.join("note.txt"),
        format!("{}\n", "n".repeat(127)).repeat(96),
    )
    .unwrap();
    for index in 0..turns {
        if index + 1 == turns {
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

#[test]
fn history_search_is_scan_bounded_and_newest_first() {
    let directory = tempfile::tempdir().unwrap();
    let (connection, session_id) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        2_000,
        0,
        3,
        8 * 1024,
    );

    let absent = search_session_history(&connection, session_id, "no-such-term", 8, None).unwrap();
    assert!(absent.matches.is_empty());
    assert!(
        absent.next_cursor.is_some(),
        "an absent term must hit the scan budget"
    );

    let newest = search_session_history(&connection, session_id, "needle-1999", 8, None).unwrap();
    assert_eq!(newest.matches.len(), 1, "{newest:?}");
    assert!(newest.matches[0].citation.starts_with("user message #"));
    let oldest = search_session_history(&connection, session_id, "needle-0", 8, None).unwrap();
    assert!(oldest.matches.is_empty());
    assert!(oldest.next_cursor.is_some());
    let mut cursor = oldest.next_cursor;
    let mut pages = 1;
    loop {
        let page = search_session_history(&connection, session_id, "needle-0", 8, cursor).unwrap();
        pages += 1;
        if !page.matches.is_empty() {
            assert_eq!(page.matches.len(), 1);
            assert!(page.matches[0].excerpt.contains("needle-0"));
            break;
        }
        cursor = page.next_cursor;
        assert!(cursor.is_some(), "pagination lost the oldest prompt");
        assert!(pages < 1_000, "cursor failed to make progress");
    }
    assert!(pages > 1);

    let common = search_session_history(&connection, session_id, "prompt", 5, None).unwrap();
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
        ordinals.windows(2).all(|pair| pair[0] > pair[1]),
        "{ordinals:?}"
    );
    assert!(ordinals[0] > 3_990, "newest first: {ordinals:?}");

    let directory = tempfile::tempdir().unwrap();
    let (connection, session_id) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        5,
        2,
        2,
        128,
    );
    let complete =
        search_session_history(&connection, session_id, "no-such-term", 8, None).unwrap();
    assert!(complete.matches.is_empty());
    assert!(complete.next_cursor.is_none());
}

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
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            AutoCompactScript::Text("c".repeat(run_bytes)),
            AutoCompactScript::Text("d".repeat(run_bytes)),
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
    for (index, request) in requests.iter().enumerate() {
        let bytes = crate::measure_messages(request.messages());
        assert!(
            crate::sessions::context::estimate_tokens(bytes) + 256 <= u64::from(window),
            "request {index} ({bytes} bytes) was sent over the window"
        );
    }
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
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 9);
    for pair in requests[1..].windows(2) {
        assert!(
            crate::measure_messages(pair[1].messages())
                < crate::measure_messages(pair[0].messages())
        );
    }
}

#[tokio::test]
async fn a_bounded_step_that_fails_stops_the_fold_without_repeating_its_input() {
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::Text(valid_summary("step one")),
                AutoCompactScript::ContextOverflow,
                AutoCompactScript::Text("invalid summary".to_owned()),
            ]),
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::Text("invalid summary".to_owned()),
                AutoCompactScript::ContextOverflow,
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
    let requests = harness.requests.lock().unwrap();
    let steps = summarizer_requests(&requests);
    assert_eq!(requests.len(), steps.len() + 4, "only four prompt sends");
    let recovery = &steps[3..];
    assert!((2..=33).contains(&recovery.len()), "{recovery:?}");
    assert!(
        recovery.windows(2).all(|pair| pair[1].1 < pair[0].1),
        "{recovery:?}"
    );
    assert!(
        request_texts(&requests[recovery[0].0])
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
    let downgraded = request_texts(&requests[6]);
    assert!(downgraded[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE));
    assert!(downgraded[0].contains("step one"));
    assert!(
        !downgraded
            .iter()
            .any(|text| text.contains(&"b".repeat(run_bytes)))
    );
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
    let window: u32 = 32 * 1024;
    let run_bytes = 20 * 1024 * 4;
    let mut harness = auto_compact_harness_with_window(
        vec![
            AutoCompactScript::Text("a".repeat(run_bytes)),
            AutoCompactScript::Text("b".repeat(run_bytes)),
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
        tokio::task::yield_now().await;
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
async fn maximum_goal_is_verbatim_immediately_after_three_in_run_summaries() {
    for max_runs in [200, 1] {
        let mut harness = in_run_compaction_harness(72, 16 * 1024, "goal progress").await;
        let held = Arc::clone(&harness.runtime.inner.permits)
            .acquire_many_owned(harness.runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let session = harness.session_id;
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session, "objective":"é".repeat(1024),
                    "budget":{"max_goal_runs":max_runs,"deadline_ms":now_ms()+86_400_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let goal = harness
            .runtime
            .inner
            .store
            .call_write(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.check = Some(qq_protocol::GoalCheck {
                    command: "c".repeat(1024),
                    timeout_ms: 3_600_000,
                });
                goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::ForGoal);
                let until_ms = now_ms() + 86_000_000;
                goal.next_run_at_ms = until_ms;
                goal.wait = Some(qq_protocol::GoalWait {
                    until_ms,
                    reason: "wait after the explicit compaction fixture".to_owned(),
                });
                goal.notes = "n".repeat(1024);
                goal.check_output = Some("o".repeat(4096));
                goal.checklist = (1..=24)
                    .map(|id| qq_protocol::GoalChecklistItem {
                        id,
                        text: "t".repeat(120),
                        state: qq_protocol::GoalChecklistState::InProgress,
                    })
                    .collect();
                crate::sessions::goals::save(connection, &goal)?;
                Ok(goal)
            })
            .await
            .unwrap();
        let run = queue_prompt(&harness.runtime, session, "work toward the goal".to_owned()).await;
        drop(held);
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        assert!(observed.iter().any(|event| matches!(event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if run_id == run
    )), "the multi-window goal run must complete");
        let compactions = observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
            .count();
        assert!(
            compactions >= 3,
            "expected at least three real compactions, got {compactions}"
        );
        let (base, summary): (u64, String) = harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT r.context_base_bytes, c.summary FROM runs r
             JOIN session_compactions c ON c.scope_run_id = r.id
             WHERE r.id = ?1 ORDER BY c.rowid DESC LIMIT 1",
                    [run.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        {
            let requests = harness.requests.lock().unwrap();
            let framed_summary = format!(
                "{}\n\n{summary}",
                crate::sessions::IN_RUN_COMPACTION_PREAMBLE
            );
            let rebased_request = requests
                .iter()
                .find(|request| {
                    let texts = request_texts(request);
                    !texts
                        .last()
                        .is_some_and(|text| text.contains("Summarize this conversation"))
                        && texts.iter().any(|text| text == &framed_summary)
                })
                .expect("a provider request must follow the latest durable summary");
            let request_weight = (rebased_request
                .system()
                .expect("compiled system prompt")
                .len() as u64)
                .saturating_add(crate::CONTEXT_BLOCK_FRAMING_BYTES)
                .saturating_add(
                    crate::runtime::tool_schema_measurement(rebased_request.tools()).bytes,
                )
                .saturating_add(crate::measure_messages(rebased_request.messages()));
            assert_eq!(
                base, request_weight,
                "durable rebase must include the post-summary goal notice"
            );
            let mut after_summary = 0;
            for request in requests.iter() {
                if request_texts(request)
                    .last()
                    .is_some_and(|text| text.contains("Summarize this conversation"))
                {
                    continue;
                }
                let texts = request_texts(request);
                for (index, text) in texts.iter().enumerate() {
                    if !text.starts_with(crate::sessions::IN_RUN_COMPACTION_PREAMBLE) {
                        continue;
                    }
                    let notice = texts
                        .get(index + 1)
                        .expect("a summary must have a following goal notice");
                    assert!(
                        notice.starts_with("[QQ runtime notice; not a user instruction]"),
                        "goal notice must immediately follow summary"
                    );
                    for field in [
                        &goal.objective,
                        &goal.notes,
                        &goal.check.as_ref().unwrap().command,
                        goal.check_output.as_ref().unwrap(),
                    ] {
                        assert!(
                            notice.contains(field),
                            "maximum goal field must remain verbatim"
                        );
                    }
                    assert!(notice.contains(&format!("[>] 24 {}", "t".repeat(120))));
                    assert!(notice.len() <= crate::tools::goal::MAX_GOAL_RENDER_BYTES);
                    after_summary += 1;
                }
            }
            assert!(after_summary >= 3);
        }
        let expected = {
            let requests = harness.requests.lock().unwrap();
            let mut messages = requests.last().unwrap().messages().to_vec();
            messages.push(Message::assistant("task complete"));
            messages
        };
        let path = harness.workspace_path.join("sessions.sqlite3");
        harness.runtime.close().await.unwrap();
        drop(harness.runtime);
        let reopened = Store::open(path).await.unwrap();
        let replayed = reopened
            .call(Priority::Control, move |connection| {
                let transaction = connection.transaction()?;
                load_model_context(&transaction, session, u64::MAX)
            })
            .await
            .unwrap();
        assert!(replayed.first().is_some_and(|message| message.content().iter().any(|block| {
        matches!(block, ContentBlock::Text { text } if text.starts_with("[QQ runtime notice; not a user instruction]"))
    })), "reopened context must retain the exact initial goal notice before the accepted prompt");
        assert_eq!(
            replayed, expected,
            "reopened goal context must match the actual model window, including summary notices"
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn one_run_spanning_several_windows_compacts_its_own_turns_and_completes() {
    let turns = 48;
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
        for at in [started, finished] {
            let (SessionEvent::RunStarted { session, .. }
            | SessionEvent::RunFinished { session, .. }) = &observed[at].event
            else {
                unreachable!()
            };
            assert_eq!(session.activity, Some(RunActivity::Compacting));
        }
        assert!(
            matches!(
                &observed[started + 1],
                SessionEventEnvelope { run_id: Some(id), event: SessionEvent::RunActivityChanged { activity: RunActivity::Compacting, .. }, cursor, .. }
                    if id == compaction && cursor.sequence == observed[started].cursor.sequence + 1
            ),
            "{:?}",
            observed[started + 1].event
        );
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
            let reserve = u64::from(request.max_output_tokens());
            assert!(
                crate::sessions::context::estimate_tokens(bytes) + reserve <= 16 * 1024,
                "request {index} ({bytes} bytes + {reserve} reserve) was sent over the window"
            );
        }
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

    let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
    let calls: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1 AND state = 'completed'",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(calls, turns as u64);
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
    let (attempted, base, increment): (u32, u64, u64) = connection
        .query_row(
            "SELECT context_compaction_attempted, context_base_bytes, context_increment_bytes
             FROM runs WHERE id = ?1",
            [run.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(attempted, 0, "in-run steps are off the between-run budget");
    let lifetime_result_bytes: u64 = connection
        .query_row(
            "SELECT SUM(length(CAST(result AS BLOB))) FROM tool_calls WHERE run_id = ?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        base + increment < lifetime_result_bytes,
        "reservation {base} + {increment} must be re-based below the run's lifetime {lifetime_result_bytes} result bytes"
    );
    drop(connection);

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
    let session = harness.session_id.to_string();
    assert_eq!(summary.cache_key(), Some(session.as_str()));
    assert!(
        requests
            .iter()
            .all(|request| request.cache_key() == Some(session.as_str()))
    );
}

#[tokio::test]
async fn a_dense_tokenizer_compacts_in_run_instead_of_failing_at_the_guard() {
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
                if message.contains("automatic context recovery") && message.contains("summarizer failed")
        ),
        "{outcome:?}"
    );
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

const STALL_SUMMARY: &str = "__STALL__";

#[test]
fn compaction_retention_preserves_recent_rollback_views_and_reclaims_covered_runs() {
    let (_directory, mut connection, session, runs) =
        record_fixture(&(0..6).map(|i| format!("task {i}")).collect::<Vec<_>>());
    for run in &runs {
        for turn in 1..=5 {
            connection
                .execute(
                    "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                 VALUES (?1, ?2, ?3)",
                    params![
                        run.to_string(),
                        turn,
                        serde_json::to_string(&vec![PersistedContentBlock::Text {
                            text: format!("raw turn {turn}")
                        }])
                        .unwrap()
                    ],
                )
                .unwrap();
        }
    }
    let markers = [
        (Some(0), 1),
        (None, 2),
        (Some(1), 1),
        (Some(1), 2),
        (Some(1), 3),
        (Some(1), 4),
        (Some(2), 1),
        (Some(3), 1),
        (Some(4), 1),
        (Some(4), 2),
        (Some(4), 3),
        (Some(4), 4),
        (None, 10),
        (Some(5), 1),
        (Some(5), 2),
        (Some(5), 3),
        (Some(5), 4),
        (None, 12),
        (None, 12),
        (None, 12),
        (None, 12),
    ];
    for (index, (scope, cutoff)) in markers.into_iter().enumerate() {
        connection.execute(
            "INSERT INTO session_compactions(session_id, run_id, summary,
                 cutoff_ordinal, before_bytes, after_bytes, created_at_ms, scope_run_id, turn_cutoff)
             VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6, ?7)",
            params![session.to_string(), RunId::generate().unwrap().to_string(), format!("marker {index}"),
                if scope.is_none() { cutoff } else { 0 }, index,
                scope.map(|i| runs[i].to_string()), scope.map(|_| cutoff)],
        ).unwrap();

        let transaction = connection.transaction().unwrap();
        transaction
            .execute_batch("SAVEPOINT before_collection")
            .unwrap();
        let mut expected = Vec::new();
        for _ in 0..=COMPACTION_HISTORY_ROWS {
            expected.push(load_model_context(&transaction, session, u64::MAX).unwrap());
            transaction
                .execute(
                    "DELETE FROM session_compactions WHERE session_id = ?1
                AND rowid = (SELECT MAX(rowid) FROM session_compactions WHERE session_id = ?1)",
                    [session.to_string()],
                )
                .unwrap();
        }
        transaction
            .execute_batch("ROLLBACK TO before_collection; RELEASE before_collection")
            .unwrap();
        prune_compaction_history(&transaction, session).unwrap();
        let retained: usize = transaction
            .query_row(
                "SELECT COUNT(*) FROM session_compactions WHERE session_id = ?1",
                [session.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained <= 4 + runs.len(), "step {index}: {retained}");
        transaction
            .execute_batch("SAVEPOINT after_collection")
            .unwrap();
        for (rollback, expected) in expected.iter().enumerate() {
            assert_eq!(
                &load_model_context(&transaction, session, u64::MAX).unwrap(),
                expected,
                "step {index}, rollback {rollback}"
            );
            transaction
                .execute(
                    "DELETE FROM session_compactions WHERE session_id = ?1
                AND rowid = (SELECT MAX(rowid) FROM session_compactions WHERE session_id = ?1)",
                    [session.to_string()],
                )
                .unwrap();
        }
        transaction
            .execute_batch("ROLLBACK TO after_collection; RELEASE after_collection")
            .unwrap();
        transaction.commit().unwrap();
    }
    let (total, scoped): (u32, u32) = connection
        .query_row(
            "SELECT COUNT(*), COUNT(scope_run_id) FROM session_compactions WHERE session_id = ?1",
            [session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (total, scoped),
        (4, 0),
        "covered runs and obsolete marker versions are reclaimed"
    );
}

#[tokio::test]
async fn repeated_in_run_compaction_retains_the_session_prefix_after_restart() {
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![
                AutoCompactScript::Text("initial reply".to_owned()),
                AutoCompactScript::Text(valid_summary("session prefix")),
                AutoCompactScript::WritesRepeatedlyWithSummaries {
                    turns: 32,
                    text: "task complete".to_owned(),
                    summary: valid_summary("active work"),
                },
            ],
            loads: StdMutex::new(0),
            context_window: Some(12 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let initial = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "original task".to_owned(),
    )
    .await;
    collect_until(&mut harness.events, finished_for(initial)).await;
    let prefix = compact_session(&harness.runtime, harness.session_id).await;
    let observed = collect_until(&mut harness.events, finished_for(prefix)).await;
    assert_eq!(
        finished_outcome(&observed, prefix),
        Some(RunOutcome::Completed)
    );
    collect_through_compacted(&mut harness.events).await;

    let run = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "continue the task".to_owned(),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run)).await;
    assert_eq!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    );
    assert!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
            .count()
            >= 3
    );
    let mut expected = harness
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages()
        .to_vec();
    expected.push(Message::assistant("task complete"));
    let session = harness.session_id;
    let database = harness.workspace_path.join("sessions.sqlite3");
    harness.runtime.close().await.unwrap();

    let (mut connection, _) = open_database(&database).unwrap();
    let transaction = connection.transaction().unwrap();
    let prefix = latest_compaction(&transaction, session)
        .unwrap()
        .expect("in-run compaction must retain the session prefix");
    assert!(prefix.summary.contains("session prefix"));
    let replayed = load_model_context(&transaction, session, u64::MAX).unwrap();
    assert_eq!(
        replayed, expected,
        "restart must preserve the live request and completed reply"
    );
    let calls: u32 = transaction
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(calls, 32, "compaction must not repeat tool effects");
}

#[tokio::test]
async fn an_in_run_marker_survives_restart_and_folds_into_a_later_between_run_compaction() {
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
    let next = queue_prompt(&harness.runtime, harness.session_id, "more".to_owned()).await;
    let observed = collect_until(&mut harness.events, finished_for(next)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == next
    )));
}

#[tokio::test]
async fn an_empty_faulted_attempt_inside_the_replaced_span_keeps_replay_equal_to_live() {
    let faults = Arc::new(AtomicUsize::new(0));
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ShellRepeatedlyWithEarlyFault {
                turns: 30,
                fault_at: 2,
                text: "task complete".to_owned(),
                summary: valid_summary("work so far"),
                faults: Arc::clone(&faults),
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
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if *run_id == run
    )));
    assert!(
        faults.load(Ordering::SeqCst) >= 1,
        "the scripted fault fired"
    );
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. })),
        "the run must compact in-run"
    );
    let live = {
        let requests = harness.requests.lock().unwrap();
        requests.last().unwrap().messages().to_vec()
    };
    let store = harness.runtime.inner.store.clone();
    let session_id = harness.session_id;
    let replayed = store
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction().unwrap();
            load_model_context(&transaction, session_id, u64::MAX)
        })
        .await
        .unwrap();
    drop(store);
    assert_eq!(
        &replayed[..replayed.len() - 1],
        live.as_slice(),
        "replay keeps exactly the turns the live run kept"
    );
    harness.runtime.shutdown().await.unwrap();
    assert_assembly_matches_reference(
        &harness.workspace_path.join("sessions.sqlite3"),
        harness.session_id,
    );
}

#[tokio::test]
async fn an_exhausted_fold_admits_the_prompt_with_summary_only_history() {
    for reject_prefix in [false, true] {
        let window: u32 = 32 * 1024;
        let mut retry_steps = vec![AutoCompactScript::Text("invalid summary".to_owned())];
        if reject_prefix {
            retry_steps.extend([
                AutoCompactScript::ContextOverflow,
                AutoCompactScript::Text(valid_summary("recovered prefix")),
            ]);
        }
        retry_steps.push(AutoCompactScript::Text("recovered".to_owned()));
        let run_bytes = 20 * 1024 * 4;
        let mut harness = auto_compact_harness_with_window(
            vec![
                AutoCompactScript::Text("a".repeat(run_bytes)),
                AutoCompactScript::Text("b".repeat(run_bytes)),
                AutoCompactScript::Sequence(vec![
                    AutoCompactScript::Text(valid_summary("step one")),
                    AutoCompactScript::ContextOverflow,
                    AutoCompactScript::Text("invalid summary".to_owned()),
                ]),
                AutoCompactScript::Sequence(retry_steps),
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
        assert_eq!(texts.len(), 2, "{texts:?}");
        if reject_prefix {
            assert!(texts[0].contains("recovered prefix"));
            let summarizer = request_texts(&requests[requests.len() - 2]);
            assert!(
                summarizer
                    .iter()
                    .any(|text| text.contains(&"b".repeat(run_bytes))),
                "a summary-only overflow must read the omitted history before advancing its cutoff"
            );
        } else {
            assert!(
                texts[0].starts_with(crate::sessions::SUMMARY_ONLY_NOTICE),
                "{}",
                texts[0]
            );
            assert!(texts[0].contains("step one"));
        }
        assert!(texts[0].contains(COMPACTION_SUMMARY_PREAMBLE));
        for (ordinal, input) in [(1, "one"), (3, "two"), (5, "three")] {
            assert!(
                texts[0].contains(&format!("--- user message #{ordinal} ---\n{input}\n")),
                "summary-only admission must preserve even inputs after the latest marker"
            );
        }
        assert_eq!(texts[1], "retry");
        assert!(
            !texts
                .iter()
                .any(|text| text.contains(&"b".repeat(run_bytes)))
        );
        drop(requests);
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
}

#[test]
fn replay_drops_the_notice_and_steering_the_in_run_splice_removed() {
    use crate::runtime::TurnNotice;
    let assistant = |n: u32| Message::assistant(format!("turn {n}"));
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
    assert!(!live.contains(&Message::user(TurnNotice::Continuation.text())));
    assert!(!live.contains(&Message::user("steer before four")));

    let turns = (1..=7)
        .map(|n| StoredTurn {
            goal_notices: Vec::new(),
            strict_projection: true,
            checkpoint_projection_required: false,
            projection_json: None,
            run_id: "fixture".to_owned(),
            legacy_projection: false,
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
            goal_notice: None,
            summary: "summary".to_owned(),
            turn_cutoff: replaced_turns,
        }),
        &mut replayed,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(replayed, live);

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
            goal_notices: Vec::new(),
            strict_projection: true,
            checkpoint_projection_required: false,
            projection_json: None,
            run_id: "fixture".to_owned(),
            legacy_projection: false,
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
            goal_notice: None,
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
            goal_notices: Vec::new(),
            strict_projection: true,
            checkpoint_projection_required: false,
            projection_json: None,
            run_id: "fixture".to_owned(),
            legacy_projection: false,
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
            goal_notice: None,
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
    let run: Vec<Message> = (0..6).flat_map(|_| [assistant(), results()]).collect();
    assert_eq!(boundary(&run, 4), Some((4, 2)));
    let mut with_steer = run.clone();
    with_steer.insert(4, Message::user("steer"));
    assert_eq!(boundary(&with_steer, 4), Some((5, 2)));
    let short: Vec<Message> = (0..4).flat_map(|_| [assistant(), results()]).collect();
    assert_eq!(boundary(&short, 4), None);
    assert_eq!(boundary(&[], 4), None);
    let mut folded = vec![Message::user("summary so far")];
    folded.extend((0..5).flat_map(|_| [assistant(), results()]));
    assert_eq!(boundary(&folded, 4), Some((3, 1)));
}

#[tokio::test]
async fn an_in_run_summary_cut_at_the_output_limit_is_continued_and_joined_verbatim() {
    for with_goal in [false, true] {
        let turns = 48;
        let summary = valid_summary("continued");
        let cut = summary.find("Decis").unwrap() + "Decis".len();
        let script = AutoCompactScript::ShellRepeatedlyWithTruncatedSummaries {
            turns,
            text: "task complete".to_owned(),
            summary: summary.clone(),
            cut,
        };
        let mut harness = auto_compact_harness_with_loader_and_mode(
            AutoCompactLoader {
                requests: Arc::new(StdMutex::new(Vec::new())),
                scripts: vec![if with_goal {
                    AutoCompactScript::Measured {
                        script: Box::new(script),
                        bytes_per_token: 4,
                    }
                } else {
                    script
                }],
                loads: StdMutex::new(0),
                context_window: Some(16 * 1024),
                max_output_tokens: 1_024,
                provider_identity: true,
            },
            ApprovalMode::Full,
        )
        .await;
        let held = Arc::clone(&harness.runtime.inner.permits)
            .acquire_many_owned(harness.runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        if with_goal {
            harness.runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":harness.session_id,"objective":"bounded multi-request summary",
            "budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"max_total_tokens":1_000_000}
        })).unwrap()).await.unwrap();
        }
        let receipt = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    input: vec![InputPart::text("do the task")],
                    limits: RunLimits {
                        max_output_tokens: with_goal.then_some(1000),
                        ..RunLimits::default()
                    },
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id: run, .. } = receipt.outcome else {
            panic!("run")
        };
        drop(held);
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
        if with_goal {
            let summary_caps: Vec<_> = requests
                .iter()
                .filter(|request| {
                    request_texts(request).last().is_some_and(|text| {
                        text.contains("Summarize this conversation")
                            || text.contains("cut off at the output token limit")
                    })
                })
                .map(ModelRequest::max_output_tokens)
                .collect();
            assert_eq!(summary_caps.len(), compactions * 2);
            for pair in summary_caps.chunks_exact(2) {
                assert!(
                    pair[0] < 1000,
                    "summary input must respect the caller's remaining output cap: {pair:?}"
                );
                assert_eq!(
                    pair[1],
                    pair[0] - 1,
                    "a continuation re-quotes the measured summary prefix"
                );
            }
        }
    }
}

#[tokio::test]
async fn compacting_after_a_slice_report_replays_the_live_context() {
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
            context_window: Some(24 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let run = queue_prompt(&harness.runtime, harness.session_id, "do it".to_owned()).await;
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

#[tokio::test]
async fn snapshot_run_tail_retains_the_foreground_receipt_after_many_owned_compactions() {
    let (_directory, store, prompt) = claimed_store_fixture().await;
    let session_id = prompt.identity.session_id;
    let run_id = prompt.identity.run_id;
    store
        .call(Priority::Control, move |connection| {
            for index in 0..258 {
                let internal = RunId::generate().unwrap().to_string();
                connection.execute(
                    "INSERT INTO runs(id, session_id, command_id, user_message_id,
                 assistant_message_id, status, kind, created_at_ms, auto_compaction_for_run_id)
                 VALUES (?1, ?2, ?1, ?1, ?1, 'completed', 'compaction', ?3, ?4)",
                    params![
                        internal,
                        session_id.to_string(),
                        i64::MAX - 258 + index,
                        run_id.to_string()
                    ],
                )?;
            }
            for finished in [false, true] {
                if finished {
                    connection.execute(
                        "UPDATE runs SET status = 'cancelled', outcome_json = ?2,
                     estimated_cost_usd_nanos = 1234567, finished_at_ms = 1 WHERE id = ?1",
                        params![
                            run_id.to_string(),
                            serde_json::to_string(&RunOutcome::Cancelled).unwrap()
                        ],
                    )?;
                    connection.execute(
                        "UPDATE sessions SET active_run_id = NULL WHERE id = ?1",
                        [session_id.to_string()],
                    )?;
                }
                for limit in [1, 2, 256] {
                    let mut budget = snapshots::SnapshotBudget::new(16 * 1024 * 1024);
                    let body = snapshots::load_session_snapshot(
                        connection,
                        session_id,
                        limit,
                        &mut budget,
                    )?;
                    assert_eq!(
                        body.runs.len(),
                        usize::from(limit),
                        "the run tail stays bounded"
                    );
                    let receipt = body
                        .runs
                        .iter()
                        .find(|run| run.id == run_id)
                        .expect("foreground receipt survives newer internal steps");
                    if finished {
                        assert_eq!(receipt.outcome, Some(RunOutcome::Cancelled));
                        assert_eq!(receipt.estimated_cost_usd_nanos, Some(1234567));
                    } else {
                        assert_eq!(receipt.status, RunStatus::Running);
                    }
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn a_later_queued_cancellation_cannot_evict_receipts_for_displayed_messages() {
    let (_directory, store, prompt) = claimed_store_fixture().await;
    let session_id = prompt.identity.session_id;
    let owner = prompt.identity.run_id;
    store.call(Priority::Control,move |connection| {
        for index in 0..258 {
            let id = RunId::generate().unwrap().to_string();
            connection.execute("INSERT INTO runs(id,session_id,command_id,user_message_id,assistant_message_id,status,kind,created_at_ms,auto_compaction_for_run_id)
                VALUES (?1,?2,?1,?1,?1,'completed','compaction',?3,?4)",
                params![id,session_id.to_string(),i64::MAX-258+index,owner.to_string()])?;
        }
        Ok(())
    }).await.unwrap();
    store
        .finish_run(
            &prompt,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET estimated_cost_usd_nanos = 7654321 WHERE id = ?1",
                [owner.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let queued = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("queued later")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: later, .. } = queued.receipt.outcome else {
        panic!("queued")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: later },
        )
        .await
        .unwrap();
    store
        .call(Priority::Control, move |connection| {
            for limit in [256, 2, 1] {
                for bytes in [16 * 1024 * 1024, 1024] {
                    let mut budget = snapshots::SnapshotBudget::new(bytes);
                    let body = snapshots::load_session_snapshot(
                        connection,
                        session_id,
                        limit,
                        &mut budget,
                    )?;
                    assert!(body.runs.len() <= usize::from(limit));
                    for message in &body.messages {
                        assert!(
                            body.runs.iter().any(|run| run.id == message.run_id),
                            "every displayed message has its receipt: limit={limit},bytes={bytes}"
                        );
                    }
                    if limit == 256 && bytes > 1024 {
                        assert!(body.messages.iter().any(|message| message.run_id == owner));
                        let receipt = body.runs.iter().find(|run| run.id == owner).unwrap();
                        assert_eq!(receipt.outcome, Some(RunOutcome::Completed));
                        assert_eq!(receipt.estimated_cost_usd_nanos, Some(7654321));
                    }
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[test]
fn history_recall_includes_applied_steering_and_live_run_evidence_without_search_echoes() {
    let directory = tempfile::tempdir().unwrap();
    let (connection, session) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        1,
        0,
        1,
        128,
    );
    connection
        .execute("UPDATE runs SET status = 'running'", [])
        .unwrap();
    for (ordinal, state, text) in [
        (10, "complete", "APPLIED_CONSTRAINT"),
        (11, "queued", "PENDING_CONSTRAINT"),
    ] {
        connection.execute(
            "INSERT INTO messages(id, session_id, run_id, ordinal, turn_ordinal, role, state, output, created_at_ms, steering)
             SELECT ?1, session_id, run_id, ?2, 2, 'user', ?3, ?4, 1, 1 FROM messages WHERE ordinal = 1",
            params![MessageId::generate().unwrap().to_string(), ordinal, state, text],
        ).unwrap();
    }
    connection
        .execute("UPDATE tool_calls SET result = 'EARLIER_LIVE_RESULT'", [])
        .unwrap();
    let live =
        search_session_history(&connection, session, "EARLIER_LIVE_RESULT", 20, None).unwrap();
    assert!(
        live.matches
            .iter()
            .any(|hit| hit.citation.contains("result")),
        "{live:?}"
    );
    let applied =
        search_session_history(&connection, session, "APPLIED_CONSTRAINT", 8, None).unwrap();
    assert_eq!(applied.matches.len(), 1);
    assert_eq!(applied.matches[0].citation, "user message #10");
    assert!(
        search_session_history(&connection, session, "PENDING_CONSTRAINT", 8, None)
            .unwrap()
            .matches
            .is_empty()
    );

    let raw: String = connection
        .query_row(
            "SELECT assistant_content_json FROM model_turns",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut blocks: serde_json::Value = serde_json::from_str(&raw).unwrap();
    blocks.as_array_mut().unwrap().push(serde_json::json!({
        "type": "tool_call", "id": "history-call", "name": "search_history", "arguments": {"query": "SEARCH_ECHO"}
    }));
    connection
        .execute(
            "UPDATE model_turns SET assistant_content_json = ?1",
            [blocks.to_string()],
        )
        .unwrap();
    connection.execute(
        "INSERT INTO tool_calls(id, run_id, turn_ordinal, call_ordinal, provider_call_id, name, arguments_json, state, result, requested_at_ms, effect)
         SELECT ?1, run_id, turn_ordinal, 2, 'history-call', 'search_history', ?2, 'completed', 'SEARCH_ECHO', 1, 'read_only'
         FROM tool_calls WHERE call_ordinal = 1",
        params![ToolCallId::generate().unwrap().to_string(), serde_json::json!({"query": "SEARCH_ECHO"}).to_string()],
    ).unwrap();
    assert!(
        search_session_history(&connection, session, "SEARCH_ECHO", 8, None)
            .unwrap()
            .matches
            .is_empty()
    );
    let foreign = search_session_history(
        &connection,
        SessionId::from_bytes([7; 16]),
        "read_file",
        8,
        Some(HistoryCursor::Result {
            prompt: 1,
            turn: 1,
            before: u16::MAX,
        }),
    )
    .unwrap();
    assert!(foreign.matches.is_empty());
    assert!(foreign.next_cursor.is_none());
}

#[test]
fn history_pages_cover_oversized_turns_without_skipping_older_sources() {
    let directory = tempfile::tempdir().unwrap();
    let (connection, session) = bench_support::seed_compacted_session(
        &directory.path().join("sessions.sqlite3"),
        2,
        0,
        1,
        128,
    );
    connection
        .execute(
            "UPDATE messages SET output = 'PAGED_NEEDLE oldest constraint' WHERE ordinal = 1",
            [],
        )
        .unwrap();
    let raw: String = connection.query_row(
        "SELECT assistant_content_json FROM model_turns WHERE run_id = (SELECT run_id FROM messages WHERE ordinal = 3)",
        [], |row| row.get(0),
    ).unwrap();
    let mut blocks: serde_json::Value = serde_json::from_str(&raw).unwrap();
    blocks[0]["text"] = serde_json::Value::String(format!(
        "{} PAGED_NEEDLE",
        "x".repeat(HISTORY_SCAN_BUDGET_BYTES + 1)
    ));
    connection.execute(
        "UPDATE model_turns SET assistant_content_json = ?1 WHERE run_id = (SELECT run_id FROM messages WHERE ordinal = 3)",
        [blocks.to_string()],
    ).unwrap();
    let first = search_session_history(&connection, session, "PAGED_NEEDLE", 8, None).unwrap();
    assert_eq!(first.matches.len(), 1);
    assert!(first.matches[0].citation.starts_with("assistant"));
    let next = first
        .next_cursor
        .expect("large row must stop the page after examining it");
    let second =
        search_session_history(&connection, session, "PAGED_NEEDLE", 8, Some(next)).unwrap();
    assert_eq!(
        second.matches.len(),
        1,
        "the large row must not be searched twice"
    );
    assert_eq!(second.matches[0].citation, "user message #1");
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn an_empty_fold_preserves_the_uncompacted_transcript() {
    for (partial, final_text, requests) in [
        (
            String::new(),
            valid_summary("FINAL WITHOUT EARLIER EVIDENCE"),
            3,
        ),
        (
            " \n\t".to_owned(),
            valid_summary("FINAL WITHOUT EARLIER EVIDENCE"),
            3,
        ),
        (valid_summary("PRECEDING PORTION ONLY"), String::new(), 4),
    ] {
        let evidence = "UNSUMMARIZED FINAL PORTION ".repeat(6_000);
        let mut harness = auto_compact_harness(vec![
            AutoCompactScript::Text(evidence.clone()),
            AutoCompactScript::Sequence(vec![
                AutoCompactScript::ContextOverflow,
                AutoCompactScript::Text(partial),
                AutoCompactScript::Text(final_text),
            ]),
        ])
        .await;
        let seed = queue_prompt(&harness.runtime, harness.session_id, "seed".to_owned()).await;
        collect_until(&mut harness.events, finished_for(seed)).await;
        let run = compact_session(&harness.runtime, harness.session_id).await;
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. })),
            "an empty portion must not advance the cutoff: {observed:?}"
        );
        assert!(
            observed.iter().any(|event| matches!(&event.event,
                SessionEvent::RunFinished { run_id, outcome: RunOutcome::Failed { failure }, .. }
                    if *run_id == run && failure.message.contains("empty summary")
            )),
            "{observed:?}"
        );
        let connection = Connection::open(harness.workspace_path.join("sessions.sqlite3")).unwrap();
        let markers: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM session_compactions WHERE session_id = ?1",
                [harness.session_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(markers, 0, "failed folds must leave the cutoff unchanged");
        let context = load_model_context(&connection, harness.session_id, u64::MAX).unwrap();
        assert!(
            context
                .iter()
                .flat_map(Message::content)
                .any(|block| matches!(block, ContentBlock::Text { text } if text == &evidence))
        );
        assert_eq!(harness.requests.lock().unwrap().len(), requests);
    }
}

#[test]
fn legacy_summary_projection_recovers_exact_inputs_without_another_compaction() {
    let early = format!(
        "Original task: preserve the protected files.\n{}\nNEVER_MODIFY_PRODUCTION_TAIL",
        "é".repeat(40_000)
    );
    let correction =
        "Correction: write the report to staging/new.txt instead of staging/old.txt.".to_owned();
    let (_directory, connection, session, _) = record_fixture(&[early.clone(), correction.clone()]);
    connection.execute(
        "INSERT INTO session_compactions(session_id, run_id, summary, cutoff_ordinal, before_bytes, after_bytes, created_at_ms)
         VALUES (?1, ?2, ?3, 4, 1, 1, 1)",
        params![session.to_string(), RunId::generate().unwrap().to_string(), valid_summary("legacy narrative omitted the original instructions")],
    ).unwrap();
    let context = load_model_context(&connection, session, u64::MAX).unwrap();
    assert_eq!(context.len(), 1);
    assert_eq!(context[0].role(), Role::User);
    let ContentBlock::Text { text } = &context[0].content()[0] else {
        panic!("user context");
    };
    assert!(
        text.contains(&early),
        "legacy summaries must recover complete durable instructions without a manual compaction"
    );
    assert!(text.rfind(&correction).unwrap() > text.find(&early).unwrap());
}

#[tokio::test]
async fn early_constraints_and_corrections_survive_repeated_automatic_compaction() {
    let early = format!(
        "Original task: preserve protected files.\n{}\nNEVER_MODIFY_PRODUCTION_TAIL",
        "é".repeat(40_000)
    );
    let evidence = "work evidence ".repeat(8_000);
    let mut scripts = vec![AutoCompactScript::Text(evidence.clone())];
    for _ in 0..4 {
        scripts.push(AutoCompactScript::Sequence(vec![
            AutoCompactScript::ContextOverflow,
            AutoCompactScript::Text(valid_summary(
                "work saved; exact inputs deliberately omitted",
            )),
            AutoCompactScript::Text(evidence.clone()),
        ]));
    }
    scripts.push(AutoCompactScript::Sequence(vec![
        AutoCompactScript::ContextOverflow,
        AutoCompactScript::Text("   ".to_owned()),
    ]));
    let mut harness = auto_compact_harness(scripts).await;
    let seed = queue_prompt(&harness.runtime, harness.session_id, early.clone()).await;
    collect_until(&mut harness.events, finished_for(seed)).await;
    let mut corrections = Vec::new();
    for index in 0..4 {
        let correction = format!(
            "Correction {index}: write only staging/report-{index}.txt; preserve production."
        );
        corrections.push(correction.clone());
        let run = queue_prompt(&harness.runtime, harness.session_id, correction).await;
        let observed = collect_until(&mut harness.events, finished_for(run)).await;
        assert!(observed.iter().any(|event| matches!(event.event,
            SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. } if run_id == run
        )), "automatic recovery failed in round {index}");
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
        );
        let requests = harness.requests.lock().unwrap();
        let texts = request_texts(requests.last().unwrap());
        let text = texts.join("\n");
        assert!(
            text.contains(&early),
            "compaction discarded an early instruction beyond the old record cap in round {index}"
        );
        assert_eq!(text.matches("NEVER_MODIFY_PRODUCTION_TAIL").count(), 1);
        let mut previous = text.find(&early).unwrap();
        for correction in &corrections {
            let position = text.rfind(correction).unwrap();
            assert!(position > previous, "corrections must keep their order");
            previous = position;
        }
    }
    let database = harness.workspace_path.join("sessions.sqlite3");
    let marker = || {
        Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT MAX(rowid) FROM session_compactions WHERE session_id = ?1",
                [harness.session_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .unwrap()
    };
    let before_failure = marker();
    let failed = queue_prompt(
        &harness.runtime,
        harness.session_id,
        "continue the same task".to_owned(),
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(failed)).await;
    assert!(matches!(
        finished_outcome(&observed, failed),
        Some(RunOutcome::Failed { .. })
    ));
    assert_eq!(
        marker(),
        before_failure,
        "a failed summary cannot advance the cutoff"
    );
    assert!(!*harness.runtime.inner.failed.borrow());
    harness.runtime.shutdown().await.unwrap();
    let replay = load_model_context(
        &Connection::open(&database).unwrap(),
        harness.session_id,
        u64::MAX,
    )
    .unwrap();
    let text = replay
        .iter()
        .flat_map(request_texts_of)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains(&early));
    assert_eq!(text.matches("NEVER_MODIFY_PRODUCTION_TAIL").count(), 1);
    for correction in &corrections {
        assert!(text.contains(correction));
    }
}

#[test]
fn scoped_inputs_cover_steering_before_a_kept_turn_after_an_empty_fault() {
    let (_directory, connection, session, runs) = record_fixture(&["task".to_owned()]);
    let run = runs[0];
    for (turn, content, truncated) in [
        (1, r#"[{"type":"text","text":"replaced work"}]"#, false),
        (2, "[]", true),
        (3, r#"[{"type":"text","text":"kept work"}]"#, false),
    ] {
        connection
            .execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json, truncated)
            VALUES (?1, ?2, ?3, ?4)",
                params![run.to_string(), turn, content, truncated],
            )
            .unwrap();
    }
    insert_record_steering(
        &connection,
        session,
        run,
        5,
        3,
        "Correction before kept work",
    );
    let summary = with_exact_inputs(
        &connection,
        &session.to_string(),
        RecordScope::Run {
            run_id: run,
            turn_cutoff: 1,
        },
        valid_summary("narrative"),
    )
    .unwrap();
    assert!(
        summary.contains("Correction before kept work"),
        "the live splice replaces this input"
    );
    connection.execute("INSERT INTO session_compactions(session_id, run_id, summary, cutoff_ordinal, before_bytes, after_bytes, created_at_ms, scope_run_id, turn_cutoff)
        VALUES (?1, ?2, ?3, 0, 1, 1, 1, ?4, 1)",
        params![session.to_string(), RunId::generate().unwrap().to_string(), valid_summary("narrative"), run.to_string()]).unwrap();
    let replay = load_model_context(&connection, session, u64::MAX).unwrap();
    let texts: Vec<_> = replay.iter().flat_map(request_texts_of).collect();
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("Correction before kept work"))
            .count(),
        1
    );
    assert!(
        !texts.contains(&"Correction before kept work"),
        "the retained prefix must not repeat steering"
    );
}

#[test]
fn exact_inputs_preserve_applied_corrections_and_answers_in_conversation_order() {
    let (_directory, connection, session, runs) = record_fixture(&[
        "original task".to_owned(),
        "newer queued prompt".to_owned(),
        "generated continuation".to_owned(),
    ]);
    insert_record_steering(
        &connection,
        session,
        runs[0],
        7,
        2,
        "late correction to first task",
    );
    insert_record_steering(&connection, session, runs[0], 8, 3, "unapplied correction");
    connection
        .execute("UPDATE messages SET state = 'queued' WHERE ordinal = 8", [])
        .unwrap();
    connection
        .execute(
            "UPDATE runs SET origin_json = '{\"type\":\"goal\"}' WHERE id = ?1",
            [runs[2].to_string()],
        )
        .unwrap();
    insert_record_steering(
        &connection,
        session,
        runs[2],
        9,
        1,
        "human steering during continuation",
    );
    for (call, resolution, text) in [
        (1, "answered", "Q1: Which path?\nA: staging/report.txt\n"),
        (2, "denied", "not an answer"),
    ] {
        insert_record_call(
            &connection,
            runs[0],
            call,
            "ask_user",
            serde_json::json!({}),
            "completed",
            text,
        );
        connection.execute("UPDATE tool_calls SET approval_resolution = ?2 WHERE run_id = ?1 AND call_ordinal = ?3",
            params![runs[0].to_string(), resolution, call]).unwrap();
    }
    let text = with_exact_inputs(
        &connection,
        &session.to_string(),
        RecordScope::Session { cutoff_ordinal: 6 },
        String::new(),
    )
    .unwrap();
    let mut last = 0;
    for input in [
        "original task",
        "Q1: Which path?\nA: staging/report.txt\n",
        "late correction to first task",
        "newer queued prompt",
        "human steering during continuation",
    ] {
        let at = text.find(input).unwrap();
        assert!(at > last, "{input}: {text}");
        last = at;
    }
    for omitted in [
        "unapplied correction",
        "generated continuation",
        "not an answer",
    ] {
        assert!(!text.contains(omitted), "{text}");
    }
}

#[test]
fn exact_inputs_reject_capacity_overflow_without_cutting_or_deleting_sources() {
    let input = format!("{} protected tail", "é".repeat(MAX_CONTEXT_BYTES / 2));
    let (_directory, connection, session, _) = record_fixture(std::slice::from_ref(&input));
    assert!(matches!(
        with_exact_inputs(
            &connection,
            &session.to_string(),
            RecordScope::Session { cutoff_ordinal: 2 },
            valid_summary("narrative")
        ),
        Err(SessionRuntimeError::ContextTooLarge)
    ));
    let stored: String = connection
        .query_row(
            "SELECT output FROM messages WHERE role = 'user'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, input);
}

#[tokio::test]
async fn oversized_legacy_inputs_fail_only_the_affected_run() {
    let inputs = vec!["original constraint ".repeat(8_000); 40];
    let (directory, connection, session, _) = record_fixture(&inputs);
    let workspace: String = connection
        .query_row(
            "SELECT workspace_id FROM sessions WHERE id = ?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workspaces SET path = ?1",
            [directory.path().to_str().unwrap()],
        )
        .unwrap();
    connection.execute("INSERT INTO session_compactions(session_id, run_id, summary, cutoff_ordinal, before_bytes, after_bytes, created_at_ms)
        VALUES (?1, ?2, ?3, 80, 1, 1, 1)", params![session.to_string(), RunId::generate().unwrap().to_string(), valid_summary("legacy narrative")]).unwrap();
    drop(connection);
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(AutoCompactLoader {
            requests: Arc::clone(&requests),
            scripts: vec![AutoCompactScript::Text("healthy".to_owned())],
            loads: StdMutex::new(0),
            context_window: None,
            max_output_tokens: 1_024,
            provider_identity: true,
        }),
    )
    .await
    .unwrap();
    let workspace_id: WorkspaceId = workspace.parse().unwrap();
    let (_, after) = resolve_workspace(&runtime, directory.path()).await;
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    for manual in [false, true] {
        let run = if manual {
            compact_session(&runtime, session).await
        } else {
            queue_prompt(&runtime, session, "continue".to_owned()).await
        };
        let observed = collect_until(&mut events, finished_for(run)).await;
        assert!(
            matches!(finished_outcome(&observed, run), Some(RunOutcome::Failed { failure })
            if failure.kind == RunFailureKind::Policy && failure.message.contains("4 MiB"))
        );
        assert!(!*runtime.inner.failed.borrow());
    }
    let healthy = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: healthy,
    } = healthy.outcome
    else {
        panic!("session");
    };
    let run = queue_prompt(&runtime, healthy, "healthy session".to_owned()).await;
    let observed = collect_until(&mut events, finished_for(run)).await;
    assert_eq!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "blocked inputs must not reach a provider"
    );
    runtime.shutdown().await.unwrap();
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let saved: u64 = connection.query_row("SELECT SUM(length(CAST(output AS BLOB))) FROM messages WHERE session_id = ?1 AND role = 'user'", [session.to_string()], |row| row.get(0)).unwrap();
    assert!(saved > MAX_CONTEXT_BYTES as u64);
    let markers: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM session_compactions WHERE session_id = ?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(markers, 1);
}
