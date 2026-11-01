use super::*;

mod auto_compaction;
mod exact_inputs;
mod folding;
mod in_run;
mod owned_summaries;
mod pruning;

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
    let run_id = submit(&harness.runtime, harness.session_id, "do work").await;
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
    let (workspace_id, session_id, created) =
        new_session(&runtime, directory.path(), ApprovalMode::default()).await;
    let mut events = subscribe_after(&runtime, workspace_id, created.committed_through);
    try_submit(&runtime, session_id, "say hello").await.unwrap();
    collect_through_finished(&mut events).await;
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 32))
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
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 32))
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
    cancel_run(&harness.runtime, stalled).await.unwrap();
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
                    .map_err(load_error)
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
    let SessionFixture {
        directory,
        runtime,
        session_id,
        mut events,
        ..
    } = SessionFixture::open_at(
        directory,
        Arc::new(ForgetfulLoader {
            requests: Arc::clone(&requests),
        }),
        ApprovalMode::default(),
    )
    .await;

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
        .store()
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

fn old_format_summary(body: &str) -> String {
    format!(
        "1. Intent: {body}\n2. Decisions and constraints: {body}\n3. Work state: {body}\n\
         4. Files touched: {body}\n5. Errors: {body}\n6. User messages: {body}"
    )
}
