use super::*;

#[tokio::test(start_paused = true)]
async fn loading_exhaustion_without_work_blocks_after_three_attempts() {
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: None,
            })]))
        }
    }
    struct Loader {
        loads: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let attempt = self.loads.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                if attempt >= 3 {
                    return Err(RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: "bounded red-fixture stop".to_owned(),
                    });
                }
                tokio::time::advance(Duration::from_millis(200)).await;
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    let keep_clock = tokio::spawn(async {
        let began = std::time::Instant::now();
        while began.elapsed() < Duration::from_secs(5) {
            tokio::task::yield_now().await;
        }
    });
    let directory = TempDir::new().unwrap();
    let loads = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(Loader {
            loads: Arc::clone(&loads),
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
        "type":"set_goal","session_id":session_id,"objective":"do not loop on unproductive loading",
        "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000,"per_run":{"max_duration_ms":100}}
    })).unwrap()).await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            if let SessionEvent::GoalUpdated { goal } = event.unwrap().event
                && goal.status != qq_protocol::GoalStatus::Active
            {
                return goal;
            }
        }
        panic!("goal stream ended")
    })
    .await
    .unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    keep_clock.abort();
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(stopped.counters.runs, 0);
    assert_eq!(stopped.counters.usage, Some(TokenUsage::default()));
    assert_eq!(stopped.counters.estimated_cost_usd_nanos, Some(0));
    assert_eq!(
        stopped.status,
        qq_protocol::GoalStatus::Blocked {
            reason: qq_protocol::GoalBlockedReason::RepeatedFailure
        },
        "unproductive budget exhaustion must not restart forever"
    );
    assert_eq!(stopped.counters.failure_streak, 3);
    assert_eq!(loads.load(Ordering::SeqCst), 3);
    let runs = &snapshot.focused.unwrap().runs;
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|run|matches!(&run.outcome,Some(RunOutcome::BudgetExhausted {exhaustion}) if exhaustion.limit==qq_protocol::BudgetLimitKind::Duration)));
}

#[tokio::test]
async fn no_work_exhaustion_streak_reopens_resets_and_respects_goal_deadline() {
    for deadline_wins in [false, true] {
        let (directory, mut store, initial) = super::goals::model_goal_claim(true).await;
        let session = initial.identity.session_id;
        let mut next = Some(initial);
        for (index, expected_streak) in [1, 2, 0, 1, 2, 3].into_iter().enumerate() {
            let claim = if let Some(claim) = next.take() {
                claim
            } else {
                store.queue_due_goals(now_ms()).await.unwrap();
                let claim = store.reserve_next_run(false).await.unwrap().unwrap();
                store
                    .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                claim
            };
            let accounting = if index == 2 {
                let usage = TokenUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    ..TokenUsage::default()
                };
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage: Some(usage),
                    estimated_cost_usd_nanos: Some(37),
                    saw_turn: true,
                    context_tokens: None,
                    final_output: None,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                };
                store
                    .persist_model_turn(
                        &claim,
                        ModelTurnCommit {
                            model_work: crate::runtime::ModelWorkDisposition::Committed,
                            turn_ordinal: 1,
                            message: Message::assistant(
                                "committed work before duration exhaustion",
                            ),
                            calls: Vec::new(),
                            turn_message: None,
                            context_tokens: None,
                            occupancy_basis: None,
                            usage: Some(usage),
                            estimated_cost_usd_nanos: Some(37),
                            accounting: Some(accounting.clone()),
                            truncated: false,
                            notice: None,
                            goal_notices: Vec::new(),
                            reserved_goal_notices: 0,
                        },
                    )
                    .await
                    .unwrap();
                Some(accounting)
            } else {
                None
            };
            if deadline_wins && index == 5 {
                store
                    .call(Priority::Control, move |connection| {
                        let mut goal =
                            crate::sessions::goals::load_live(connection, session)?.unwrap();
                        goal.budget.deadline_ms = now_ms().saturating_sub(1);
                        crate::sessions::goals::save(connection, &goal)
                    })
                    .await
                    .unwrap();
            }
            let now = tokio::time::Instant::now();
            let budget = crate::runtime::BudgetMeter::new(
                RunLimits {
                    max_duration_ms: Some(1),
                    ..RunLimits::default()
                },
                None,
                now,
            );
            let outcome = RunOutcome::BudgetExhausted {
                exhaustion: Box::new(budget.exhaustion(
                    qq_protocol::BudgetLimitKind::Duration,
                    false,
                    now + Duration::from_millis(2),
                )),
            };
            store
                .finish_run(
                    &claim,
                    outcome.clone(),
                    accounting.clone(),
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            assert!(
                store
                    .finish_run(&claim, outcome, accounting, TeardownComplete::nothing_ran())
                    .await
                    .unwrap()
                    .is_empty()
            );
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(
                goal.counters.failure_streak,
                if deadline_wins && index == 5 {
                    2
                } else {
                    expected_streak
                }
            );
            assert_eq!(goal.counters.runs, u32::from(index >= 2));
            if index == 5 {
                assert_eq!(
                    goal.status,
                    if deadline_wins {
                        qq_protocol::GoalStatus::BudgetExhausted {
                            reason: qq_protocol::GoalExhaustion::Deadline,
                        }
                    } else {
                        qq_protocol::GoalStatus::Blocked {
                            reason: qq_protocol::GoalBlockedReason::RepeatedFailure,
                        }
                    }
                );
            } else {
                assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
            }
            if index == 1 {
                store.close().await.unwrap();
                store = Store::open(directory.path().join("sessions.sqlite3"))
                    .await
                    .unwrap();
            }
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_pending_main_request_preserves_unknown_session_and_goal_spend() {
    struct Model(Arc<tokio::sync::Notify>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.notify_one();
            Box::pin(stream::pending())
        }
    }
    struct Loader(Arc<tokio::sync::Notify>);
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let entered = Arc::clone(&self.0);
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model(entered), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for user_prompt in [false, true] {
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader(Arc::clone(&entered))),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let set=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"pending request certainty",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = set.outcome else {
            panic!("goal")
        };
        runtime.inner.store.drive_goals(now_ms()).await.unwrap();
        let run = if user_prompt {
            queue_prompt(&runtime, session_id, "explicit work".to_owned()).await
        } else {
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session_id)?
                        .unwrap()
                        .pending_run_id
                        .unwrap())
                })
                .await
                .unwrap()
        };
        drop(held);
        runtime.inner.schedule.send(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        runtime.inner.store.call(Priority::Control,move |connection| {
            let (pending,origin,turns):(bool,String,u64)=connection.query_row("SELECT goal_request_pending,origin_json,(SELECT COUNT(*) FROM model_turns WHERE run_id=runs.id) FROM runs WHERE id=?1",[run.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            assert!(pending);
            assert_eq!(turns,0);
            assert!(if user_prompt {matches!(serde_json::from_str::<qq_protocol::RunOrigin>(&origin).unwrap(),qq_protocol::RunOrigin::UserPrompt {})} else {matches!(serde_json::from_str::<qq_protocol::RunOrigin>(&origin).unwrap(),qq_protocol::RunOrigin::Goal {..})});
            Ok(())
        }).await.unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::Pause,
                },
            )
            .await
            .unwrap();
        let finished = collect_until_run_finished(&mut events, run).await;
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();
        assert!(matches!(
            finished_outcome(&finished, run),
            Some(RunOutcome::Cancelled)
        ));
        let focused = snapshot.focused.unwrap();
        let current = focused.goal.unwrap();
        assert_eq!(current.counters.runs, 0);
        assert_eq!(current.counters.usage, None);
        assert_eq!(current.counters.estimated_cost_usd_nanos, None);
        let accounting = focused.summary.accounting.unwrap();
        let unknown = qq_protocol::AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        };
        assert_eq!(
            accounting.direct, unknown,
            "user_prompt={user_prompt}: pending dispatch cannot retain the zero rejection seed"
        );
        assert_eq!(accounting.inclusive, unknown);
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let (usage,cost,known):(Option<String>,Option<u64>,bool)=connection.query_row("SELECT r.usage_json,r.estimated_cost_usd_nanos,s.cost_known FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1",[run.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!((usage, cost, known), (None, None, false));
    }
}

