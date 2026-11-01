use super::*;

#[tokio::test]
async fn recovered_internal_summary_settles_durable_session_cost_once() {
    for state in 0..4 {
        let directory = test_directory();
        let path = directory.path().join("recovered-summary.sqlite");
        let store = Store::open(path.clone()).await.unwrap();
        let (_, session_id, parent) = create_claimed_parent(&store, directory.path()).await;
        let model = Arc::new(test_resolved_model(
            "test/model",
            "test-model",
            256,
            Some(budget_pricing()),
        ));
        let (summary, _) = store
            .start_in_run_compaction(&parent, &model, 1)
            .await
            .unwrap()
            .unwrap();
        if state == 1 || state == 2 {
            store.start_in_run_summary_request(&summary).await.unwrap();
            let mut accounting = RunAccountingAccumulator::new(
                Some(budget_pricing()),
                context_occupancy_basis(
                    qq_protocol::ContentHash::from_bytes([0; 32]),
                    crate::runtime::PreparedStaticPrefix::new(
                        qq_protocol::ContentHash::from_bytes([0; 32]),
                        None,
                    ),
                    0,
                ),
            );
            accounting.record_turn(Some(usage(7, 3)));
            store
                .persist_in_run_summary_turn(
                    &summary,
                    TurnCommit::new(
                        1,
                        Message::new(
                            Role::Assistant,
                            vec![ContentBlock::Text {
                                text: "measured prefix".to_owned(),
                            }],
                        ),
                    )
                    .usage(Some(usage(7, 3)))
                    .cost(Some(13_000))
                    .accounting(accounting.snapshot())
                    .build(),
                )
                .await
                .unwrap();
        }
        if state == 2 || state == 3 {
            store.start_in_run_summary_request(&summary).await.unwrap();
        }
        store.close().await.unwrap();
        drop(store);
        let (mut connection, store_id) = open_database(&path).unwrap();
        let recovered =
            crate::sessions::settlement::recover_interrupted_runs(&mut connection, store_id)
                .unwrap();
        assert_eq!(recovered.len(), 2);
        let expected = if state == 1 {
            (13_000, true)
        } else {
            (0, state == 0)
        };
        for reopen in 0..2 {
            let actual: (u64, bool) = connection
                .query_row(
                    "SELECT estimated_cost_usd_nanos, cost_known FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(actual, expected, "durable state {state}, reopen {reopen}");
            assert!(
                crate::sessions::settlement::recover_interrupted_runs(&mut connection, store_id)
                    .unwrap()
                    .is_empty()
            );
            drop(connection);
            (connection, _) = open_database(&path).unwrap();
        }
    }
}

#[tokio::test]
async fn prompt_terminal_receipt_waits_for_abandoned_owned_summary() {
    for panicked in [false, true] {
        for sent in [false, true] {
            let directory = test_directory();
            let path = directory.path().join("abandoned-summary.sqlite");
            let store = Store::open(path.clone()).await.unwrap();
            let (_, session_id, parent) = create_claimed_parent(&store, directory.path()).await;
            let model = Arc::new(test_resolved_model(
                "test/model",
                "test-model",
                256,
                Some(budget_pricing()),
            ));
            let (summary, _) = store
                .start_in_run_compaction(&parent, &model, 1)
                .await
                .unwrap()
                .unwrap();
            if sent {
                store.start_in_run_summary_request(&summary).await.unwrap();
            }
            let events = if panicked {
                let parent = parent.clone();
                let summary_id = summary.identity.run_id;
                let store_id = store.store_id();
                store
                    .call(Priority::Control, move |connection| {
                        let settled = crate::sessions::settle_panicked_execution(
                            connection,
                            store_id,
                            &parent,
                            RunOutcome::Interrupted,
                        )?;
                        assert!(settled.run_ids.contains(&summary_id));
                        Ok(settled.events)
                    })
                    .await
                    .unwrap()
            } else {
                store
                    .finish_run(
                        &parent,
                        RunOutcome::Cancelled,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap()
            };
            let terminal: Vec<_> = events
                .iter()
                .filter_map(|event| match event.event {
                    SessionEvent::RunFinished { run_id, .. } => Some(run_id),
                    _ => None,
                })
                .collect();
            assert_eq!(terminal, [summary.identity.run_id, parent.identity.run_id]);
            let (outcome, spend) = store
                .run_outcome(parent.identity.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                outcome,
                if panicked {
                    RunOutcome::Interrupted
                } else {
                    RunOutcome::Cancelled
                }
            );
            let owner_id = parent.identity.run_id;
            let summary_id = summary.identity.run_id;
            let visible = store
                .call(Priority::Control, move |connection| {
                    connection.execute(
                        "UPDATE runs SET finished_at_ms = 100 WHERE id IN (?1, ?2)",
                        params![owner_id.to_string(), summary_id.to_string()],
                    )?;
                    crate::sessions::load_session_summary(connection, session_id)
                })
                .await
                .unwrap();
            assert_eq!(
                visible.last_outcome,
                Some(outcome.clone()),
                "owner must win equal millisecond timestamps"
            );
            assert_eq!(spend.cost_usd_nanos, None);
            assert!(
                store
                    .finish_in_run_compaction_failed(&summary, RunOutcome::Cancelled)
                    .await
                    .unwrap()
                    .is_empty()
            );
            store.close().await.unwrap();
            let connection = Connection::open(path).unwrap();
            let known: bool = connection
                .query_row(
                    "SELECT cost_known FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(known, !sent);
        }
    }
}

#[tokio::test]
async fn admitted_summary_turn_survives_a_dropped_waiter_before_settlement() {
    let directory = test_directory();
    let path = directory.path().join("queued-summary.sqlite");
    let store = Store::open(path.clone()).await.unwrap();
    let (_, session_id, parent) = create_claimed_parent(&store, directory.path()).await;
    let model = Arc::new(test_resolved_model(
        "test/model",
        "test-model",
        256,
        Some(budget_pricing()),
    ));
    let (summary, _) = store
        .start_in_run_compaction(&parent, &model, 1)
        .await
        .unwrap()
        .unwrap();
    store.start_in_run_summary_request(&summary).await.unwrap();
    let mut accounting = RunAccountingAccumulator::new(
        Some(budget_pricing()),
        context_occupancy_basis(
            qq_protocol::ContentHash::from_bytes([0; 32]),
            crate::runtime::PreparedStaticPrefix::new(
                qq_protocol::ContentHash::from_bytes([0; 32]),
                None,
            ),
            0,
        ),
    );
    accounting.record_turn(Some(usage(7, 3)));
    let (entered, held) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let blocked_store = store.clone();
    let blocked = tokio::spawn(async move {
        blocked_store
            .call(Priority::Control, move |_| {
                let _ = entered.send(());
                wait.recv_timeout(Duration::from_secs(5))
                    .map_err(|_| SessionRuntimeError::Unavailable)
            })
            .await
    });
    held.await.unwrap();
    let mut receipt = Box::pin(
        store.persist_in_run_summary_turn(
            &summary,
            TurnCommit::new(
                1,
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::Text {
                        text: "measured prefix".to_owned(),
                    }],
                ),
            )
            .usage(Some(usage(7, 3)))
            .cost(Some(13_000))
            .accounting(accounting.snapshot())
            .build(),
        ),
    );
    assert!(futures_util::poll!(receipt.as_mut()).is_pending());
    drop(receipt);
    let mut settlement =
        Box::pin(store.finish_in_run_compaction_failed(&summary, RunOutcome::Cancelled));
    assert!(futures_util::poll!(settlement.as_mut()).is_pending());
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    let finished = settlement.await.unwrap();
    assert_eq!(finished.iter().filter(|event| matches!(&event.event, SessionEvent::RunFinished { usage: Some(tokens), .. } if *tokens == usage(7, 3))).count(), 1);
    assert!(
        store
            .finish_in_run_compaction_failed(&summary, RunOutcome::Cancelled)
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
    let connection = Connection::open(path).unwrap();
    let (turns, encoded_usage, cost): (u64, String, u64) = connection.query_row(
        "SELECT COUNT(*), r.usage_json, r.estimated_cost_usd_nanos FROM model_turns t JOIN runs r ON r.id = t.run_id WHERE r.id = ?1",
        [summary.identity.run_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
    assert_eq!(turns, 1);
    assert_eq!(
        serde_json::from_str::<TokenUsage>(&encoded_usage).unwrap(),
        usage(7, 3)
    );
    assert_eq!(cost, 13_000);
    let session_cost: u64 = connection
        .query_row(
            "SELECT estimated_cost_usd_nanos FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_cost, 13_000);
}

#[tokio::test]
async fn settled_internal_summary_rejects_stale_turns_without_recharging() {
    let directory = test_directory();
    let store = Store::open(directory.path().join("settled-summary.sqlite"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let model = Arc::new(test_resolved_model(
        "test/model",
        "test-model",
        256,
        Some(budget_pricing()),
    ));
    let (summary, _) = store
        .start_in_run_compaction(&parent, &model, 1)
        .await
        .unwrap()
        .unwrap();
    let events = store
        .finish_in_run_compaction_failed(&summary, RunOutcome::Cancelled)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::RunFinished { .. }))
            .count(),
        1
    );
    assert!(
        store
            .finish_in_run_compaction_failed(&summary, RunOutcome::Cancelled)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        store.start_in_run_summary_request(&summary).await,
        Err(SessionRuntimeError::Unavailable)
    ));
    let stale = store
        .persist_in_run_summary_turn(
            &summary,
            TurnCommit::new(
                1,
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::Text {
                        text: "late summary".to_owned(),
                    }],
                ),
            )
            .usage(Some(usage(7, 3)))
            .cost(Some(13_000))
            .build(),
        )
        .await;
    assert!(
        matches!(stale, Err(SessionRuntimeError::Unavailable)),
        "{stale:?}"
    );
    let id = summary.identity.run_id.to_string();
    store
        .call(Priority::Control, move |connection| {
            let turns: u64 = connection.query_row(
                "SELECT COUNT(*) FROM model_turns WHERE run_id = ?1",
                [id],
                |row| row.get(0),
            )?;
            assert_eq!(turns, 0);
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[test]
fn owned_compaction_index_upgrades_preserves_rows_and_reopens() {
    for previous in ["44", "45"] {
        let directory = test_directory();
        let path = directory.path().join("owner-index.sqlite");
        let (connection, _) = super::bench_support::seed_compacted_session(&path, 3, 2, 2, 100);
        let store_id: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'store_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let original: Vec<(String, Option<String>, Option<u64>)> = connection
            .prepare("SELECT id, usage_json, estimated_cost_usd_nanos FROM runs ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        connection
            .execute_batch("DROP INDEX IF EXISTS runs_owned_compaction")
            .unwrap();
        connection
            .execute(
                "UPDATE metadata SET value = ?1 WHERE key = 'schema_version'",
                [previous],
            )
            .unwrap();
        drop(connection);
        for _ in 0..2 {
            let (connection, reopened_id) = open_database(&path).unwrap();
            assert_eq!(reopened_id.to_string(), store_id);
            let indexes: Vec<(String, bool, bool)> = connection
                .prepare("PRAGMA index_list('runs')")
                .unwrap()
                .query_map([], |row| Ok((row.get(1)?, row.get(2)?, row.get(4)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert!(indexes.contains(&("runs_owned_compaction".to_owned(), false, true)));
            let plan: Vec<String> = connection
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM runs WHERE auto_compaction_for_run_id = ?1 AND kind = 'compaction' AND auto_compaction_for_run_id IS NOT NULL")
            .unwrap()
            .query_map(["owner"], |row| row.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
            assert!(
                plan.iter()
                    .any(|detail| detail.contains("runs_owned_compaction")),
                "{plan:?}"
            );
            let rows: Vec<(String, Option<String>, Option<u64>)> = connection
                .prepare("SELECT id, usage_json, estimated_cost_usd_nanos FROM runs ORDER BY id")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(rows, original);
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, STORE_SCHEMA_VERSION.to_string());
        }
    }
}

#[test]
fn owned_compaction_index_migration_failure_preserves_previous_version() {
    let directory = test_directory();
    let path = directory.path().join("owner-index-failure.sqlite");
    let (connection, _) = open_database(&path).unwrap();
    connection.execute_batch("DROP INDEX runs_owned_compaction; UPDATE metadata SET value = '43' WHERE key = 'schema_version'; CREATE TABLE runs_owned_compaction(protected TEXT); INSERT INTO runs_owned_compaction VALUES ('retained');").unwrap();
    drop(connection);
    assert!(open_database(&path).is_err());
    let connection = Connection::open(&path).unwrap();
    let version: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "45");
    let protected: String = connection
        .query_row("SELECT protected FROM runs_owned_compaction", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(protected, "retained");
}

#[test]
fn owned_compaction_index_rejects_wrong_existing_predicate() {
    let directory = test_directory();
    let path = directory.path().join("wrong-owner-index.sqlite");
    let (connection, _) = open_database(&path).unwrap();
    connection.execute_batch("DROP INDEX runs_owned_compaction; CREATE INDEX runs_owned_compaction ON runs(auto_compaction_for_run_id) WHERE kind = 'prompt'; UPDATE metadata SET value = '43' WHERE key = 'schema_version';").unwrap();
    drop(connection);
    assert!(open_database(&path).is_err());
    let connection = Connection::open(&path).unwrap();
    let version: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "45");
    connection
        .execute(
            "UPDATE metadata SET value = '43' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(open_database(&path).is_err());
}
