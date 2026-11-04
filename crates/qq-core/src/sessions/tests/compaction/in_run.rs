use super::*;

async fn in_run_compaction_harness(turns: usize, window: u32, summary: &str) -> AutoCompactHarness {
    auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![AutoCompactScript::ToolRepeatedlyWithSummaries {
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

    let store = harness.runtime.store().clone();
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
                AutoCompactScript::ToolRepeatedlyWithSummaries {
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
                script: Box::new(AutoCompactScript::ToolRepeatedlyWithSummaries {
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
            scripts: vec![AutoCompactScript::ToolRepeatedlyWithSummaries {
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
            scripts: vec![AutoCompactScript::ToolRepeatedlyWithSummaries {
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
    cancel_run(&harness.runtime, run).await.unwrap();
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
            .store()
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
    let store = runtime.store().clone();
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
            scripts: vec![AutoCompactScript::ToolRepeatedlyWithProviderOverflow {
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
            scripts: vec![AutoCompactScript::ToolRepeatedlyWithEarlyFault {
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
    let store = harness.runtime.store().clone();
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
            boundary_notices: Vec::new(),
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
            boundary_notices: Vec::new(),
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
            boundary_notices: Vec::new(),
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
    let turns = 48;
    let summary = valid_summary("continued");
    let cut = summary.find("Decis").unwrap() + "Decis".len();
    let script = AutoCompactScript::ToolRepeatedlyWithTruncatedSummaries {
        turns,
        text: "task complete".to_owned(),
        summary: summary.clone(),
        cut,
    };
    let mut harness = auto_compact_harness_with_loader_and_mode(
        AutoCompactLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            scripts: vec![script],
            loads: StdMutex::new(0),
            context_window: Some(16 * 1024),
            max_output_tokens: 1_024,
            provider_identity: true,
        },
        ApprovalMode::Full,
    )
    .await;
    let held = harness.runtime.hold_scheduling().await;
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("do the task")],
                limits: RunLimits::default(),
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
    let queued = store_command(&store, session_id, "queued later")
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