#[tokio::test]
async fn skipped_routing_crash_preserves_known_zero_and_paid_pending_stays_unknown() {
    for paid in [false, true] {
        let (directory, store, initial) = super::goals::model_goal_claim(false).await;
        let session = initial.identity.session_id;
        let workspace = initial.identity.workspace_id;
        store
            .finish_run(
                &initial,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        // These prepared-only fixture rows never dispatched a provider. Seed
        // their known-zero baseline so legacy NULL history cannot mask this run.
        store.call(Priority::Control,move |connection| {
            connection.execute("UPDATE runs SET usage_json=?2,estimated_cost_usd_nanos=0 WHERE session_id=?1 AND outcome_json IS NOT NULL",params![session.to_string(),serde_json::to_string(&TokenUsage::default())?])?;
            Ok(())
        }).await.unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![InputPart::text("x".repeat(17 * 1024))],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        let run = claim.identity.run_id;
        let baseline = store
            .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
            .await
            .unwrap()
            .focused
            .unwrap()
            .summary
            .accounting
            .unwrap();
        let zero = qq_protocol::AccountingTotal {
            usage: Some(TokenUsage::default()),
            estimated_cost_usd_nanos: Some(0),
        };
        assert_eq!(baseline.direct, zero);
        assert_eq!(baseline.inclusive, zero);
        store.call(Priority::Control,|connection| {
            connection.execute_batch("CREATE TEMP TRIGGER refuse_routing_start BEFORE INSERT ON events WHEN json_extract(NEW.envelope_json,'$.event.type')='routing_started' BEGIN SELECT RAISE(ABORT,'routing event failure'); END;")?;
            Ok(())
        }).await.unwrap();
        assert!(
            store
                .record_routing_started(&claim, paid, None)
                .await
                .is_err()
        );
        let captured_goal = serde_json::to_string(claim.goal.as_deref().unwrap()).unwrap();
        store.call(Priority::Control,move |connection| {
            let (routing,usage,cost,goal,admitted):(Option<String>,Option<String>,Option<u64>,Option<String>,bool)=connection.query_row("SELECT routing_json,usage_json,estimated_cost_usd_nanos,goal_snapshot_json,goal_helper_admitted FROM runs WHERE id=?1",[run.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
            assert_eq!((routing,usage,cost,goal,admitted),(None,None,None,Some(captured_goal),false));
            connection.execute_batch("DROP TRIGGER refuse_routing_start;")?;
            Ok(())
        }).await.unwrap();
        assert!(
            matches!(store.record_routing_started(&claim,paid,None).await.unwrap().unwrap().unwrap().event,SessionEvent::RoutingStarted {run_id} if run_id==run)
        );
        store.close().await.unwrap();
        let reopened = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        assert!(
            !reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        let snapshot = reopened
            .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        let accounting = focused.summary.accounting.unwrap();
        let expected = if paid {
            qq_protocol::AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            }
        } else {
            zero
        };
        let goal = focused.goal.unwrap();
        assert_eq!(goal.counters.runs, 0);
        assert_eq!(goal.counters.usage, expected.usage);
        assert_eq!(
            goal.counters.estimated_cost_usd_nanos,
            expected.estimated_cost_usd_nanos
        );
        assert!(
            reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        reopened.close().await.unwrap();
        assert_eq!(
            accounting.direct, expected,
            "paid={paid}: a skipped route has no crash uncertainty"
        );
        assert_eq!(accounting.inclusive, expected);
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let (usage,cost,admitted):(Option<String>,Option<u64>,bool)=connection.query_row("SELECT usage_json,estimated_cost_usd_nanos,goal_helper_admitted FROM runs WHERE id=?1",[run.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(
            usage.map(|value| serde_json::from_str::<TokenUsage>(&value).unwrap()),
            expected.usage
        );
        assert_eq!(cost, expected.estimated_cost_usd_nanos);
        assert_eq!(admitted, paid);
    }
}

#[tokio::test]
async fn legacy_pending_model_work_restores_chargeable_proof_before_recovery() {
    for case in [
        "measured",
        "zero",
        "ambiguous",
        "terminal",
        "replaced",
        "already53",
        "replay",
        "content",
        "truncated",
    ] {
        let (directory, store, claim) = super::goals::model_goal_claim(false).await;
        let session = claim.identity.session_id;
        let run = claim.identity.run_id;
        let goal_id = claim.goal.as_ref().unwrap().goal_id;
        let revision = claim.goal.as_ref().unwrap().revision;
        let usage = match case {
            "zero" => Some(TokenUsage::default()),
            "ambiguous" | "content" | "truncated" | "replay" => None,
            _ => Some(TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            }),
        };
        let cost = usage.map(|_| if case == "zero" { 0 } else { 37 });
        let basis = ContextOccupancyBasis {
            version: 1,
            shape: ContentHash::from_bytes([0; 32]),
            static_prefix: PreparedStaticPrefix::new(ContentHash::from_bytes([0; 32]), None),
            request_bytes: 0,
        };
        let accounting = RunAccounting {
            goal_request_rejected: false,
            usage,
            estimated_cost_usd_nanos: cost,
            saw_turn: true,
            context_tokens: None,
            final_output: None,
            request_basis: basis,
        };
        store
            .persist_model_turn(
                &claim,
                ModelTurnCommit {
                    model_work: if case == "ambiguous" {
                        crate::runtime::ModelWorkDisposition::Uncertain
                    } else {
                        crate::runtime::ModelWorkDisposition::Committed
                    },
                    turn_ordinal: 1,
                    message: Message::new(
                        Role::Assistant,
                        if matches!(case, "zero" | "ambiguous" | "truncated" | "replay") {
                            Vec::new()
                        } else {
                            vec![ContentBlock::Text {
                                text: "durable legacy work".to_owned(),
                            }]
                        },
                    ),
                    calls: Vec::new(),
                    turn_message: None,
                    context_tokens: None,
                    occupancy_basis: None,
                    usage,
                    estimated_cost_usd_nanos: cost,
                    accounting: Some(accounting.clone()),
                    truncated: case == "truncated",
                    notice: None,
                    goal_notices: Vec::new(),
                    reserved_goal_notices: 0,
                },
            )
            .await
            .unwrap();
        if case == "terminal" {
            store
                .finish_run(
                    &claim,
                    RunOutcome::Completed,
                    Some(accounting),
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
        }
        if case == "replaced" {
            store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session,"objective":"replacement","replace":true,
                "expected":{"goal_id":goal_id,"revision":revision},"budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000}
            })).unwrap()).await.unwrap();
        }
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        if case == "replay" {
            connection
                .execute(
                    "UPDATE model_turns SET assistant_content_json=?1",
                    [r#"{"content":[],"replay":"durable provider replay"}"#],
                )
                .unwrap();
        }
        if case == "already53" {
            connection.execute_batch("UPDATE model_turns SET work_disposition=NULL; UPDATE metadata SET value='53' WHERE key='schema_version';").unwrap();
        } else {
            connection
                .execute_batch(
                    "ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id;
            ALTER TABLE runs DROP COLUMN goal_request_pending;
            ALTER TABLE runs DROP COLUMN goal_helper_admitted;
            ALTER TABLE model_turns DROP COLUMN work_disposition;
            UPDATE metadata SET value='51' WHERE key='schema_version';",
                )
                .unwrap();
        }
        drop(connection);
        let reopened = Store::open(path).await.unwrap();
        reopened.recover_interrupted_runs().await.unwrap();
        let (historical, live, totals, proof): (
            qq_protocol::GoalSnapshot,
            qq_protocol::GoalSnapshot,
            (u64, bool),
            Option<String>,
        ) = reopened
            .call(Priority::Control, move |connection| {
                let payload: String = connection.query_row(
                    "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                    [goal_id.to_string()],
                    |row| row.get(0),
                )?;
                let totals = connection.query_row(
                    "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                    [session.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let proof = connection.query_row(
                    "SELECT work_disposition FROM model_turns WHERE run_id=?1 AND turn_ordinal=1",
                    [run.to_string()],
                    |row| row.get(0),
                )?;
                Ok((
                    serde_json::from_str(&payload)?,
                    crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    totals,
                    proof,
                ))
            })
            .await
            .unwrap();
        assert!(
            reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        reopened.close().await.unwrap();
        assert_eq!(
            historical.counters.runs,
            u32::from(case != "ambiguous"),
            "case={case}: unfinished legacy work must retain its chargeable evidence"
        );
        assert_eq!(historical.counters.usage, usage);
        assert_eq!(historical.counters.estimated_cost_usd_nanos, cost);
        assert_eq!(totals, (cost.unwrap_or(0), cost.is_some()));
        assert_eq!(
            proof,
            if case == "terminal" {
                None
            } else {
                Some(
                    if case == "ambiguous" {
                        "uncertain"
                    } else {
                        "committed"
                    }
                    .to_owned(),
                )
            }
        );
        if case == "replaced" {
            assert_ne!(live.goal_id, goal_id);
            assert_eq!(live.counters, qq_protocol::GoalCounters::default());
        }
    }
}

#[tokio::test]
async fn legacy_model_work_repair_rolls_back_on_invalid_turn() {
    let (directory, store, claim) = super::goals::model_goal_claim(false).await;
    let run = claim.identity.run_id.to_string();
    store.close().await.unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    for (ordinal, content) in [(1, r#"[{"type":"text","text":"durable"}]"#), (2, "invalid")] {
        connection.execute("INSERT INTO model_turns(run_id,turn_ordinal,assistant_content_json) VALUES(?1,?2,?3)", rusqlite::params![run,ordinal,content]).unwrap();
    }
    connection
        .execute(
            "UPDATE metadata SET value='53' WHERE key='schema_version'",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(Store::open(path.clone()).await.is_err());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let (version, repaired): (String,u32) = connection.query_row("SELECT (SELECT value FROM metadata WHERE key='schema_version'),(SELECT COUNT(*) FROM model_turns WHERE work_disposition IS NOT NULL)", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(version, "53");
    assert_eq!(repaired, 0);
    connection
        .execute(
            "UPDATE model_turns SET assistant_content_json='[]' WHERE turn_ordinal=2",
            [],
        )
        .unwrap();
    drop(connection);
    let reopened = Store::open(path).await.unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn direct_goal_rejections_with_finite_caps_keep_retry_pause_and_known_zero() {
    for goal_origin in [false, true] {
        for (tokens, cost) in [
            (Some(10_000_u64), None),
            (None, Some(1_000_000_000_u64)),
            (Some(10_000), Some(1_000_000_000)),
        ] {
            let attempts = Arc::new(AtomicUsize::new(0));
            struct Outage(Arc<AtomicUsize>);
            impl Provider for Outage {
                fn stream(&self, _: ModelRequest) -> ProviderStream {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Box::pin(stream::iter([Err(qq_provider::ProviderError::Api {
                        status: 529,
                        message: "scripted unavailable before model output".to_owned(),
                    })]))
                }
            }
            struct Loader(Arc<AtomicUsize>);
            impl RuntimeLoader for Loader {
                fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                    let attempts = Arc::clone(&self.0);
                    Box::pin(async move {
                        let runtime = Runtime::new(Outage(attempts), "test-model", 256).unwrap();
                        Ok(loaded_runtime(
                            runtime,
                            &request.workspace,
                            Some(budget_pricing()),
                        ))
                    })
                }
            }
            let directory = TempDir::new().unwrap();
            let runtime = SessionRuntime::open(
                SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
                Arc::new(Loader(Arc::clone(&attempts))),
            )
            .await
            .unwrap();
            let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
            let created =
                create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::ReadOnly)
                    .await;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("session")
            };
            let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session_id,"objective":"survive provider outage",
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+86_400_000,"max_total_tokens":tokens,"max_cost_usd_nanos":cost}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
            let mut events = runtime
                .subscribe(SubscribeRequest {
                    workspace_id,
                    after: receipt.committed_through,
                })
                .unwrap();
            let (run_id, events) = if goal_origin {
                let events = collect_until(&mut events, |event| {
                    matches!(event, SessionEvent::RunFinished { .. })
                })
                .await;
                let run = events
                    .iter()
                    .find_map(|event| match event.event {
                        SessionEvent::RunFinished { run_id, .. } => Some(run_id),
                        _ => None,
                    })
                    .unwrap();
                (run, events)
            } else {
                let receipt = runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::SubmitPrompt {
                            session_id,
                            input: vec![InputPart::text("work")],
                            limits: RunLimits::default(),
                            correlation: Correlation::default(),
                            output: None,
                        },
                    )
                    .await
                    .unwrap();
                let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
                    panic!("prompt")
                };
                let events = collect_until_run_finished(&mut events, run_id).await;
                (run_id, events)
            };
            assert!(
                matches!(
                    finished_outcome(&events, run_id),
                    Some(RunOutcome::Paused { pause }) if matches!(pause.as_ref(),qq_protocol::RunPause::ProviderRetry{..})
                ),
                "known outage pauses the run"
            );
            let snapshot = runtime
                .snapshot(SnapshotRequest::new(
                    workspace_id,
                    Some(session_id),
                    256,
                    256,
                ))
                .await
                .unwrap();
            let focused = snapshot.focused.unwrap();
            let total = qq_protocol::AccountingTotal {
                usage: Some(TokenUsage::default()),
                estimated_cost_usd_nanos: Some(0),
            };
            let accounting = focused.summary.accounting.unwrap();
            runtime.shutdown().await.unwrap();
            assert!(
                attempts.load(Ordering::SeqCst) > 1,
                "finite caps must preserve bounded provider retries"
            );
            let connection =
                rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
            let ownership: (bool, Option<String>) = connection
                .query_row(
                    "SELECT goal_id IS NOT NULL,goal_paid_owner_run_id FROM runs WHERE id=?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                ownership,
                (true, None),
                "exercise directly captured goals without inherited spending ownership"
            );
            assert_eq!(
                accounting.direct, total,
                "typed direct rejection preserves public known zero"
            );
            assert_eq!(accounting.inclusive, total);
            let goal = focused.goal.unwrap();
            assert_eq!(
                goal.counters.runs, 0,
                "HTTP rejection before model output cannot consume the goal run allowance"
            );
            assert_eq!(goal.counters.usage, Some(TokenUsage::default()));
            assert_eq!(goal.counters.estimated_cost_usd_nanos, Some(0));
        }
    }
}

#[tokio::test]
async fn goal_failure_after_start_before_provider_poll_preserves_known_zero() {
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(stream::pending())
        }
    }
    struct Loader(Arc<AtomicUsize>);
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let polls = Arc::clone(&self.0);
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model(polls), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for (user_prompt, activity_failure) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let directory = TempDir::new().unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader(Arc::clone(&polls))),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal", "session_id":session_id,"objective":"known zero before dispatch",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        runtime.inner.store.drive_goals(now_ms()).await.unwrap();
        let run_id = if user_prompt {
            queue_prompt(&runtime, session_id, "explicit work".to_owned()).await
        } else {
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session_id)?
                        .unwrap()
                        .pending_run_id
                        .unwrap())
                })
                .await
                .unwrap()
        };
        let (_initial_read, release_initial) = store::hold_cancellation_read(run_id);
        let (post_start_read, release_post_start) = store::hold_cancellation_read(run_id);
        release_initial.send(()).unwrap();
        drop(held);
        runtime.inner.schedule.send(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), post_start_read)
            .await
            .unwrap()
            .unwrap();
        if activity_failure {
            runtime.inner.store.call(Priority::Control, |connection| {
                connection.execute_batch("CREATE TEMP TRIGGER refuse_waiting_activity BEFORE INSERT ON events WHEN json_extract(NEW.envelope_json,'$.event.type')='run_activity_changed' AND json_extract(NEW.envelope_json,'$.event.activity')='waiting_for_provider' BEGIN SELECT RAISE(ABORT,'activity event failure'); END;")?;
                Ok(())
            }).await.unwrap();
        } else {
            store::fail_cancellation_reads(run_id, [SessionRuntimeError::CONSTRAINT]);
        }
        release_post_start.send(()).unwrap();
        let observed = collect_until_run_finished(&mut events, run_id).await;
        let outcome = finished_outcome(&observed, run_id).unwrap();
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        runtime.shutdown().await.unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert!(matches!(outcome, RunOutcome::Failed { .. }), "{outcome:?}");
        let zero = qq_protocol::AccountingTotal {
            usage: Some(TokenUsage::default()),
            estimated_cost_usd_nanos: Some(0),
        };
        let accounting = snapshot.summary.accounting.unwrap();
        assert_eq!(accounting.direct, zero, "user_prompt={user_prompt}");
        assert_eq!(accounting.inclusive, zero);
        let goal = snapshot.goal.unwrap();
        assert_eq!(goal.counters.runs, 0);
        assert_eq!(goal.counters.usage, Some(TokenUsage::default()));
        assert_eq!(goal.counters.estimated_cost_usd_nanos, Some(0));
    }
}

