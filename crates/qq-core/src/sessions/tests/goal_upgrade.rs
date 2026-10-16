use super::*;

fn legacy_checkpoint(path: &std::path::Path, version: u16) {
    let connection = Connection::open(path).unwrap();
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
}

#[tokio::test]
async fn goal_upgrade_preserves_owned_helper_prefix_and_dispatch_uncertainty() {
    for version in [51, 54] {
        for between_runs in [false, true] {
            for state in ["created", "pending", "measured", "prefix_pending"] {
                let (directory, store, initial) = super::goals::model_goal_claim(false).await;
                let session = initial.identity.session_id;
                let owner = if between_runs {
                    store
                        .finish_run(
                            &initial,
                            RunOutcome::Completed,
                            None,
                            TeardownComplete::nothing_ran(),
                        )
                        .await
                        .unwrap();
                    store
                        .command(
                            CommandId::generate().unwrap(),
                            SessionCommand::SubmitPrompt {
                                session_id: session,
                                input: vec![InputPart::text("summarize before next run")],
                                limits: RunLimits::default(),
                                correlation: Correlation::default(),
                                output: None,
                            },
                        )
                        .await
                        .unwrap();
                    store.reserve_next_run(false).await.unwrap().unwrap()
                } else {
                    initial
                };
                let audit = test_prepared_audit(&owner);
                let (helper, _) = if between_runs {
                    store
                        .start_auto_compaction(&owner, audit, None)
                        .await
                        .unwrap()
                        .unwrap()
                } else {
                    store
                        .start_in_run_compaction(&owner, &audit.resolved_model, 0)
                        .await
                        .unwrap()
                        .unwrap()
                };
                let helper_id = helper.identity.run_id;
                let usage = TokenUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    ..TokenUsage::default()
                };
                if state != "created" {
                    store.start_in_run_summary_request(&helper).await.unwrap();
                }
                if matches!(state, "measured" | "prefix_pending") {
                    store
                        .persist_in_run_summary_turn(
                            &helper,
                            ModelTurnCommit {
                                model_work: crate::runtime::ModelWorkDisposition::Committed,
                                turn_ordinal: 1,
                                message: Message::assistant("retained measured summary prefix"),
                                calls: Vec::new(),
                                turn_message: None,
                                context_tokens: None,
                                occupancy_basis: None,
                                usage: Some(usage),
                                estimated_cost_usd_nanos: Some(37),
                                accounting: Some(RunAccounting {
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
                                }),
                                truncated: false,
                                notice: None,
                                goal_notices: Vec::new(),
                                reserved_goal_notices: 0,
                            },
                        )
                        .await
                        .unwrap();
                }
                if state == "prefix_pending" {
                    store.start_in_run_summary_request(&helper).await.unwrap();
                }
                let prefix=store.call(Priority::Control,move|connection| {
                    Ok(connection.query_row(
                        "SELECT assistant_content_json,usage_json FROM model_turns WHERE run_id=?1",
                        [helper_id.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?)),
                    ).optional()?)
                }).await.unwrap();
                store.close().await.unwrap();
                let path = directory.path().join("sessions.sqlite3");
                legacy_checkpoint(&path, version);
                let reopened = Store::open(path.clone()).await.unwrap();
                let pending = reopened
                    .call(Priority::Control, move |connection| {
                        Ok(connection.query_row(
                            "SELECT goal_request_pending FROM runs WHERE id=?1",
                            [helper_id.to_string()],
                            |row| row.get::<_, bool>(0),
                        )?)
                    })
                    .await
                    .unwrap();
                reopened.recover_interrupted_runs().await.unwrap();
                let first=reopened.call(Priority::Control,move|connection| {
                    let goal=crate::sessions::goals::load_live(connection,session)?.unwrap();
                    let prefix=connection.query_row("SELECT assistant_content_json,usage_json FROM model_turns WHERE run_id=?1",[helper_id.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?))).optional()?;
                    Ok((goal,prefix,workspace_sequence(connection,owner.identity.workspace_id)?))
                }).await.unwrap();
                reopened.close().await.unwrap();
                let again = Store::open(path).await.unwrap();
                let repeated = again.recover_interrupted_runs().await.unwrap();
                let second = again
                    .call(Priority::Control, move |connection| {
                        Ok((
                            crate::sessions::goals::load_live(connection, session)?.unwrap(),
                            workspace_sequence(connection, owner.identity.workspace_id)?,
                        ))
                    })
                    .await
                    .unwrap();
                again.close().await.unwrap();
                assert_eq!(
                    pending,
                    matches!(state, "pending" | "prefix_pending"),
                    "v{version} between={between_runs} {state}"
                );
                assert_eq!(
                    first.1, prefix,
                    "measured prefix bytes survive upgrade and recovery"
                );
                assert_eq!(
                    first.0.counters.runs, 0,
                    "helpers never charge a main goal run"
                );
                assert_eq!(
                    first.0.counters.usage,
                    match state {
                        "created" => Some(TokenUsage::default()),
                        "measured" => Some(usage),
                        _ => None,
                    },
                    "v{version} between={between_runs} {state}"
                );
                assert_eq!(
                    first.0.counters.estimated_cost_usd_nanos,
                    match state {
                        "created" => Some(0),
                        "measured" => Some(37),
                        _ => None,
                    }
                );
                assert!(repeated.is_empty());
                assert_eq!(
                    (first.0, first.2),
                    second,
                    "recovery after reopen must not recharge or republish"
                );
            }
        }
    }
}

