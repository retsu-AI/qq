use super::*;
use crate::test_support::within;

#[tokio::test]
async fn paid_preparation_freezes_the_optional_goal_before_main_start() {
    for has_goal in [false, true] {
        for routed in [false] {
            let directory = TempDir::new().unwrap();
            let store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let receipt = store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::ResolveWorkspace {
                        path: directory.path().to_str().unwrap().to_owned(),
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.receipt.outcome else {
                panic!("workspace")
            };
            let receipt = store
                .command(
                    CommandId::generate().unwrap(),
                    create_session_command(workspace_id, None, ApprovalMode::default()),
                )
                .await
                .unwrap();
            let CommandOutcome::SessionCreated { session_id } = receipt.receipt.outcome else {
                panic!("session")
            };
            store_command(&store, session_id, "queued before the goal")
                .await
                .unwrap();
            let goal = if has_goal {
                let receipt = store.command(CommandId::generate().unwrap(), goal_command(serde_json::json!({"session_id":session_id, "objective":"first objective",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))).await.unwrap();
                let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
                    panic!("goal")
                };
                Some(goal)
            } else {
                None
            };
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            let run_id = claim.identity.run_id;
            assert!(
                store
                    .start_auto_compaction(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .is_some()
            );
            let (id, revision, frozen) = store
                .call(Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT goal_id, goal_revision, goal_snapshot_json FROM runs WHERE id = ?1",
                        [run_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<u32>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(
                id,
                goal.as_ref().map(|goal| goal.goal_id.to_string()),
                "routed={routed}"
            );
            assert_eq!(revision, goal.as_ref().map(|goal| goal.revision));
            let frozen =
                frozen.expect("paid admission durably distinguishes captured None from uncaptured");
            assert_eq!(
                serde_json::from_str::<Option<qq_protocol::GoalSnapshot>>(&frozen)
                    .unwrap()
                    .as_ref(),
                goal.as_deref()
            );
            let expected = goal.as_ref().map(|goal| qq_protocol::GoalIdentity {
                goal_id: goal.goal_id,
                revision: goal.revision,
            });
            let receipt = store.command(CommandId::generate().unwrap(), goal_command(serde_json::json!({"session_id":session_id, "objective":"replacement objective",
                "expected":expected, "replace":has_goal,
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))).await.unwrap();
            let CommandOutcome::GoalUpdated {
                goal: replacement, ..
            } = receipt.receipt.outcome
            else {
                panic!("replacement")
            };
            let original = claim.goal.clone();
            let initial_notice = claim.goal_initial_notice.clone();
            let replacement_notice = crate::tools::goal::render_notice(&replacement, now_ms());
            let identity = claim.identity;
            store.call(Priority::Control, move |connection| {
                crate::sessions::goals::freeze_goal_claim(connection, identity, original.as_deref(), initial_notice.as_deref())?;
                crate::sessions::goals::freeze_goal_claim(connection, identity, original.as_deref(), initial_notice.as_deref())?;
                assert_eq!(crate::sessions::goals::freeze_goal_claim(connection, identity, Some(&replacement), Some(&replacement_notice)), Err(SessionRuntimeError::CONSTRAINT));
                if !routed {
                    connection.execute(
                        "UPDATE runs SET status = 'completed', outcome_json = ?2, finished_at_ms = 1
                         WHERE auto_compaction_for_run_id = ?1",
                        params![run_id.to_string(), serde_json::to_string(&RunOutcome::Completed).unwrap()],
                    )?;
                    connection.execute(
                        "UPDATE sessions SET active_run_id = NULL, status = 'idle' WHERE id = ?1",
                        [session_id.to_string()],
                    )?;
                }
                Ok(())
            }).await.unwrap();
            store.close().await.unwrap();
            drop(store);
            let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            reopened.recover_interrupted_runs().await.unwrap();
            if !routed {
                let reclaimed = reopened.reserve_next_run(false).await.unwrap().unwrap();
                assert_eq!(reclaimed.identity.run_id, run_id);
                assert_eq!(
                    reclaimed.goal.as_deref(),
                    goal.as_deref(),
                    "replacement cannot rebind paid preparation"
                );
                let started = reopened
                    .start_reserved_run(&reclaimed, test_prepared_audit(&reclaimed), None)
                    .await
                    .unwrap();
                if has_goal {
                    assert!(
                        reclaimed.cancel_requested,
                        "replacement retains durable cancellation after paid preparation"
                    );
                    assert!(
                        matches!(
                            started,
                            Err(crate::sessions::claim::GoalStartRefusal::Cancelled)
                        ),
                        "cancelled paid preparation cannot start model work"
                    );
                    reopened
                        .finish_reserved_run(&reclaimed, RunOutcome::Cancelled)
                        .await
                        .unwrap();
                } else {
                    assert!(!reclaimed.cancel_requested);
                    assert!(
                        started.unwrap().is_some(),
                        "captured absence remains runnable and gains no later goal authority"
                    );
                }
            }
            let persisted = reopened
                .call(Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT goal_snapshot_json FROM runs WHERE id = ?1",
                        [run_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(persisted, frozen, "restart retains the exact attribution");
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn goal_terminal_spend_is_once_and_stays_with_the_captured_historical_goal() {
    for goal_origin in [false, true] {
        for replace in [false, true] {
            for measured in [false, true] {
                let (_directory, store, claim) = model_goal_claim(goal_origin).await;
                let captured = claim.goal.as_ref().unwrap();
                let goal_id = captured.goal_id;
                let session_id = claim.identity.session_id;
                let replacement = if replace {
                    let result = store
                        .command(
                            CommandId::generate().unwrap(),
                            goal_command(serde_json::json!({"session_id":session_id,
                                "objective":"replacement work", "replace":true,
                                "expected":{"goal_id":goal_id,"revision":captured.revision},
                                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                            })),
                        )
                        .await
                        .unwrap();
                    let CommandOutcome::GoalUpdated { goal, .. } = result.receipt.outcome else {
                        panic!("replacement");
                    };
                    Some(goal.goal_id)
                } else {
                    None
                };
                let usage = measured.then_some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    ..TokenUsage::default()
                });
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage,
                    context_tokens: None,
                    estimated_cost_usd_nanos: measured.then_some(37),
                    saw_turn: true,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                    final_output: None,
                };
                store
                    .persist_model_turn(
                        &claim,
                        TurnCommit::new(1, Message::assistant("observed model work"))
                            .usage(usage)
                            .cost(measured.then_some(37))
                            .accounting(accounting.clone())
                            .build(),
                    )
                    .await
                    .unwrap();
                let events = store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting.clone()),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
                assert!(!events.is_empty());
                let replay = store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
                assert!(
                    replay.is_empty(),
                    "terminal replay must publish and charge nothing"
                );
                let (historical, live) = store
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [goal_id.to_string()],
                            |row| row.get(0),
                        )?;
                        let historical: qq_protocol::GoalSnapshot = serde_json::from_str(&payload)?;
                        Ok((
                            historical,
                            crate::sessions::goals::load_live(connection, session_id)?.unwrap(),
                        ))
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    historical.counters.runs, 1,
                    "origin={goal_origin}, replace={replace}, measured={measured}"
                );
                assert_eq!(historical.counters.usage, usage);
                assert_eq!(
                    historical.counters.estimated_cost_usd_nanos,
                    measured.then_some(37)
                );
                if let Some(replacement) = replacement {
                    assert_eq!(live.goal_id, replacement);
                    assert_eq!(live.counters, qq_protocol::GoalCounters::default());
                    assert!(
                        !events.iter().any(|event| matches!(&event.event,
                        SessionEvent::GoalUpdated {goal} if goal.goal_id==goal_id)),
                        "archived accounting cannot replace the live client's goal"
                    );
                } else {
                    assert_eq!(live, historical);
                    assert!(
                        events.iter().any(|event| matches!(&event.event,
                        SessionEvent::GoalUpdated {goal} if goal.goal_id==goal_id)),
                        "live counters must be published with terminal settlement"
                    );
                }
                store.close().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn failed_terminal_goal_event_rolls_back_spend_and_outcome_then_retries_once() {
    let (_directory, store, claim) = model_goal_claim(false).await;
    let run = claim.identity.run_id;
    let goal_id = claim.goal.as_ref().unwrap().goal_id;
    let accounting = RunAccounting {
        goal_request_rejected: false,
        usage: Some(TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..TokenUsage::default()
        }),
        context_tokens: None,
        estimated_cost_usd_nanos: Some(37),
        saw_turn: true,
        request_basis: ContextOccupancyBasis {
            version: 1,
            shape: ContentHash::from_bytes([0; 32]),
            static_prefix: PreparedStaticPrefix::new(ContentHash::from_bytes([0; 32]), None),
            request_bytes: 0,
        },
        final_output: None,
    };
    store
        .persist_model_turn(
            &claim,
            TurnCommit::new(1, Message::assistant("observed model work"))
                .usage(accounting.usage)
                .cost(accounting.estimated_cost_usd_nanos)
                .accounting(accounting.clone())
                .build(),
        )
        .await
        .unwrap();
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch(
                "CREATE TEMP TRIGGER refuse_terminal_goal BEFORE INSERT ON events
            WHEN json_extract(NEW.envelope_json,'$.event.type')='goal_updated'
            BEGIN SELECT RAISE(ABORT,'terminal goal event fault'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut snapshots = Vec::new();
    for after_failure in [false, true] {
        if after_failure {
            assert!(
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting.clone()),
                        TeardownComplete::nothing_ran()
                    )
                    .await
                    .is_err()
            );
        }
        snapshots.push(
            store
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [goal_id.to_string()],
                        |row| row.get(0),
                    )?;
                    let outcome: Option<String> = connection.query_row(
                        "SELECT outcome_json FROM runs WHERE id=?1",
                        [run.to_string()],
                        |row| row.get(0),
                    )?;
                    let events: u64 =
                        connection
                            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
                    Ok((payload, outcome, events))
                })
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        snapshots[0], snapshots[1],
        "failed goal publication must roll back terminal accounting and outcome"
    );
    assert!(snapshots[1].1.is_none());
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch("DROP TRIGGER refuse_terminal_goal")?;
            Ok(())
        })
        .await
        .unwrap();
    let events = store
        .finish_run(
            &claim,
            RunOutcome::Completed,
            Some(accounting.clone()),
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(&event.event,SessionEvent::GoalUpdated {goal}
        if goal.goal_id==goal_id && goal.counters.runs==1 && goal.counters.estimated_cost_usd_nanos==Some(37))));
    assert!(
        matches!(&events.last().unwrap().event,SessionEvent::RunFinished {run_id,..} if *run_id==run)
    );
    assert!(
        store
            .finish_run(
                &claim,
                RunOutcome::Completed,
                Some(accounting),
                TeardownComplete::nothing_ran()
            )
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejected_provider_outage_spends_no_goal_run_or_measured_budget() {
    struct Outage;
    impl Provider for Outage {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Err(qq_provider::ProviderError::Api {
                status: 529,
                message: "scripted unavailable before model output".to_owned(),
            })]))
        }
    }
    struct Loader;
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async move {
                let runtime = Runtime::new(Outage, "test-model", 256).unwrap();
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(Loader),
    )
    .await
    .unwrap();
    let (workspace_id, session_id, _) =
        new_session(&runtime, directory.path(), ApprovalMode::ReadOnly).await;
    let receipt = set_goal(
        &runtime,
        serde_json::json!({"session_id":session_id,"objective":"survive provider outage",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+86_400_000}
        }),
    )
    .await
    .unwrap();
    let mut events = subscribe_after(&runtime, workspace_id, receipt.committed_through);
    let run_id = submit(&runtime, session_id, "work").await;
    let events = collect_until_run_finished(&mut events, run_id).await;
    assert!(
        matches!(
            finished_outcome(&events, run_id),
            Some(RunOutcome::Paused { .. })
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

#[tokio::test]
async fn pending_goal_request_recovery_poison_spend_without_charging_a_run() {
    let (directory, store, claim) = model_goal_claim(false).await;
    let session = claim.identity.session_id;
    store
        .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
        .await
        .unwrap();
    let run = claim.identity.run_id;
    let pending = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT goal_request_pending FROM runs WHERE id=?1",
                [run.to_string()],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(pending);
    store.close().await.unwrap();
    let reopened = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    reopened.recover_interrupted_runs().await.unwrap();
    let goal = reopened
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        goal.counters.runs, 0,
        "an unconfirmed dispatch is not a committed model turn"
    );
    assert_eq!(goal.counters.usage, None);
    assert_eq!(goal.counters.estimated_cost_usd_nanos, None);
    let (cost_known, summary) = reopened
        .call(Priority::Control, move |connection| {
            let known: bool = connection.query_row(
                "SELECT cost_known FROM sessions WHERE id=?1",
                [session.to_string()],
                |row| row.get(0),
            )?;
            Ok((known, load_session_summary(connection, session)?))
        })
        .await
        .unwrap();
    assert!(
        !cost_known,
        "pending recovery preserves session cost uncertainty without in-memory goal capability"
    );
    assert_eq!(summary.estimated_cost_usd_nanos, None);

    assert!(
        reopened
            .recover_interrupted_runs()
            .await
            .unwrap()
            .is_empty()
    );
    let again = reopened
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(again, goal);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn goal_request_migration_refuses_bad_shapes_without_advancing_version() {
    for column in ["goal_request_pending", "goal_helper_admitted"] {
        for definition in ["TEXT", "INTEGER", "INTEGER NOT NULL DEFAULT 7"] {
            let directory = TempDir::new().unwrap();
            let path = directory.path().join("sessions.sqlite3");
            let (connection, _) = open_database(&path).unwrap();
            connection
                .execute_batch(&format!(
                    "ALTER TABLE runs DROP COLUMN {column};
            ALTER TABLE runs ADD COLUMN {column} {definition};
            ALTER TABLE model_turns DROP COLUMN work_disposition;
            UPDATE metadata SET value='50' WHERE key='schema_version';"
                ))
                .unwrap();
            drop(connection);
            assert!(open_database(&path).is_err());
            let connection = rusqlite::Connection::open(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, "51");
            let columns:u32=connection.query_row("SELECT COUNT(*) FROM pragma_table_info('model_turns') WHERE name='work_disposition'",[],|row|row.get(0)).unwrap();
            assert_eq!(
                columns, 0,
                "failed migration cannot leave a new evidence column"
            );
        }
    }
}

#[tokio::test]
async fn owned_compaction_spend_survives_owner_settlement_and_goal_replacement() {
    for state in ["created", "pending", "measured"] {
        for replace in [false, true] {
            let (_directory, store, owner) = model_goal_claim(false).await;
            let session = owner.identity.session_id;
            let goal = owner.goal.as_ref().unwrap();
            let id = goal.goal_id;
            let audit = test_prepared_audit(&owner);
            let (helper, _) = store
                .start_in_run_compaction(&owner, &audit.resolved_model, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                helper.goal.is_none(),
                "helper never acquires goal tool authority"
            );
            let usage = TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            };
            if state != "created" {
                store.start_in_run_summary_request(&helper).await.unwrap();
            }
            if state == "measured" {
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage: Some(usage),
                    context_tokens: None,
                    estimated_cost_usd_nanos: Some(43),
                    saw_turn: true,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                    final_output: None,
                };
                store
                    .persist_in_run_summary_turn(
                        &helper,
                        TurnCommit::new(1, Message::assistant("observed summary work"))
                            .usage(Some(usage))
                            .cost(Some(43))
                            .accounting(accounting)
                            .build(),
                    )
                    .await
                    .unwrap();
            }
            if replace {
                store.command(CommandId::generate().unwrap(), goal_command(serde_json::json!({"session_id":session, "objective":"replacement", "replace":true,
                    "expected":{"goal_id":id,"revision":goal.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))).await.unwrap();
            }
            if state == "measured" && !replace {
                store
                    .call(Priority::Control, |connection| {
                        connection.execute_batch(
                            "CREATE TEMP TRIGGER refuse_helper_goal BEFORE INSERT ON events
                        WHEN json_extract(NEW.envelope_json,'$.event.type')='goal_updated'
                        BEGIN SELECT RAISE(ABORT,'helper goal event fault'); END;",
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
                assert!(
                    store
                        .finish_run(
                            &owner,
                            RunOutcome::Interrupted,
                            None,
                            TeardownComplete::nothing_ran()
                        )
                        .await
                        .is_err()
                );
                let owner_id = owner.identity.run_id;
                let helper_id = helper.identity.run_id;
                let unchanged=store.call(Priority::Control, move |connection| {
                    let settled:u32=connection.query_row("SELECT COUNT(*) FROM runs WHERE id IN (?1,?2) AND outcome_json IS NOT NULL", params![owner_id.to_string(),helper_id.to_string()], |row|row.get(0))?;
                    let goal=crate::sessions::goals::load_live(connection,session)?.unwrap();
                    connection.execute_batch("DROP TRIGGER refuse_helper_goal")?;
                    Ok((settled,goal.counters))
                }).await.unwrap();
                assert_eq!(
                    unchanged,
                    (0, qq_protocol::GoalCounters::default()),
                    "failed helper publication rolls back both terminal rows and spend"
                );
            }
            let events = store
                .finish_run(
                    &owner,
                    RunOutcome::Interrupted,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.event, SessionEvent::GoalUpdated { .. }))
                    .count(),
                usize::from(state != "created" && !replace)
            );
            assert!(
                store
                    .finish_in_run_compaction_failed(&helper, RunOutcome::Interrupted)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (historical, live) = store
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok((
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(historical.counters.runs, 0, "helper is not a main goal run");
            assert_eq!(
                historical.counters.usage,
                match state {
                    "created" => Some(TokenUsage::default()),
                    "measured" => Some(usage),
                    _ => None,
                },
                "helper state={state}, replace={replace}"
            );
            assert_eq!(
                historical.counters.estimated_cost_usd_nanos,
                match state {
                    "created" => Some(0),
                    "measured" => Some(43),
                    _ => None,
                }
            );
            if replace {
                assert_eq!(live.counters, qq_protocol::GoalCounters::default());
            } else {
                assert_eq!(live, historical);
            }
            store.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn between_run_compactor_charges_its_goal_after_owner_cancel_and_reopen() {
    for replace in [false, true] {
        for recover in [false, true] {
            let (directory, store, previous) = model_goal_claim(false).await;
            let session = previous.identity.session_id;
            store
                .finish_run(
                    &previous,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            store_command(&store, session, "needs a summary")
                .await
                .unwrap();
            let owner = store.reserve_next_run(false).await.unwrap().unwrap();
            let goal = owner.goal.as_ref().unwrap();
            let id = goal.goal_id;
            let (helper, _) = store
                .start_auto_compaction(&owner, test_prepared_audit(&owner), None)
                .await
                .unwrap()
                .unwrap();
            assert!(helper.goal.is_none());
            let usage = TokenUsage {
                input_tokens: 17,
                output_tokens: 5,
                ..TokenUsage::default()
            };
            let accounting = RunAccounting {
                goal_request_rejected: false,
                usage: Some(usage),
                context_tokens: None,
                estimated_cost_usd_nanos: Some(61),
                saw_turn: true,
                request_basis: ContextOccupancyBasis {
                    version: 1,
                    shape: ContentHash::from_bytes([0; 32]),
                    static_prefix: PreparedStaticPrefix::new(
                        ContentHash::from_bytes([0; 32]),
                        None,
                    ),
                    request_bytes: 0,
                },
                final_output: None,
            };
            store
                .persist_in_run_summary_turn(
                    &helper,
                    TurnCommit::new(1, Message::assistant("measured summary prefix"))
                        .usage(Some(usage))
                        .cost(Some(61))
                        .accounting(accounting)
                        .build(),
                )
                .await
                .unwrap();
            store
                .finish_reserved_run(&owner, RunOutcome::Cancelled)
                .await
                .unwrap();
            if replace {
                store.command(CommandId::generate().unwrap(), goal_command(serde_json::json!({"session_id":session,"objective":"replacement","replace":true,
                    "expected":{"goal_id":id,"revision":goal.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))).await.unwrap();
            }
            store.close().await.unwrap();
            let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            if recover {
                assert!(
                    !reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert!(
                    !reopened
                        .finish_in_run_compaction_failed(&helper, RunOutcome::Interrupted)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
            assert!(
                reopened
                    .finish_in_run_compaction_failed(&helper, RunOutcome::Interrupted)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (historical, live) = reopened
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok((
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(historical.counters.runs, 0);
            assert_eq!(historical.counters.usage, Some(usage));
            assert_eq!(historical.counters.estimated_cost_usd_nanos, Some(61));
            if replace {
                assert_eq!(live.counters, qq_protocol::GoalCounters::default());
            } else {
                assert_eq!(live, historical);
            }
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn real_in_run_compactor_preserves_rejected_prefix_and_uncertain_dispatch() {
    struct Fault {
        prefix: bool,
        transport: bool,
        evidence: &'static str,
        calls: AtomicUsize,
    }
    impl Provider for Fault {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            if self.prefix && self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return provider_script(
                    tool_call("summary_call", "read_file", r#"{"path":"note"}"#)
                        .into_iter()
                        .chain([completed(Some(qq_provider::ProviderUsage {
                            input_tokens: 11,
                            output_tokens: 3,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: None,
                        }))]),
                );
            }
            if self.evidence == "eof" {
                return Box::pin(stream::empty());
            }
            let error = if self.transport {
                qq_provider::ProviderError::Transport("scripted connection loss".to_owned())
            } else {
                qq_provider::ProviderError::Api {
                    status: 529,
                    message: "scripted rejection before output".to_owned(),
                }
            };
            let mut events = Vec::new();
            match self.evidence {
                "text" => events.push(Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "partial".to_owned(),
                })),
                "reasoning" => events.push(Ok(qq_provider::ProviderEvent::ReasoningDelta {
                    kind: qq_protocol::ReasoningKind::ExposedThinking,
                    text: "thinking".to_owned(),
                })),
                _ => {}
            }
            events.push(Err(error));
            Box::pin(stream::iter(events))
        }
    }
    struct Loader;
    impl RuntimeLoader for Loader {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async { panic!("scheduler permit is held") })
        }
    }
    for prefix in [false, true] {
        for (transport, evidence) in [
            (false, "none"),
            (true, "none"),
            (false, "text"),
            (false, "reasoning"),
            (false, "eof"),
        ] {
            let directory = TempDir::new().unwrap();
            let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
            options.max_active_runs = 1;
            let runtime = SessionRuntime::open(options, Arc::new(Loader))
                .await
                .unwrap();
            let permit = runtime.hold_scheduling().await;
            let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
            let created =
                create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
            let CommandOutcome::SessionCreated {
                session_id: session,
            } = created.outcome
            else {
                panic!("session")
            };
            set_goal(
                &runtime,
                serde_json::json!({"session_id":session,"objective":"preserve helper spending",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }),
            )
            .await
            .unwrap();
            try_submit(&runtime, session, "work").await.unwrap();
            let store = &runtime.store();
            let owner = store.reserve_next_run(false).await.unwrap().unwrap();
            store
                .start_reserved_run(&owner, test_prepared_audit(&owner), None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let loaded = loaded_runtime(
                Runtime::new(
                    Fault {
                        prefix,
                        transport,
                        evidence,
                        calls: AtomicUsize::new(0),
                    },
                    "test-model",
                    256,
                )
                .unwrap(),
                &owner.workspace,
                None,
            );
            let compactor = crate::sessions::in_run_compaction::SessionInRunCompactor::new(
                Arc::clone(&runtime.inner),
                &owner,
                &loaded,
            );
            let result = crate::runtime::InRunCompactor::compact(
                &compactor,
                crate::runtime::InRunCompactionRequest {
                    transcript: vec![Message::user("summarize")],
                    turn_cutoff: 0,
                    inherited_prefix: false,
                    system: Arc::from("system"),
                    tools: Arc::from([qq_provider::ToolSpec::new(
                        "read_file",
                        "read",
                        serde_json::json!({"type":"object"}),
                    )]),
                    retained_bytes: 0,
                    goal_notice: None,
                    budget: crate::runtime::ChildBudget::default(),
                },
            )
            .await;
            let Err(crate::runtime::InRunCompactionError::SummarizerUnavailable {
                usage,
                estimated_cost_usd_nanos,
                ..
            }) = result
            else {
                panic!("expected scripted provider fault")
            };
            let expected_usage = if transport || evidence != "none" {
                None
            } else {
                Some(if prefix {
                    TokenUsage {
                        input_tokens: 11,
                        output_tokens: 3,
                        ..TokenUsage::default()
                    }
                } else {
                    TokenUsage::default()
                })
            };
            let expected_cost = if transport || prefix || evidence != "none" {
                None
            } else {
                Some(0)
            };
            assert_eq!(
                usage, expected_usage,
                "prefix={prefix}, transport={transport}, evidence={evidence}"
            );
            assert_eq!(estimated_cost_usd_nanos, expected_cost);
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(goal.counters.runs, 0);
            assert_eq!(goal.counters.usage, expected_usage);
            assert_eq!(goal.counters.estimated_cost_usd_nanos, expected_cost);
            store
                .finish_run(
                    &owner,
                    RunOutcome::Interrupted,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            drop(permit);
            runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn real_between_run_compactor_records_goal_dispatch_and_known_rejection() {
    struct Seed {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for Seed {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            provider_script([
                text_delta("x".repeat(29 * 1024 * 4)),
                completed(Some(qq_provider::ProviderUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                })),
            ])
        }
    }
    struct Fault {
        transport: bool,
        cancel: bool,
        prefix: bool,
        calls: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for Fault {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            assert!(
                request_texts(&request)
                    .iter()
                    .any(|text| text.contains("Summarize this conversation")),
                "only automatic summarizer work is admitted"
            );
            self.requests.lock().unwrap().push(request);
            if self.prefix && self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return provider_script(
                    tool_call("summary_call", "read_file", r#"{"path":"note"}"#)
                        .into_iter()
                        .chain([completed(Some(qq_provider::ProviderUsage {
                            input_tokens: 11,
                            output_tokens: 3,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: None,
                        }))]),
                );
            }
            if self.cancel {
                return Box::pin(stream::pending());
            }
            let error = if self.transport {
                qq_provider::ProviderError::Transport("scripted interrupted summary".to_owned())
            } else {
                qq_provider::ProviderError::Api {
                    status: 529,
                    message: "scripted empty summary rejection".to_owned(),
                }
            };
            Box::pin(stream::iter([Err(error)]))
        }
    }
    struct Loader {
        transport: bool,
        cancel: bool,
        prefix: bool,
        loads: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let first = self.loads.fetch_add(1, Ordering::SeqCst) == 0;
            let requests = Arc::clone(&self.requests);
            let transport = self.transport;
            let cancel = self.cancel;
            let prefix = self.prefix;
            Box::pin(async move {
                let runtime = if first {
                    Runtime::new(Seed { requests }, "test-model", 1024).unwrap()
                } else {
                    Runtime::new(
                        Fault {
                            transport,
                            cancel,
                            prefix,
                            calls: AtomicUsize::new(0),
                            requests,
                        },
                        "test-model",
                        1024,
                    )
                    .unwrap()
                };
                Ok(loaded_runtime(
                    runtime.with_context_window(Some(32 * 1024)),
                    &request.workspace,
                    Some(ModelPricing {
                        input_usd_nanos_per_token: 1,
                        output_usd_nanos_per_token: 1,
                        cache_read_usd_nanos_per_token: Some(1),
                        cache_write_usd_nanos_per_token: Some(1),
                        context_tier: None,
                        provenance: "deterministic fixture".to_owned(),
                    }),
                ))
            })
        }
    }
    for (transport, cancel, prefix) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(Loader {
                transport,
                cancel,
                prefix,
                loads: AtomicUsize::new(0),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
        let CommandOutcome::SessionCreated {
            session_id: session,
        } = created.outcome
        else {
            panic!("session")
        };
        let mut events = subscribe_after(&runtime, workspace, created.committed_through);
        let first = queue_prompt(&runtime, session, "grow".to_owned()).await;
        let initial = collect_until_run_finished(&mut events, first).await;
        assert_eq!(
            finished_outcome(&initial, first),
            Some(RunOutcome::Completed)
        );
        set_goal(&runtime, serde_json::json!({"session_id":session,"objective":"survive automatic compaction outage",
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000,"max_cost_usd_nanos":(!transport && !cancel && !prefix).then_some(1_000_000_u64)}
        })).await.unwrap();
        let owner = queue_prompt(&runtime, session, "continue".to_owned()).await;
        let cancelled_helper = if cancel {
            within(async {
                while requests.lock().unwrap().len() < if prefix { 3 } else { 2 } {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned summary dispatch occurs");
            let helper:RunId=runtime.store().call(Priority::Control,move|connection| {
                let (id,admitted,usage):(String,bool,Option<String>)=connection.query_row("SELECT id,goal_helper_admitted,usage_json FROM runs WHERE kind='compaction' AND auto_compaction_for_run_id=?1",[owner.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
                assert!(admitted && usage.is_none(),"paid admission is durable before provider poll");
                parse_id(&id)
            }).await.unwrap();
            cancel_run(&runtime, owner).await.unwrap();
            Some(helper)
        } else {
            None
        };
        let observed = collect_until_run_finished(&mut events, owner).await;
        if cancel {
            assert_eq!(
                finished_outcome(&observed, owner),
                Some(RunOutcome::Cancelled)
            );
            let preparing: Option<String> = runtime
                .store()
                .call(Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT preparing_run_id FROM sessions WHERE id=?1",
                        [session.to_string()],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert!(
                preparing.is_none(),
                "cancelled owner releases preparation only after its helper drains"
            );
        }
        if let Some(helper) = cancelled_helper
            && !observed.iter().any(|event|matches!(event.event,SessionEvent::RunFinished{run_id,..} if run_id==helper)) {
                collect_until_run_finished(&mut events,helper).await;
        }
        assert!(
            requests.lock().unwrap().len() > 1,
            "provider dispatch occurred"
        );
        let connection = rusqlite::Connection::open(&path).unwrap();
        let rows:Vec<(Option<String>,Option<u64>,Option<String>)>=connection.prepare("SELECT usage_json,estimated_cost_usd_nanos,goal_id FROM runs WHERE kind='compaction' AND auto_compaction_for_run_id=?1").unwrap().query_map([owner.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().map(Result::unwrap).collect();
        assert!(!rows.is_empty());
        let expected_usage = (!transport).then_some(if prefix {
            TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            }
        } else {
            TokenUsage::default()
        });
        let expected_cost = (!transport).then_some(if prefix { 14 } else { 0 });
        for (usage, cost, authority) in rows {
            let usage: Option<TokenUsage> =
                usage.map(|encoded| serde_json::from_str(&encoded).unwrap());
            assert_eq!(usage, expected_usage, "transport={transport}");
            assert_eq!(cost, expected_cost);
            assert!(
                authority.is_none(),
                "compactor has accounting only, not goal authority"
            );
        }
        drop(connection);
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .find(|summary| summary.id == session)
                .unwrap()
                .estimated_cost_usd_nanos,
            expected_cost
        );
        let cost_known = runtime
            .store()
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT cost_known FROM sessions WHERE id=?1",
                    [session.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            cost_known, !transport,
            "durable session certainty agrees with public accounting"
        );
        let goal = snapshot.focused.unwrap().goal.unwrap();
        assert_eq!(goal.counters.runs, 0);
        assert_eq!(goal.counters.usage, expected_usage);
        assert_eq!(goal.counters.estimated_cost_usd_nanos, expected_cost);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn committed_goal_turn_recovery_charges_session_and_historical_goal_once() {
    for measured in [false, true] {
        for pending in [false, true] {
            for replace in [false, true] {
                let (directory, store, claim) = model_goal_claim(false).await;
                let session = claim.identity.session_id;
                store.call(Priority::Control,move|connection| {
                    connection.execute("UPDATE runs SET usage_json=?2,estimated_cost_usd_nanos=0 WHERE session_id=?1 AND outcome_json IS NOT NULL AND NOT EXISTS(SELECT 1 FROM model_turns WHERE run_id=runs.id)",params![session.to_string(),serde_json::to_string(&TokenUsage::default())?])?;
                    assert_eq!(load_session_summary(connection,session)?.estimated_cost_usd_nanos,Some(0),"fixture starts with known-zero prior spending");
                    Ok(())
                }).await.unwrap();

                let goal = claim.goal.as_ref().unwrap();
                let id = goal.goal_id;
                let usage = TokenUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    ..TokenUsage::default()
                };
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage: Some(usage),
                    estimated_cost_usd_nanos: measured.then_some(37),
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
                        TurnCommit::new(1, Message::assistant("committed before crash"))
                            .usage(Some(usage))
                            .cost(measured.then_some(37))
                            .accounting(accounting)
                            .build(),
                    )
                    .await
                    .unwrap();
                if pending {
                    store
                        .append_run_activity(
                            &claim,
                            qq_protocol::RunActivity::WaitingForProvider,
                            false,
                        )
                        .await
                        .unwrap();
                }
                if replace {
                    store.command(CommandId::generate().unwrap(),goal_command(serde_json::json!({"session_id":session,"objective":"replacement","replace":true,
                        "expected":{"goal_id":id,"revision":goal.revision},
                        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                    }))).await.unwrap();
                }
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
                let receipt = reopened
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [id.to_string()],
                            |row| row.get(0),
                        )?;
                        let totals: (u64, bool) = connection.query_row(
                            "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                            [session.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?;
                        Ok((
                            serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                            crate::sessions::goals::load_live(connection, session)?.unwrap(),
                            totals,
                            load_session_summary(connection, session)?,
                        ))
                    })
                    .await
                    .unwrap();
                let known_cost = measured && !pending;
                assert_eq!(
                    receipt.2,
                    (if known_cost { 37 } else { 0 }, known_cost),
                    "stored session cost measured={measured}, pending={pending}, replace={replace}"
                );
                assert_eq!(receipt.0.counters.runs, 1);
                assert_eq!(receipt.0.counters.usage, (!pending).then_some(usage));
                assert_eq!(
                    receipt.0.counters.estimated_cost_usd_nanos,
                    known_cost.then_some(37)
                );
                assert_eq!(receipt.3.estimated_cost_usd_nanos, known_cost.then_some(37));
                if replace {
                    assert_eq!(receipt.1.counters, qq_protocol::GoalCounters::default());
                } else {
                    assert_eq!(receipt.0, receipt.1);
                }
                assert!(
                    reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
                let after = reopened
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [id.to_string()],
                            |row| row.get(0),
                        )?;
                        let totals: (u64, bool) = connection.query_row(
                            "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                            [session.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?;
                        Ok((
                            serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                            totals,
                        ))
                    })
                    .await
                    .unwrap();
                assert_eq!(after, (receipt.0, receipt.2));
                reopened.close().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn rejection_only_goal_recovery_does_not_invent_session_spending() {
    let (directory, store, claim) = model_goal_claim(false).await;
    let session = claim.identity.session_id;
    store
        .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
        .await
        .unwrap();
    store
        .persist_model_turn(
            &claim,
            ModelTurnCommit {
                model_work: crate::runtime::ModelWorkDisposition::RejectedBeforeWork,
                turn_ordinal: 1,
                message: Message::assistant(""),
                calls: Vec::new(),
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: Some(RunAccounting {
                    goal_request_rejected: true,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    saw_turn: false,
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
                }),
                truncated: false,
                notice: None,
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
            },
        )
        .await
        .unwrap();
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
    let (totals, goal) = reopened
        .call(Priority::Control, move |connection| {
            let totals: (u64, bool) = connection.query_row(
                "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                [session.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok((
                totals,
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
            ))
        })
        .await
        .unwrap();
    assert_eq!(totals, (0, true));
    assert_eq!(goal.counters, qq_protocol::GoalCounters::default());
    assert!(
        reopened
            .recover_interrupted_runs()
            .await
            .unwrap()
            .is_empty()
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn delegated_child_spend_keeps_the_original_goal_after_replacement() {
    for child_goal in [false, true] {
        for replace in [false, true] {
            let (_directory, store, parent) = model_goal_claim(false).await;
            let session = parent.identity.session_id;
            let goal = parent.goal.as_ref().unwrap();
            let id = goal.goal_id;
            let child = store
                .create_child_run(
                    &parent,
                    ToolCallId::from_bytes([0x6b; 16]),
                    ChildAdmission {
                        reasoning_effort: None,
                        profile: AgentProfileId::default(),
                        model: parent.model.clone(),
                        task: "delegated work".to_owned(),
                        limits: RunLimits::default(),
                        approval_mode: ApprovalMode::ReadOnly,
                        purpose: SessionPurpose::Task,
                        detached: true,
                    },
                )
                .await
                .unwrap();
            let own_goal = if child_goal {
                let receipt=store.command(CommandId::generate().unwrap(),goal_command(serde_json::json!({"session_id":child.session_id,"objective":"independent child objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))).await.unwrap();
                let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
                    panic!("child goal")
                };
                Some(goal)
            } else {
                None
            };
            let claim = store.claim_next_run(true).await.unwrap().unwrap();
            assert_eq!(claim.identity.run_id, child.run_id);
            assert_eq!(
                claim.goal.as_ref().map(|goal| goal.goal_id),
                own_goal.as_ref().map(|goal| goal.goal_id)
            );
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
                    TurnCommit::new(1, Message::assistant("child completed work"))
                        .usage(Some(usage))
                        .cost(Some(37))
                        .accounting(accounting.clone())
                        .build(),
                )
                .await
                .unwrap();
            if replace {
                store.command(CommandId::generate().unwrap(),goal_command(serde_json::json!({"session_id":session,"objective":"replacement","replace":true,
                "expected":{"goal_id":id,"revision":goal.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))).await.unwrap();
            }
            store
                .finish_run(
                    &parent,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            let events = store
                .finish_run(
                    &claim,
                    RunOutcome::Completed,
                    Some(accounting.clone()),
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.event, SessionEvent::GoalUpdated { .. }))
                    .count(),
                usize::from(!replace) + usize::from(child_goal)
            );
            for event in &events {
                if let SessionEvent::GoalUpdated { goal } = &event.event {
                    let expected = if goal.goal_id == id {
                        session
                    } else {
                        child.session_id
                    };
                    assert_eq!(goal.session_id, expected);
                    assert_eq!(
                        event.session_id, expected,
                        "each paid goal update has its own session context"
                    );
                }
            }
            assert!(
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting),
                        TeardownComplete::nothing_ran()
                    )
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (historical, live) = store
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok((
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(
                historical.counters.runs, 0,
                "delegated work does not consume a main goal run"
            );
            assert_eq!(historical.counters.usage, Some(usage));
            assert_eq!(historical.counters.estimated_cost_usd_nanos, Some(37));
            if replace {
                assert_eq!(live.counters, qq_protocol::GoalCounters::default());
            } else {
                assert_eq!(historical, live);
            }
            if let Some(own_goal) = own_goal {
                let child_session = child.session_id;
                let own = store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, child_session)?.unwrap())
                    })
                    .await
                    .unwrap();
                assert_eq!(own.goal_id, own_goal.goal_id);
                assert_eq!(own.counters.runs, 1);
                assert_eq!(own.counters.usage, Some(usage));
                assert_eq!(own.counters.estimated_cost_usd_nanos, Some(37));
                store_command(&store, child_session, "unrelated later prompt")
                    .await
                    .unwrap();
                let followup = store.claim_next_run(true).await.unwrap().unwrap();
                assert!(
                    followup.goal_spend_owner.is_none(),
                    "later user prompt has no old root accounting binding"
                );
                let followup_accounting = RunAccounting {
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
                        &followup,
                        TurnCommit::new(1, Message::assistant("later work"))
                            .usage(Some(usage))
                            .cost(Some(37))
                            .accounting(followup_accounting.clone())
                            .build(),
                    )
                    .await
                    .unwrap();
                store
                    .finish_run(
                        &followup,
                        RunOutcome::Completed,
                        Some(followup_accounting),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
                let original = store
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [id.to_string()],
                            |row| row.get(0),
                        )?;
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)
                            .map_err(SessionRuntimeError::from)
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    original, historical,
                    "later work leaves original ancestor goal unchanged"
                );
            }
            store.close().await.unwrap();
        }
    }
}

#[test]
fn paid_owner_schema_upgrades_and_rejects_bad_shapes_atomically() {
    for definition in [None, Some("INTEGER"), Some("TEXT NOT NULL DEFAULT ''")] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (connection, _) = open_database(&path).unwrap();
        connection.execute_batch("ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id; UPDATE metadata SET value='50' WHERE key='schema_version';").unwrap();
        if let Some(definition) = definition {
            connection
                .execute_batch(&format!(
                    "ALTER TABLE runs ADD COLUMN goal_paid_owner_run_id {definition};"
                ))
                .unwrap();
        }
        drop(connection);
        if definition.is_none() {
            let (connection, _) = open_database(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, STORE_SCHEMA_VERSION.to_string());
            let nullable:Option<String>=connection.query_row("SELECT dflt_value FROM pragma_table_info('runs') WHERE name='goal_paid_owner_run_id'",[],|row|row.get(0)).unwrap();
            assert_eq!(nullable, None);
            drop(connection);
            assert!(open_database(&path).is_ok());
        } else {
            assert!(open_database(&path).is_err());
            let connection = rusqlite::Connection::open(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                version, "52",
                "failed shape validation does not advance version"
            );
        }
    }
}

#[tokio::test]
async fn sponsored_child_rejected_before_turn_preserves_known_zero_goal_spend() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "delegated work".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claim.identity.run_id, child.run_id);
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET goal_request_pending=1 WHERE id=?1",
                [child.run_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let accounting = RunAccounting {
        goal_request_rejected: true,
        usage: Some(TokenUsage::default()),
        estimated_cost_usd_nanos: Some(0),
        saw_turn: false,
        context_tokens: None,
        final_output: None,
        request_basis: ContextOccupancyBasis {
            version: 1,
            shape: ContentHash::from_bytes([0; 32]),
            static_prefix: PreparedStaticPrefix::new(ContentHash::from_bytes([0; 32]), None),
            request_bytes: 0,
        },
    };
    store
        .finish_run(
            &claim,
            RunOutcome::Cancelled,
            Some(accounting),
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let (pending, goal) = store
        .call(Priority::Control, move |connection| {
            let pending: bool = connection.query_row(
                "SELECT goal_request_pending FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| row.get(0),
            )?;
            Ok((
                pending,
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
            ))
        })
        .await
        .unwrap();
    assert!(
        !pending,
        "typed rejection clears sponsored dispatch uncertainty without a persisted turn"
    );
    assert_eq!(goal.counters, qq_protocol::GoalCounters::default());
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn parent_deletion_preserves_unsettled_paid_child_then_detaches_settled_child() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "delegated work".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claim.identity.run_id, child.run_id);
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let rejected = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: session,
            },
        )
        .await;
    assert!(
        matches!(rejected, Err(SessionRuntimeError::SessionActive)),
        "unsettled sponsored work retains historical ownership"
    );
    store
        .finish_run(
            &claim,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: session,
            },
        )
        .await
        .unwrap();
    let detached: (Option<String>,Option<String>,Option<String>) = store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT s.parent_id,s.owner_run_id,r.goal_paid_owner_run_id FROM sessions s JOIN runs r ON r.session_id=s.id WHERE r.id=?1", [child.run_id.to_string()], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?)
    }).await.unwrap();
    assert_eq!(detached, (None, None, None));
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_admission_refuses_expired_or_unknown_budgets_before_loading() {
    struct Loader(Arc<AtomicUsize>);
    impl RuntimeLoader for Loader {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(RuntimeLoadError {
                    kind: RunFailureKind::Authentication,
                    message: "expired goal must never reach loading".to_owned(),
                })
            })
        }
    }
    for (reason, limit) in [
        (
            qq_protocol::GoalExhaustion::Deadline,
            qq_protocol::BudgetLimitKind::Duration,
        ),
        (
            qq_protocol::GoalExhaustion::TokensUnknown,
            qq_protocol::BudgetLimitKind::TokensUnknown,
        ),
        (
            qq_protocol::GoalExhaustion::CostUnknown,
            qq_protocol::BudgetLimitKind::CostUnknown,
        ),
    ] {
        for user_prompt in [false, true] {
            let directory = TempDir::new().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let runtime = SessionRuntime::open(
                SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
                Arc::new(Loader(Arc::clone(&calls))),
            )
            .await
            .unwrap();
            let held = runtime.hold_scheduling().await;
            let (workspace_id, session_id, created) =
                new_session(&runtime, directory.path(), ApprovalMode::default()).await;
            let mut events = subscribe_after(&runtime, workspace_id, created.committed_through);
            set_goal(&runtime, serde_json::json!({"session_id":session_id, "objective":"expire while awaiting the permit",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).await.unwrap();
            runtime.store().drive_goals(now_ms()).await.unwrap();
            if user_prompt {
                queue_prompt(
                    &runtime,
                    session_id,
                    "explicit work under the same goal".to_owned(),
                )
                .await;
            }
            runtime
                .store()
                .call(Priority::Control, move |connection| {
                    let mut goal =
                        crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                    match reason {
                        qq_protocol::GoalExhaustion::Deadline => {
                            goal.budget.deadline_ms = now_ms().saturating_sub(1)
                        }
                        qq_protocol::GoalExhaustion::TokensUnknown => {
                            goal.budget.max_total_tokens = Some(100);
                            goal.counters.usage = None;
                        }
                        qq_protocol::GoalExhaustion::CostUnknown => {
                            goal.budget.max_cost_usd_nanos = Some(100);
                            goal.counters.estimated_cost_usd_nanos = None;
                        }
                        _ => unreachable!(),
                    }
                    crate::sessions::goals::save(connection, &goal)
                })
                .await
                .unwrap();
            drop(held);
            runtime.request_schedule();
            let outcome = within(async {
                let mut finished = None;
                let mut exhausted = false;
                while let Some(event) = events.next().await {
                    match event.unwrap().event {
                        SessionEvent::RunFinished { outcome, .. }
                            if !matches!(outcome, RunOutcome::Cancelled) =>
                        {
                            finished = Some(outcome)
                        }
                        SessionEvent::GoalUpdated { goal }
                            if matches!(
                                goal.status,
                                qq_protocol::GoalStatus::BudgetExhausted {
                                    reason: actual
                                } if actual == reason
                            ) =>
                        {
                            exhausted = true
                        }
                        _ => {}
                    }
                    if exhausted && let Some(outcome) = finished.take() {
                        return outcome;
                    }
                }
                panic!("stream ended");
            })
            .await
            .unwrap();
            runtime.shutdown().await.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "origin user_prompt={user_prompt}: expired work must not load"
            );
            assert!(
                matches!(outcome, RunOutcome::BudgetExhausted { ref exhaustion } if exhaustion.limit == limit)
            );
            let connection =
                rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
            let goal = crate::sessions::goals::load_live(&connection, session_id)
                .unwrap()
                .unwrap();
            let bound: u64 = connection.query_row(
            "SELECT COUNT(*) FROM runs WHERE session_id=?1 AND status='budget_exhausted' AND goal_id=?2 AND goal_snapshot_json IS NOT NULL",
            params![session_id.to_string(), goal.goal_id.to_string()], |row| row.get(0),
        ).unwrap();
            assert_eq!(bound, 1, "refused prompt remains durably goal-bound");
            assert_eq!(goal.counters.runs, 0);
            assert!(matches!(
                goal.status,
                qq_protocol::GoalStatus::BudgetExhausted {
                    reason: actual
                } if actual == reason
            ));
        }
    }
}

#[tokio::test]
async fn common_goal_reservation_clamps_current_remainders_and_preserves_prompt_limits() {
    for kind in ["goal", "unbounded_prompt", "strict_prompt"] {
        let (_directory, store, previous) = model_goal_claim(false).await;
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
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.budget.deadline_ms = now_ms() + 60_000;
                goal.budget.max_total_tokens = Some(100);
                goal.budget.max_cost_usd_nanos = Some(100);
                goal.budget.per_run = RunLimits {
                    max_model_turns: Some(2),
                    max_total_tokens: Some(80),
                    max_cost_usd_nanos: Some(70),
                    max_duration_ms: Some(30_000),
                    ..RunLimits::default()
                };
                goal.counters.usage = Some(usage(11, 4));
                goal.counters.estimated_cost_usd_nanos = Some(17);
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        if kind == "goal" {
            store.queue_due_goals(now_ms()).await.unwrap();
        } else {
            let limits = if kind == "strict_prompt" {
                RunLimits {
                    max_model_turns: Some(1),
                    max_total_tokens: Some(30),
                    max_cost_usd_nanos: Some(40),
                    max_duration_ms: Some(1000),
                    ..RunLimits::default()
                }
            } else {
                RunLimits::default()
            };
            store_command(
                &store,
                session_id,
                Prompt::text("explicit prompt budget").limits(limits),
            )
            .await
            .unwrap();
            store
                .call(Priority::Control, move |connection| {
                    let mut goal =
                        crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                    goal.counters.usage = Some(usage(17, 8));
                    goal.counters.estimated_cost_usd_nanos = Some(27);
                    crate::sessions::goals::save(connection, &goal)
                })
                .await
                .unwrap();
        }
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        let (tokens, cost, turns, duration) = match kind {
            "goal" => (80, 70, Some(2), 30_000),
            "unbounded_prompt" => (75, 73, None, 60_000),
            "strict_prompt" => (30, 40, Some(1), 1000),
            _ => unreachable!(),
        };
        assert_eq!(claim.limits.max_total_tokens, Some(tokens), "{kind}");
        assert_eq!(claim.limits.max_cost_usd_nanos, Some(cost), "{kind}");
        assert_eq!(claim.limits.max_model_turns, turns, "{kind}");
        assert!(
            claim
                .limits
                .max_duration_ms
                .is_some_and(|value| value > 0 && value <= duration)
        );
        assert!(matches!(
            claim.goal_admission,
            Some(crate::sessions::goals::GoalAdmission::Allowed { .. })
        ));
        store
            .finish_reserved_run(&claim, RunOutcome::Cancelled)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_deadline_expiring_during_loading_refuses_main_request_for_both_origins() {
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            provider_script([completed(Some(qq_provider::ProviderUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: None,
            }))])
        }
    }
    struct Loader {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        requests: Arc<AtomicUsize>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for user_prompt in [false, true] {
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let held = runtime.hold_scheduling().await;
        let (workspace_id, session_id, created) =
            new_session(&runtime, directory.path(), ApprovalMode::default()).await;
        let mut events = subscribe_after(&runtime, workspace_id, created.committed_through);
        set_goal(&runtime, serde_json::json!({"session_id":session_id,"objective":"deadline must be revalidated after loading",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).await.unwrap();
        runtime.store().drive_goals(now_ms()).await.unwrap();
        let expected_run = if user_prompt {
            queue_prompt(&runtime, session_id, "explicit bounded work".to_owned()).await
        } else {
            runtime
                .store()
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
        runtime.request_schedule();
        within(entered.notified()).await.unwrap();
        let origin = runtime
            .store()
            .call(Priority::Control, move |connection| {
                Ok(load_run(connection, expected_run)?.origin)
            })
            .await
            .unwrap();
        assert!(if user_prompt {
            matches!(origin, Some(qq_protocol::RunOrigin::UserPrompt {}))
        } else {
            matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. }))
        });
        runtime
            .store()
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.budget.deadline_ms = now_ms().saturating_sub(1);
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        release.notify_one();
        let outcome = within(async {
            while let Some(event) = events.next().await {
                if let SessionEvent::RunFinished {
                    run_id, outcome, ..
                } = event.unwrap().event
                    && run_id == expected_run
                {
                    return outcome;
                }
            }
            panic!("stream ended");
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "user_prompt={user_prompt}: no main request after expiry"
        );
        assert!(
            matches!(outcome, RunOutcome::BudgetExhausted { ref exhaustion } if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
        );
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let never_started: bool = connection.query_row(
            "SELECT started_at_ms IS NULL AND status='budget_exhausted' AND goal_id IS NOT NULL FROM runs WHERE id=?1",
            [expected_run.to_string()], |row| row.get(0),
        ).unwrap();
        assert!(
            never_started,
            "refusal must not durably start the exact run"
        );
        let goal = crate::sessions::goals::load_live(&connection, session_id)
            .unwrap()
            .unwrap();
        assert_eq!(goal.counters.runs, 0);
        assert!(matches!(
            goal.status,
            qq_protocol::GoalStatus::BudgetExhausted {
                reason: qq_protocol::GoalExhaustion::Deadline
            }
        ));
    }
}

#[tokio::test]
async fn goal_main_start_refreshes_and_persists_positive_remaining_allowance() {
    struct Model(Arc<StdMutex<Vec<u32>>>);
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.0.lock().unwrap().push(request.max_output_tokens());
            provider_script([
                text_delta("one bounded work slice"),
                completed(Some(qq_provider::ProviderUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                })),
            ])
        }
    }
    struct Loader {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        requests: Arc<StdMutex<Vec<u32>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 1024).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for (user_prompt, raised, strict) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (true, true, true),
    ] {
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let held = runtime.hold_scheduling().await;
        let (workspace_id, session_id, created) =
            new_session(&runtime, directory.path(), ApprovalMode::default()).await;
        let mut events = subscribe_after(&runtime, workspace_id, created.committed_through);
        set_goal(&runtime, serde_json::json!({"session_id":session_id,"objective":"refresh allowance after loading",
            "budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"max_total_tokens":if raised {500} else {200_000}}
        })).await.unwrap();
        runtime.store().drive_goals(now_ms()).await.unwrap();
        let run = if user_prompt {
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![InputPart::text("explicit work under the same budget")],
                        limits: RunLimits {
                            max_total_tokens: strict.then_some(200_000),
                            ..RunLimits::default()
                        },
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
                panic!("prompt")
            };
            run_id
        } else {
            runtime
                .store()
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
        runtime.request_schedule();
        within(entered.notified()).await.unwrap();
        runtime
            .store()
            .call(Priority::Control, move |connection| {
                let origin = load_run(connection, run)?.origin;
                assert!(if user_prompt {
                    matches!(origin, Some(qq_protocol::RunOrigin::UserPrompt {}))
                } else {
                    matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. }))
                });
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                if raised {
                    goal.budget.max_total_tokens = Some(300_000);
                    goal.budget.deadline_ms = now_ms() + 120_000;
                } else {
                    goal.counters.usage = Some(usage(80_000, 10_000));
                }
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        release.notify_one();
        let finished = collect_until_run_finished(&mut events, run).await;
        runtime.shutdown().await.unwrap();
        assert!(
            matches!(
                finished_outcome(&finished, run),
                Some(RunOutcome::Completed)
            ),
            "positive remainder permits bounded work: user_prompt={user_prompt}, raised={raised}, strict={strict}, outcome={:?}, stored={:?}",
            finished_outcome(&finished, run),
            Connection::open(directory.path().join("sessions.sqlite3"))
                .unwrap()
                .query_row(
                    "SELECT limits_json FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap()
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one work response and the final-slot handoff"
        );
        let expected = if strict {
            200_000
        } else if raised {
            300_000
        } else {
            110_000
        };
        assert!(
            requests.iter().all(|cap| *cap == expected.min(1024)),
            "remaining goal allowance must shape the actual request: {:?}",
            *requests
        );
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let limits: String = connection
            .query_row(
                "SELECT limits_json FROM runs WHERE id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let limits: RunLimits = serde_json::from_str(&limits).unwrap();
        assert_eq!(
            limits.max_total_tokens,
            Some(u64::from(expected)),
            "effective limits must be durable: user_prompt={user_prompt}"
        );
        let goal = crate::sessions::goals::load_live(&connection, session_id)
            .unwrap()
            .unwrap();
        if raised {
            assert!(
                limits
                    .max_duration_ms
                    .is_some_and(|duration| duration > 110_000 && duration <= 125_000),
                "raised deadline must recover its allowance: user_prompt={user_prompt}, strict={strict}, limits={limits:?}"
            );
        }
        assert_eq!(goal.counters.runs, 1);
        assert_eq!(
            goal.counters.usage,
            Some(if raised {
                usage(2, 2)
            } else {
                usage(80_002, 10_002)
            })
        );
    }
}

#[tokio::test]
async fn detached_child_refreshes_ancestor_budget_after_parent_settlement() {
    for budget in [10_000_u64, 100] {
        struct Model(Arc<StdMutex<Vec<ModelRequest>>>);
        impl Provider for Model {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.0.lock().unwrap().push(request);
                provider_script([
                    text_delta("child work"),
                    completed(Some(qq_provider::ProviderUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: None,
                    })),
                ])
            }
        }
        struct Loader {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            requests: Arc<StdMutex<Vec<ModelRequest>>>,
        }
        impl RuntimeLoader for Loader {
            fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let entered = Arc::clone(&self.entered);
                let release = Arc::clone(&self.release);
                let requests = Arc::clone(&self.requests);
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(loaded_runtime(
                        Runtime::new(Model(requests), "test-model", 16_384).unwrap(),
                        &request.workspace,
                        None,
                    ))
                })
            }
        }
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let held = runtime.hold_scheduling().await;
        let child_held = Arc::clone(&runtime.inner.child_permits[0])
            .acquire_many_owned(runtime.inner.child_permits[0].available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: parent_session,
        } = created.outcome
        else {
            panic!("session")
        };
        set_goal(
            &runtime,
            serde_json::json!({"session_id":parent_session,"objective":"original objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }),
        )
        .await
        .unwrap();
        let parent_run = queue_prompt(&runtime, parent_session, "parent work".to_owned()).await;
        let store = runtime.store().clone();
        let parent = store.reserve_next_run(false).await.unwrap().unwrap();
        assert_eq!(parent.identity.run_id, parent_run);
        store
            .start_reserved_run(&parent, test_prepared_audit(&parent), None)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let captured = parent.goal.as_ref().unwrap().clone();
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "detached bounded child".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store
            .call(Priority::Control, move |connection| {
                let mut goal =
                    crate::sessions::goals::load_live(connection, parent_session)?.unwrap();
                goal.next_run_at_ms = now_ms() + 60_000;
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        drop(child_held);
        runtime.request_schedule();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: parent.identity.workspace_id,
                after: EventCursor {
                    store_id: runtime.store_id(),
                    workspace_id: parent.identity.workspace_id,
                    sequence: 0,
                },
            })
            .unwrap();
        within(entered.notified()).await.unwrap();
        runtime
            .store()
            .call(Priority::Control, move |connection| {
                let claim: Option<String> = connection.query_row(
                    "SELECT preparing_run_id FROM sessions WHERE id=?1",
                    [child.session_id.to_string()],
                    |row| row.get(0),
                )?;
                assert_eq!(claim.as_deref(), Some(child.run_id.to_string().as_str()));
                Ok(())
            })
            .await
            .unwrap();
        let receipt = set_goal(&runtime, serde_json::json!({"session_id":parent_session,"objective":"original objective",
                "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000,"max_total_tokens":budget}
            }))
        .await
        .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal")
        };
        assert_eq!(goal.goal_id, captured.goal_id);
        runtime
        .store()
        .call(Priority::Control, move |connection| {
            let cancelled: bool = connection.query_row(
                "SELECT cancel_requested FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| row.get(0),
            )?;
            assert!(
                !cancelled,
                "editing the active ancestor budget must not confound admission with cancellation"
            );
            Ok(())
        })
        .await
        .unwrap();
        release.notify_one();
        let finished = collect_until_run_finished(&mut events, child.run_id).await;
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        let first = futures_util::poll!(&mut shutdown);
        drop(held);
        match first {
            std::task::Poll::Ready(result) => result.unwrap(),
            std::task::Poll::Pending => shutdown.await.unwrap(),
        };
        let outcome = finished_outcome(&finished, child.run_id).unwrap();
        if budget == 100 {
            assert!(
                matches!(outcome, RunOutcome::BudgetExhausted { .. }),
                "{outcome:?}"
            );
        } else {
            assert_eq!(outcome, RunOutcome::Completed);
        }
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let (ancestor, authority, origin): (Option<String>, Option<String>, String) = connection
            .query_row(
                "SELECT goal_paid_owner_run_id,goal_id,origin_json FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(ancestor, Some(parent.identity.run_id.to_string()));
        assert!(authority.is_none());
        assert!(
            matches!(serde_json::from_str::<qq_protocol::RunOrigin>(&origin).unwrap(),qq_protocol::RunOrigin::Delegated {owner_run_id,..} if owner_run_id==parent.identity.run_id)
        );
        let goal = crate::sessions::goals::load_live(&connection, parent_session)
            .unwrap()
            .unwrap();
        assert_eq!(
            goal.counters.runs, 0,
            "a sponsored child does not consume another ancestor run slot"
        );
        let requests = requests.lock().unwrap();
        if budget == 100 {
            assert_eq!(goal.counters.usage, Some(TokenUsage::default()));
            assert!(requests.is_empty(), "unaffordable input must not dispatch");
            continue;
        }
        assert_eq!(goal.counters.usage, Some(usage(1, 1)));
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0]
                .tools()
                .iter()
                .any(|tool| tool.name() == "update_goal")
        );
        assert!(
            u64::from(requests[0].max_output_tokens()) <= budget,
            "the settled parent's current ancestor allowance must shape child dispatch: {}",
            requests[0].max_output_tokens()
        );
    }
}