#[tokio::test]
async fn replayed_goal_commands_keep_the_scheduler_wakeup() {
    let (_directory, store, previous) = claimed_store_fixture().await;
    let session_id = previous.identity.session_id;
    store
        .finish_run(
            &previous,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let mut goal: Option<qq_protocol::GoalSnapshot> = None;
    let mut observed = Vec::new();
    for action in ["set", "pause", "resume"] {
        let command = if action == "set" {
            serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"wake after a lost caller",
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
            }))
            .unwrap()
        } else {
            let current = goal.as_ref().unwrap();
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: current.goal_id,
                    revision: current.revision,
                },
                action: if action == "pause" {
                    qq_protocol::GoalControl::Pause
                } else {
                    qq_protocol::GoalControl::Resume
                },
            }
        };
        let id = CommandId::generate().unwrap();
        let committed = store.command(id, command.clone()).await.unwrap();
        let replayed = store.command(id, command).await.unwrap();
        observed.push((
            action,
            committed.schedule,
            replayed.schedule,
            replayed.replayed,
        ));
        assert_eq!(
            serde_json::to_value(&committed.receipt).unwrap(),
            serde_json::to_value(&replayed.receipt).unwrap()
        );
        let CommandOutcome::GoalUpdated { goal: current, .. } = committed.receipt.outcome else {
            panic!("goal")
        };
        goal = Some(*current);
    }
    store.close().await.unwrap();
    for (action, original_wake, replay_wake, replayed) in observed {
        assert!(original_wake && replayed);
        assert!(
            replay_wake,
            "replayed {action} must preserve the scheduler wake after a lost response"
        );
    }
}

