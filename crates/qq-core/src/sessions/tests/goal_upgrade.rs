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
async fn goal_check_schema56_upgrade_preserves_unknown_interruption_until_acknowledged() {
    let (directory, store, claim) = super::goals::model_goal_claim(true).await;
    let session = claim.identity.session_id;
    let workspace = claim.identity.workspace_id;
    let run = claim.identity.run_id;
    store
        .finish_run(
            &claim,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let mut goal = (*claim.goal.as_ref().unwrap()).as_ref().clone();
    goal.check = Some(qq_protocol::GoalCheck {
        command: "true".to_owned(),
        timeout_ms: 5000,
    });
    goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::Consumed);
    goal.status = qq_protocol::GoalStatus::Paused {
        reason: qq_protocol::GoalPauseReason::NeedsUser {
            cause: qq_protocol::GoalNeedsUser::CheckInterrupted,
        },
    };
    goal.audit_pending = true;
    goal.audit_revision = Some(goal.revision);
    goal.pending_run_id = None;
    goal.counters.audits = 1;
    store.close().await.unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("DROP INDEX runs_pending_check_ack; ALTER TABLE runs DROP COLUMN check_acknowledgment_pending; UPDATE metadata SET value='56' WHERE key='schema_version';").unwrap();
    connection
        .execute(
            "UPDATE runs SET kind='goal_check',origin_json=?2,goal_snapshot_json=?3 WHERE id=?1",
            params![
                run.to_string(),
                serde_json::to_string(&qq_protocol::RunOrigin::GoalCheck {
                    goal_id: goal.goal_id,
                    revision: goal.revision
                })
                .unwrap(),
                serde_json::to_string(&goal).unwrap()
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE session_goals SET payload_json=?2 WHERE session_id=?1 AND live=1",
            params![session.to_string(), serde_json::to_string(&goal).unwrap()],
        )
        .unwrap();
    drop(connection);
    let upgraded = Store::open(path.clone()).await.unwrap();
    let snapshot = upgraded
        .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
        .await
        .unwrap();
    assert_eq!(
        snapshot.check_interruption,
        Some(qq_protocol::CheckInterruption {
            session_id: session,
            run_id: run
        })
    );
    assert_eq!(snapshot.focused.unwrap().goal.as_deref(), Some(&goal));
    upgraded.close().await.unwrap();
    let again = Store::open(path).await.unwrap();
    let snapshot = again
        .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
        .await
        .unwrap();
    assert_eq!(
        snapshot.check_interruption,
        Some(qq_protocol::CheckInterruption {
            session_id: session,
            run_id: run
        })
    );
    assert_eq!(snapshot.focused.unwrap().goal.as_deref(), Some(&goal));
    let id = CommandId::generate().unwrap();
    let acknowledge = SessionCommand::AcknowledgeCheckInterruption {
        workspace_id: workspace,
        run_id: run,
    };
    let first = again.command(id, acknowledge.clone()).await.unwrap();
    assert_eq!(
        again.command(id, acknowledge).await.unwrap().receipt,
        first.receipt
    );
    let snapshot = again
        .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
        .await
        .unwrap();
    assert!(snapshot.check_interruption.is_none());
    assert!(matches!(
        snapshot.focused.unwrap().goal.unwrap().status,
        qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::User
        }
    ));
    again.close().await.unwrap();
}

#[tokio::test]
async fn goal_check_migration_rejects_incompatible_acknowledgment_storage_without_advancing_version()
 {
    for version in [56, 57] {
        for (column, index) in [
            (
                "TEXT NOT NULL DEFAULT 0",
                "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1",
            ),
            (
                "INTEGER DEFAULT NULL",
                "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1",
            ),
            (
                "INTEGER NOT NULL DEFAULT 0",
                "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1",
            ),
            (
                "INTEGER NOT NULL DEFAULT 0 CHECK(check_acknowledgment_pending IN (0,1) AND (check_acknowledgment_pending=0 OR kind='goal_check'))",
                "CREATE INDEX runs_pending_check_ack ON runs(id) WHERE check_acknowledgment_pending=0",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("sessions.sqlite3");
            Store::open(path.clone())
                .await
                .unwrap()
                .close()
                .await
                .unwrap();
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(&format!("DROP INDEX runs_pending_check_ack; ALTER TABLE runs DROP COLUMN check_acknowledgment_pending; ALTER TABLE runs ADD COLUMN check_acknowledgment_pending {column}; {index};")).unwrap();
            connection
                .execute(
                    "UPDATE metadata SET value=?1 WHERE key='schema_version'",
                    [version.to_string()],
                )
                .unwrap();
            drop(connection);
            assert!(
                matches!(
                    Store::open(path.clone()).await,
                    Err(SessionRuntimeError::CONSTRAINT)
                ),
                "schema {version} accepted {column} / {index}"
            );
            let connection = Connection::open(path).unwrap();
            let actual: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                actual,
                version.to_string(),
                "failed migration must roll back metadata"
            );
        }
    }
}