#[tokio::test]
async fn goal_upgrade_preserves_independent_routing_receipts_before_main_start() {
    for version in [51, 54] {
        for state in [
            "skipped",
            "pending",
            "zero",
            "measured",
            "usage_only",
            "cost_only",
        ] {
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
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id: session,
                        input: vec![InputPart::text("route before main")],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            let run = claim.identity.run_id;
            store
                .record_routing_started(&claim, state != "skipped", None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let usage = match state {
                "skipped" | "zero" => Some(TokenUsage::default()),
                "measured" | "usage_only" => Some(TokenUsage {
                    input_tokens: 13,
                    output_tokens: 2,
                    ..TokenUsage::default()
                }),
                _ => None,
            };
            let cost = match state {
                "skipped" | "zero" => Some(0),
                "measured" | "cost_only" => Some(37),
                _ => None,
            };
            if !matches!(state, "skipped" | "pending") {
                store
                    .record_routing_completed(
                        &claim,
                        qq_protocol::RoutingDecision {
                            model: claim.model.clone(),
                            reasoning_effort: None,
                            outcome: qq_protocol::RoutingOutcome::Selected,
                            reason: "deterministic upgrade fixture".to_owned(),
                            usage,
                            estimated_cost_usd_nanos: cost,
                        },
                    )
                    .await
                    .unwrap()
                    .unwrap();
            }
            store.close().await.unwrap();
            let path = directory.path().join("sessions.sqlite3");
            legacy_checkpoint(&path, version);
            let reopened = Store::open(path.clone()).await.unwrap();
            reopened.recover_interrupted_runs().await.unwrap();
            let first = reopened
                .call(Priority::Control, move |connection| {
                    let totals = connection.query_row(
                        "SELECT usage_json,estimated_cost_usd_nanos FROM runs WHERE id=?1",
                        [run.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<u64>>(1)?,
                            ))
                        },
                    )?;
                    Ok((
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                        totals,
                        workspace_sequence(connection, workspace)?,
                    ))
                })
                .await
                .unwrap();
            reopened.close().await.unwrap();
            let again = Store::open(path).await.unwrap();
            let repeated = again.recover_interrupted_runs().await.unwrap();
            let second = again
                .call(Priority::Control, move |connection| {
                    Ok((
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                        workspace_sequence(connection, workspace)?,
                    ))
                })
                .await
                .unwrap();
            again.close().await.unwrap();
            assert_eq!(
                first.0.counters.runs, 0,
                "routing never charges a main goal run"
            );
            assert_eq!(first.0.counters.usage, usage, "v{version} {state}");
            assert_eq!(
                first.0.counters.estimated_cost_usd_nanos, cost,
                "v{version} {state}"
            );
            assert_eq!(
                first
                    .1
                    .0
                    .map(|value| serde_json::from_str::<TokenUsage>(&value).unwrap()),
                usage
            );
            assert_eq!(first.1.1, cost);
            assert!(repeated.is_empty());
            assert_eq!(
                (first.0, first.2),
                second,
                "recovery after reopen must not recharge or republish"
            );
        }
    }
}