#[tokio::test]
async fn provider_replay_and_empty_reasoning_are_goal_work_before_api_fault() {
    struct Model(u8);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let mut events = Vec::new();
            if self.0 == 1 {
                events.push(Ok(qq_provider::ProviderEvent::Replay {
                    data: Arc::from("opaque-thinking-proof"),
                }));
            } else if self.0 >= 2 {
                let kind = qq_protocol::ReasoningKind::ExposedThinking;
                events.push(Ok(qq_provider::ProviderEvent::ReasoningStarted { kind }));
                if self.0 == 4 {
                    events.push(Ok(qq_provider::ProviderEvent::ReasoningDelta {
                        kind,
                        text: String::new(),
                    }));
                }
                if self.0 >= 3 {
                    events.push(Ok(qq_provider::ProviderEvent::ReasoningCompleted { kind }));
                }
            }
            events.push(Err(qq_provider::ProviderError::Api {
                status: 529,
                message: "fault after scripted provider evidence".to_owned(),
            }));
            Box::pin(stream::iter(events))
        }
    }
    struct Loader(u8);
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let marker = self.0;
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model(marker), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for marker in 0..=4 {
        let directory = TempDir::new().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader(marker)),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::ReadOnly).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"retain provider work evidence",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .unwrap();
        let finished = collect_until(&mut events, |event| {
            matches!(event, SessionEvent::RunFinished { .. })
        })
        .await;
        let run = finished
            .iter()
            .find_map(|event| match event.event {
                SessionEvent::RunFinished { run_id, .. } => Some(run_id),
                _ => None,
            })
            .unwrap();
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        runtime.shutdown().await.unwrap();
        let goal = snapshot.goal.unwrap();
        let accounting = snapshot.summary.accounting.unwrap();
        let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let dispositions: Vec<String> = connection
            .prepare("SELECT work_disposition FROM model_turns WHERE run_id=?1")
            .unwrap()
            .query_map([run.to_string()], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!dispositions.is_empty());
        if marker == 0 {
            assert_eq!(goal.counters.runs, 0);
            assert_eq!(accounting.direct.usage, Some(TokenUsage::default()));
            assert!(
                dispositions
                    .iter()
                    .all(|value| value == "rejected_before_work")
            );
        } else {
            assert_eq!(
                goal.counters.runs, 1,
                "provider marker={marker} proves work began"
            );
            assert_eq!(goal.counters.usage, None);
            assert_eq!(goal.counters.estimated_cost_usd_nanos, None);
            assert_eq!(accounting.direct.usage, None);
            assert_eq!(accounting.inclusive.estimated_cost_usd_nanos, None);
            assert!(
                dispositions.iter().all(|value| value == "committed"),
                "{dispositions:?}"
            );
        }
    }
}

