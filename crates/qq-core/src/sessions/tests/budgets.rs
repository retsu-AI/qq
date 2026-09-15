use super::*;

#[tokio::test]
async fn turn_budget_grants_one_final_response_then_settles_as_budget_exhausted() {
    let mut harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: None,
        pricing: None,
        hang: false,
    })
    .await;
    let run_id = queue_limited_prompt(
        &harness,
        RunLimits {
            max_model_turns: Some(2),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::ModelTurns);
    assert!(exhaustion.final_response);
    assert!(exhaustion.message.contains("2 model turn"));

    // One working turn, then the last permitted turn is the tool-free final
    // response whose text is persisted as the run's last assistant message.
    {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].tools().is_empty());
        assert!(requests[1].tools().is_empty());
        assert!(
            requests[1]
                .system()
                .is_some_and(|system| system.contains(crate::BUDGET_FINAL_RESPONSE_NOTICE))
        );
    }
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 16,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let run = focused.runs.iter().find(|run| run.id == run_id).unwrap();
    assert_eq!(run.status, RunStatus::BudgetExhausted);
    assert_eq!(
        run.limits
            .as_deref()
            .and_then(|limits| limits.max_model_turns),
        Some(2)
    );
    assert!(focused.messages.iter().any(|message| {
        message.role == MessageRole::Assistant
            && message.output == "final status"
            && message.state == MessageState::Complete
    }));
    assert_eq!(
        focused.summary.status,
        SessionStatus::Idle,
        "a settled budget leaves no active run"
    );
}

#[tokio::test]
async fn tool_call_budget_reserves_the_final_turn_before_the_cap() {
    let mut harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: None,
        pricing: None,
        hang: false,
    })
    .await;
    // Each turn issues one call, but a turn may issue sixteen: the meter
    // must reserve room for a full turn, so three calls fit and the
    // fourth turn becomes the final response.
    let run_id = queue_limited_prompt(
        &harness,
        RunLimits {
            max_tool_calls: Some(3 + crate::MAX_TOOL_CALLS_PER_TURN as u32),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::ToolCalls);
    assert!(exhaustion.final_response);
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests.last().unwrap().tools().is_empty());
}

