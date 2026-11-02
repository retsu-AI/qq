use super::*;

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
    const NOTES: usize = 3;
    let note = format!("{}\n", "n".repeat(127)).repeat(720);
    let runs = (4 * 1024 * 1024 - 512 * 1024) / (NOTES * 26 * 1024) + 4;
    let mut scripts: Vec<AutoCompactScript> = (0..runs)
        .map(|_| AutoCompactScript::ReadNotesThenText {
            notes: NOTES,
            text: "ok".to_owned(),
        })
        .collect();
    scripts.push(AutoCompactScript::Text("still fits".to_owned()));
    let mut harness = auto_compact_harness_with_limits(scripts, None, 16_384).await;
    for index in 0..NOTES {
        std::fs::write(
            harness.workspace_path.join(format!("note-{index}.txt")),
            &note,
        )
        .unwrap();
    }
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