#[tokio::test]
async fn explicit_goal_run_cancel_pauses_once_and_keeps_preparation_until_drain() {
    for phase in ["queued", "preparing", "running"] {
        let (_directory, store, previous) = super::goals::model_goal_claim(true).await;
        let session = previous.identity.session_id;
        let (run, claim) = if phase == "running" {
            (previous.identity.run_id, Some(previous))
        } else {
            store
                .finish_run(
                    &previous,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            store.queue_due_goals(now_ms()).await.unwrap();
            let run = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?
                        .unwrap()
                        .pending_run_id
                        .unwrap())
                })
                .await
                .unwrap();
            let claim = if phase == "preparing" {
                Some(store.reserve_next_run(false).await.unwrap().unwrap())
            } else {
                None
            };
            (run, claim)
        };
        let before = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let id = CommandId::generate().unwrap();
        let cancelled = store
            .command(id, SessionCommand::CancelRun { run_id: run })
            .await
            .unwrap();
        let paused = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        if phase == "preparing" {
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CancelRun { run_id: run },
                )
                .await
                .unwrap();
        }
        let (cancel_requested,preparing):(bool,Option<String>)=store.call(Priority::Control,move|connection|Ok(connection.query_row(
            "SELECT r.cancel_requested,s.preparing_run_id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1",[run.to_string()],|row|Ok((row.get(0)?,row.get(1)?)))?)).await.unwrap();
        let early = if claim.is_some() {
            Some(
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::GoalControl {
                            session_id: session,
                            expected: qq_protocol::GoalIdentity {
                                goal_id: paused.goal_id,
                                revision: paused.revision,
                            },
                            action: qq_protocol::GoalControl::Resume,
                        },
                    )
                    .await,
            )
        } else {
            None
        };
        if let Some(claim) = claim {
            if phase == "preparing" {
                store
                    .finish_reserved_run(&claim, RunOutcome::Cancelled)
                    .await
                    .unwrap();
            } else {
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Cancelled,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
        }
        store.queue_due_goals(now_ms()).await.unwrap();
        let drained = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let resumed = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: drained.goal_id,
                        revision: drained.revision,
                    },
                    action: qq_protocol::GoalControl::Resume,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated {
            goal: resumed_goal, ..
        } = resumed.receipt.outcome
        else {
            panic!("goal")
        };
        let replayed = store
            .command(id, SessionCommand::CancelRun { run_id: run })
            .await
            .unwrap();
        let after = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        store.close().await.unwrap();
        assert_eq!(
            paused.status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::User
            },
            "{phase}"
        );
        assert_eq!(paused.revision, before.revision + 1);
        assert!(cancel_requested, "{phase}");
        if phase == "preparing" {
            assert_eq!(
                preparing,
                Some(run.to_string()),
                "preparation must drain before ownership is released"
            );
        }
        if let Some(early) = early {
            assert!(
                matches!(early, Err(SessionRuntimeError::GoalCancellationPending)),
                "{phase}"
            );
        }
        assert!(
            drained.pending_run_id.is_none(),
            "explicit cancel must not schedule replacement work"
        );
        assert_eq!(
            serde_json::to_value(&cancelled.receipt).unwrap(),
            serde_json::to_value(&replayed.receipt).unwrap()
        );
        assert_eq!(
            after, *resumed_goal,
            "replaying old cancellation must not pause a resumed revision"
        );
    }
}