#[tokio::test]
async fn wall_clock_budget_settles_a_hanging_provider_without_a_final_response() {
    let mut harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: None,
        pricing: None,
        hang: true,
    })
    .await;
    let run_id = queue_limited_prompt(
        &harness,
        RunLimits {
            max_duration_ms: Some(100),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = tokio::time::timeout(
        Duration::from_secs(5),
        collect_until(&mut harness.events, finished_for(run_id)),
    )
    .await
    .expect("the deadline must settle a provider that never streams");
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::Duration);
    assert!(!exhaustion.final_response);
    assert_eq!(harness.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cost_budget_settles_on_spend_and_fails_closed_when_usage_goes_missing() {
    let mut harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: Some(usage(2, 1)), // 4_000 nanos per turn
        pricing: Some(budget_pricing()),
        hang: false,
    })
    .await;
    let run_id = queue_limited_prompt(
        &harness,
        RunLimits {
            max_cost_usd_nanos: Some(7_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::Cost);
    assert!(exhaustion.final_response);
    // Two working turns overran 7_000; the final response is the third.
    assert_eq!(harness.requests.lock().unwrap().len(), 3);
    let finished_cost = observed.iter().find_map(|event| match &event.event {
        SessionEvent::RunFinished {
            run_id: finished,
            session,
            ..
        } if *finished == run_id => Some(session.estimated_cost_usd_nanos),
        _ => None,
    });
    assert_eq!(finished_cost, Some(Some(12_000)));

    let mut unmetered = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: None,
        pricing: Some(budget_pricing()),
        hang: false,
    })
    .await;
    let run_id = queue_limited_prompt(
        &unmetered,
        RunLimits {
            max_cost_usd_nanos: Some(1_000_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until(&mut unmetered.events, finished_for(run_id)).await;
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::CostUnknown);
    assert!(!exhaustion.final_response);
    assert_eq!(
        unmetered.requests.lock().unwrap().len(),
        1,
        "unknown spend under a cost cap stops before any further provider work"
    );
}

#[tokio::test]
async fn cost_budget_without_pricing_is_rejected_before_provider_work() {
    let mut harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: Some(usage(1, 1)),
        pricing: None,
        hang: false,
    })
    .await;
    let run_id = queue_limited_prompt(
        &harness,
        RunLimits {
            max_cost_usd_nanos: Some(1_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    match finished_outcome(&observed, run_id) {
        Some(RunOutcome::Failed { failure }) => {
            assert_eq!(failure.kind, RunFailureKind::Configuration);
            assert!(failure.message.contains("no configured pricing"));
        }
        other => panic!("expected a configuration failure, got {other:?}"),
    }
    assert!(
        observed
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::RunStarted { .. })),
        "rejection happens before the run starts"
    );
    assert!(harness.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn token_budget_settles_a_completed_overrun_as_exhausted_not_completed() {
    let mut harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: Some(usage(50, 10)),
        pricing: None,
        hang: false,
    })
    .await;
    let run_id = queue_limited_prompt(
        &harness,
        RunLimits {
            max_total_tokens: Some(100),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until(&mut harness.events, finished_for(run_id)).await;
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::TotalTokens);
    assert!(exhaustion.final_response);
    assert!(exhaustion.message.contains("180 total tokens"));
}

#[tokio::test]
async fn zero_limits_are_rejected_at_submission() {
    let harness = budget_harness(BudgetLoopLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        usage: None,
        pricing: None,
        hang: false,
    })
    .await;
    let result = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("loop")],
                limits: RunLimits {
                    max_model_turns: Some(0),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await;
    assert_eq!(result, Err(SessionRuntimeError::InvalidRunLimits));
}

#[tokio::test]
async fn run_limits_survive_restart_and_bound_the_recovered_run() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let harness = budget_harness(BudgetLoopLoader {
        requests: Arc::clone(&requests),
        usage: None,
        pricing: None,
        hang: true,
    })
    .await;
    let limits = RunLimits {
        max_model_turns: Some(1),
        ..RunLimits::default()
    };
    // Queue while a hanging run occupies the session so the limited run
    // is still queued at shutdown.
    let hanging = queue_limited_prompt(&harness, RunLimits::default()).await;
    let limited = queue_limited_prompt(&harness, limits).await;
    let mut events = harness.events;
    collect_until(
        &mut events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == hanging),
    )
    .await;
    // An unclean stop (no shutdown) leaves the hanging run interrupted and
    // the limited run queued; recovery must enforce its persisted limits.
    // The hanging task still holds the runtime, so stop the store worker
    // directly: that is what process death looks like to the store, and
    // it releases store ownership for the successor.
    drop(events);
    harness.runtime.abandon_for_test().await.unwrap();
    drop(harness.runtime);
    let connection = Connection::open(&harness.database_path).unwrap();
    let stored: Option<String> = connection
        .query_row(
            "SELECT limits_json FROM runs WHERE id = ?1",
            [limited.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<RunLimits>(stored.as_deref().unwrap()).unwrap(),
        limits
    );
    let after = EventCursor {
        store_id: connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'store_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap(),
        workspace_id: harness.workspace_id,
        sequence: 0,
    };
    drop(connection);

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(harness.database_path.clone()),
        Arc::new(BudgetLoopLoader {
            requests: Arc::clone(&requests),
            usage: None,
            pricing: None,
            hang: false,
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after,
        })
        .unwrap();
    let observed = collect_until(&mut events, finished_for(limited)).await;
    let exhaustion = exhaustion_of(&observed, limited);
    assert_eq!(exhaustion.limit, BudgetLimitKind::ModelTurns);
    assert_eq!(
        exhaustion.message,
        "the run exhausted its 1 model turn budget"
    );
    runtime.close().await.unwrap();
}
