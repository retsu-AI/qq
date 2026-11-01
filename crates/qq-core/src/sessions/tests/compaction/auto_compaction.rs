use super::*;

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

    let prompt = submit(
        &harness.runtime,
        harness.session_id,
        Prompt::input(vec![
            InputPart::text("y".repeat(MAX_PROMPT_BYTES - 64)),
            InputPart::WorkspaceFile {
                path: "a.txt".to_owned(),
                expected_hash: None,
                range: None,
            },
        ]),
    )
    .await;
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
    const WINDOW: u32 = 128 * 1024;
    for (configured, expected) in [(1_024, 1_024), (16_384, 16_384)] {
        let prior = usize::try_from(WINDOW - configured).unwrap() * 4 - 8 * 1024;
        let mut harness = auto_compact_harness_with_limits(
            vec![
                AutoCompactScript::Text("x".repeat(prior)),
                AutoCompactScript::Text(valid_summary("the summary")),
                AutoCompactScript::Text("done".to_owned()),
            ],
            Some(WINDOW),
            configured,
        )
        .await;
        let first = queue_prompt(&harness.runtime, harness.session_id, "grow".to_owned()).await;
        collect_until(&mut harness.events, finished_for(first)).await;
        let prompt =
            queue_prompt(&harness.runtime, harness.session_id, "y".repeat(16 * 1024)).await;
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
            .snapshot(SnapshotRequest::new(
                harness.workspace_id,
                Some(harness.session_id),
                1,
                8,
            ))
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

    let queued = store_command(
        harness.runtime.store(),
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
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
    assert!(!harness.runtime.has_failed());
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

    let queued = store_command(
        harness.runtime.store(),
        harness.session_id,
        "y".repeat(MAX_PROMPT_BYTES),
    )
    .await
    .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    FAULTS.inject(
        StoreFault::ReservedReload(run_id),
        [SessionRuntimeError::CONSTRAINT],
    );
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
    assert!(!harness.runtime.has_failed());
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
    assert!(!harness.runtime.has_failed());
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

    let queued = store_command(harness.runtime.store(), harness.session_id, "cancel this")
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
        .store()
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
            .store()
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
    let connection = Connection::open(harness.directory.path().join("sessions.sqlite3")).unwrap();
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

    cancel_run(&harness.runtime, prompt).await.unwrap();
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
    cancel_run(&harness.runtime, prompt).await.unwrap();
    let observed = collect_until(&mut harness.events, finished_for(prompt)).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::CancellationRequested { run_id, .. } if *run_id == compaction
    )));

    cancel_run(&harness.runtime, compaction).await.unwrap();
    let observed = collect_until(&mut harness.events, finished_for(compaction)).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id == compaction
    )));
}