#[tokio::test]
async fn goal_cancel_preserves_runtime_settlement_and_pauses_new_progress_revision() {
    for client in [false, true] {
        let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
        let session = claim.identity.session_id;
        let captured = claim.goal.as_ref().unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetGoal {
                    session_id: session,
                    objective: "revised objective".to_owned(),
                    check: None,
                    budget: Box::new(captured.budget.clone()),
                    expected: Some(qq_protocol::GoalIdentity {
                        goal_id: captured.goal_id,
                        revision: captured.revision,
                    }),
                    replace: false,
                },
            )
            .await
            .unwrap();
        let before = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert!(before.revision > claim.goal.as_ref().unwrap().revision);
        let command = CommandId::generate().unwrap();
        if client {
            store
                .command(
                    command,
                    SessionCommand::CancelRun {
                        run_id: claim.identity.run_id,
                    },
                )
                .await
                .unwrap();
        } else {
            store
                .settlement_cancel(command, claim.identity.run_id)
                .await
                .unwrap();
        }
        store
            .finish_run(
                &claim,
                RunOutcome::Cancelled,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let after = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        store.close().await.unwrap();
        if client {
            assert_eq!(
                after.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::User
                }
            );
            assert_eq!(after.revision, before.revision + 1);
        } else {
            assert_eq!(after.status, qq_protocol::GoalStatus::Active);
            assert_eq!(after.revision, before.revision);
        }
    }
}

