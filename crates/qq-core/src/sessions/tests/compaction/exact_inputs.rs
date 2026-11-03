use super::*;

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
    assert!(!harness.runtime.has_failed());
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
        "third task".to_owned(),
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
        "third task",
        "human steering during continuation",
    ] {
        let at = text.find(input).unwrap();
        assert!(at > last, "{input}: {text}");
        last = at;
    }
    for omitted in ["unapplied correction", "not an answer"] {
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
        assert!(!runtime.has_failed());
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
