use super::*;

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