#[tokio::test]
async fn goal_cancel_event_failure_rolls_back_pause_and_cancellation() {
    let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
    let session = claim.identity.session_id;
    let run = claim.identity.run_id;
    let workspace = claim.identity.workspace_id;
    let before = store
        .call(Priority::Control, move |connection| {
            let goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            let cursor = workspace_sequence(connection, workspace)?;
            connection.execute_batch(
                "CREATE TRIGGER refuse_cancel_goal_event BEFORE INSERT ON events
            WHEN json_extract(NEW.envelope_json, '$.event.type') = 'goal_updated'
            BEGIN SELECT RAISE(ABORT, 'cancel goal event refused'); END;",
            )?;
            Ok((goal, cursor))
        })
        .await
        .unwrap();
    let command = CommandId::generate().unwrap();
    let failed = store
        .command(command, SessionCommand::CancelRun { run_id: run })
        .await;
    let after = store
        .call(Priority::Control, move |connection| {
            let goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            let cursor = workspace_sequence(connection, workspace)?;
            connection.execute_batch("DROP TRIGGER refuse_cancel_goal_event")?;
            Ok((goal, cursor))
        })
        .await
        .unwrap();
    let cancelled = store.cancellation_requested(run).await.unwrap();
    let retry = store
        .command(command, SessionCommand::CancelRun { run_id: run })
        .await;
    store.close().await.unwrap();
    assert!(failed.is_err());
    assert_eq!(
        before, after,
        "goal mutation and event cursor must roll back together"
    );
    assert!(!cancelled);
    assert!(
        retry.is_ok(),
        "failed transaction must not journal the command"
    );
}

#[tokio::test]
async fn replayed_goal_cancel_signals_paid_child_of_an_earlier_root() {
    let (_directory, store, parent) = super::goals::model_goal_claim(true).await;
    let session = parent.identity.session_id;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "detached paid work".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let child_claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(child_claim.identity.run_id, child.run_id);
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    // Preserve cancellation/replay coverage for a queued goal from before the
    // sponsored-work fence. New schedulers no longer create this overlap.
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET goal_paid_owner_run_id=NULL WHERE id=?1",
                [child.run_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store.queue_due_goals(now_ms()).await.unwrap();
    let next = store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET goal_paid_owner_run_id=?2 WHERE id=?1",
                params![child.run_id.to_string(), parent.identity.run_id.to_string()],
            )?;
            Ok(crate::sessions::goals::load_live(connection, session)?
                .unwrap()
                .pending_run_id
                .unwrap())
        })
        .await
        .unwrap();
    let command = CommandId::generate().unwrap();
    let first = store
        .command(command, SessionCommand::CancelRun { run_id: next })
        .await
        .unwrap();
    let replayed = store
        .command(command, SessionCommand::CancelRun { run_id: next })
        .await
        .unwrap();
    let child_cancelled = store.cancellation_requested(child.run_id).await.unwrap();
    store.close().await.unwrap();
    assert!(child_cancelled);
    assert!(first.cascade_cancels.contains(&child.run_id));
    assert!(
        replayed.cascade_cancels.contains(&child.run_id),
        "replay must restore signals for all durably cancelled goal work"
    );
    assert_eq!(
        serde_json::to_string(&first.receipt).unwrap(),
        serde_json::to_string(&replayed.receipt).unwrap()
    );
}

#[tokio::test]
async fn cancelling_replaced_goal_work_preserves_the_replacement() {
    let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
    let session = claim.identity.session_id;
    let captured = claim.goal.as_ref().unwrap();
    let receipt = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetGoal {
                session_id: session,
                objective: "replacement".to_owned(),
                check: None,
                budget: Box::new(captured.budget.clone()),
                expected: Some(qq_protocol::GoalIdentity {
                    goal_id: captured.goal_id,
                    revision: captured.revision,
                }),
                replace: true,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated {
        goal: replacement, ..
    } = receipt.receipt.outcome
    else {
        panic!("goal")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun {
                run_id: claim.identity.run_id,
            },
        )
        .await
        .unwrap();
    let after = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
    assert_ne!(replacement.goal_id, captured.goal_id);
    assert_eq!(*replacement, after);
}

#[tokio::test]
async fn legacy_no_turn_dispatch_stays_unknown_after_upgrade_and_recovery() {
    for version in [54, 51] {
        let (directory, store, claim) = super::goals::model_goal_claim(false).await;
        let session = claim.identity.session_id;
        let run = claim.identity.run_id;
        store
            .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
            .await
            .unwrap();
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        if version == 51 {
            connection
                .execute_batch(
                    "ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id;
                ALTER TABLE runs DROP COLUMN goal_request_pending;
                ALTER TABLE runs DROP COLUMN goal_helper_admitted;
                ALTER TABLE model_turns DROP COLUMN work_disposition;",
                )
                .unwrap();
        } else {
            connection
                .execute_batch("UPDATE runs SET goal_request_pending=0,goal_helper_admitted=0")
                .unwrap();
        }
        connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='schema_version'",
                [version.to_string()],
            )
            .unwrap();
        drop(connection);
        let reopened = Store::open(path).await.unwrap();
        let pending = reopened
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_request_pending FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        reopened.recover_interrupted_runs().await.unwrap();
        let goal = reopened
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert!(
            reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        reopened.close().await.unwrap();
        assert!(
            pending,
            "schema{version}: durable waiting event proves an unresolved send"
        );
        assert_eq!(goal.counters.runs, 0);
        assert_eq!(goal.counters.usage, None);
        assert_eq!(goal.counters.estimated_cost_usd_nanos, None);
    }
}

#[tokio::test]
async fn legacy_dispatch_repair_distinguishes_prefix_completion_and_rejection() {
    for case in [
        "no_send",
        "completed_zero",
        "prefix_pending",
        "rejected",
        "terminal",
    ] {
        let (directory, store, claim) = super::goals::model_goal_claim(false).await;
        let session = claim.identity.session_id;
        let run = claim.identity.run_id;
        if case != "no_send" {
            store
                .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
                .await
                .unwrap();
            let usage = Some(if case == "prefix_pending" {
                TokenUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    ..TokenUsage::default()
                }
            } else {
                TokenUsage::default()
            });
            let cost = Some(if case == "prefix_pending" { 37 } else { 0 });
            let accounting = RunAccounting {
                goal_request_rejected: case == "rejected",
                usage,
                estimated_cost_usd_nanos: cost,
                saw_turn: true,
                context_tokens: None,
                final_output: None,
                request_basis: ContextOccupancyBasis {
                    version: 1,
                    shape: ContentHash::from_bytes([0; 32]),
                    static_prefix: PreparedStaticPrefix::new(
                        ContentHash::from_bytes([0; 32]),
                        None,
                    ),
                    request_bytes: 0,
                },
            };
            store
                .persist_model_turn(
                    &claim,
                    ModelTurnCommit {
                        model_work: if case == "rejected" {
                            crate::runtime::ModelWorkDisposition::RejectedBeforeWork
                        } else {
                            crate::runtime::ModelWorkDisposition::Committed
                        },
                        turn_ordinal: 1,
                        message: Message::new(Role::Assistant, Vec::new()),
                        calls: Vec::new(),
                        turn_message: None,
                        context_tokens: None,
                        occupancy_basis: None,
                        usage,
                        estimated_cost_usd_nanos: cost,
                        accounting: Some(accounting.clone()),
                        truncated: false,
                        notice: None,
                        goal_notices: Vec::new(),
                        reserved_goal_notices: 0,
                    },
                )
                .await
                .unwrap();
            if case == "terminal" {
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
        }
        if case == "prefix_pending" {
            store
                .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
                .await
                .unwrap();
        }
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("UPDATE runs SET goal_request_pending=0; UPDATE metadata SET value='54' WHERE key='schema_version'").unwrap();
        drop(connection);
        let reopened = Store::open(path).await.unwrap();
        let pending = reopened
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_request_pending FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        reopened.recover_interrupted_runs().await.unwrap();
        let goal = reopened
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert!(
            reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        reopened.close().await.unwrap();
        assert_eq!(pending, case == "prefix_pending", "{case}");
        assert_eq!(
            goal.counters.runs,
            u32::from(!matches!(case, "no_send" | "rejected")),
            "{case}"
        );
        assert_eq!(
            goal.counters.usage,
            if case == "prefix_pending" {
                None
            } else {
                Some(TokenUsage::default())
            },
            "{case}"
        );
        assert_eq!(
            goal.counters.estimated_cost_usd_nanos,
            if case == "prefix_pending" {
                None
            } else {
                Some(0)
            },
            "{case}"
        );
    }
}

#[tokio::test]
async fn legacy_dispatch_repair_invalid_required_event_rolls_back_schema_and_flags() {
    for fault in ["typed", "wrong_workspace", "oversized_other_workspace"] {
        let (directory, store, claim) = super::goals::model_goal_claim(false).await;
        let run = claim.identity.run_id;
        store
            .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
            .await
            .unwrap();
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("UPDATE runs SET goal_request_pending=0; UPDATE metadata SET value='54' WHERE key='schema_version'").unwrap();
        if fault == "typed" {
            connection.execute_batch("UPDATE events SET envelope_json=json_set(envelope_json,'$.event.activity','invalid_activity') WHERE json_extract(envelope_json,'$.event.type')='run_activity_changed'").unwrap();
        } else {
            let other = WorkspaceId::generate().unwrap().to_string();
            connection
                .execute(
                    "INSERT INTO workspaces(id,path) VALUES(?1,'other-workspace')",
                    [&other],
                )
                .unwrap();
            connection.execute("UPDATE events SET workspace_id=?1,envelope_json=json_set(envelope_json,'$.cursor.workspace_id',?1) WHERE json_extract(envelope_json,'$.event.type')='run_activity_changed'",[other]).unwrap();
            if fault == "oversized_other_workspace" {
                connection.execute("UPDATE events SET envelope_json=json_set(envelope_json,'$.padding',?1) WHERE json_extract(envelope_json,'$.event.type')='run_activity_changed'",["x".repeat(1024*1024)]).unwrap();
            }
        }
        drop(connection);
        let failed = Store::open(path.clone()).await;
        assert!(failed.is_err());
        let connection = rusqlite::Connection::open(path).unwrap();
        let version: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let pending: bool = connection
            .query_row(
                "SELECT goal_request_pending FROM runs WHERE id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "54");
        assert!(!pending);
    }
}
