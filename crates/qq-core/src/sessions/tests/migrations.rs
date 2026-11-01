use super::*;

#[tokio::test]
async fn prune_deletes_only_idle_sessions_without_messages() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, kept, _) =
        new_session(&runtime, directory.path(), ApprovalMode::default()).await;
    let mut empties = Vec::new();
    for _ in 0..2 {
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("unexpected receipt")
        };
        empties.push(session_id);
    }
    let queued = try_submit(&runtime, kept, "keep me").await.unwrap();
    let mut events = subscribe_after(&runtime, workspace_id, queued.committed_through);
    collect_through_finished(&mut events).await;

    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::PruneSessions { workspace_id },
        )
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::SessionsPruned { deleted: 2, .. }
    ));

    let mut deleted = Vec::new();
    while deleted.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let SessionEvent::SessionDeleted { session_id } = event.event {
            deleted.push(session_id);
        }
    }
    assert_eq!(deleted, empties);
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, None, 8, 8))
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        vec![kept]
    );

    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::PruneSessions { workspace_id },
        )
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::SessionsPruned { deleted: 0, .. }
    ));
}

#[test]
fn malformed_version_eighteen_context_basis_schema_is_rejected() {
    for column in [
        "context_occupancy_json",
        "pending_context_overflow_basis_json",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (connection, _) = open_database(&path).unwrap();
        connection
            .execute(&format!("ALTER TABLE sessions DROP COLUMN {column}"), [])
            .unwrap();
        connection
            .execute(
                &format!("ALTER TABLE sessions ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"),
                [],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            open_database(&path),
            Err(SessionRuntimeError::CONSTRAINT)
        ));
    }
}

#[tokio::test]
async fn every_persistence_fault_variant_is_reachable() {
    let (directory, store, claimed) = claimed_store_fixture().await;
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch(
                "CREATE TRIGGER reject_activity BEFORE UPDATE OF activity ON runs
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let constraint = store
        .append_run_activity(&claimed, RunActivity::WaitingForProvider, false)
        .await
        .unwrap_err();
    assert_eq!(
        constraint,
        SessionRuntimeError::Persistence(PersistenceFault::Constraint)
    );
    assert!(constraint.to_string().contains("invariant"), "{constraint}");
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch(
                "DROP TRIGGER reject_activity;
                 UPDATE runs SET limits_json = 'not json';",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        parse_run_limits(Some("not json")).unwrap_err(),
        SessionRuntimeError::Persistence(PersistenceFault::Codec)
    );
    let read_only = store
        .call(Priority::Control, |connection| {
            connection.execute_batch("PRAGMA query_only = 1")?;
            let error = connection
                .execute("UPDATE runs SET activity = 'x'", [])
                .map(|_| ())
                .map_err(SessionRuntimeError::from);
            connection.execute_batch("PRAGMA query_only = 0")?;
            Ok(error)
        })
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        read_only,
        SessionRuntimeError::Persistence(PersistenceFault::Sqlite(
            rusqlite::ffi::ErrorCode::ReadOnly
        ))
    );
    store.close().await.unwrap();
    drop(directory);
}

#[test]
fn version_twenty_six_migration_adds_the_tool_call_effect_and_keeps_history_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "INSERT INTO workspaces(id, path) VALUES ('w', '/v25-effect');
             INSERT INTO sessions(id, workspace_id, title, status, created_at_ms,
                                  updated_at_ms)
             VALUES ('s', 'w', 'Old', 'idle', 1, 1);
             INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, created_at_ms)
             VALUES ('r', 's', 'cmd', 'u', 'a', 'completed', 1);
             INSERT INTO tool_calls(id, run_id, turn_ordinal, call_ordinal,
                                    provider_call_id, name, arguments_json, state,
                                    result, requested_at_ms)
             VALUES ('c', 'r', 1, 1, 'p1', 'read_file', '{}', 'completed', 'x', 1);
             UPDATE metadata SET value = '25' WHERE key = 'schema_version';
             ALTER TABLE tool_calls DROP COLUMN effect;",
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let effect: Option<String> = connection
        .query_row("SELECT effect FROM tool_calls WHERE id = 'c'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(effect, None);
}

#[test]
fn version_twenty_seven_migration_adds_the_output_contract_columns_and_keeps_history_null() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "INSERT INTO workspaces(id, path) VALUES ('w', '/v26-output');
             INSERT INTO sessions(id, workspace_id, title, status, created_at_ms,
                                  updated_at_ms)
             VALUES ('s', 'w', 'Old', 'idle', 1, 1);
             INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, created_at_ms, outcome_json)
             VALUES ('r', 's', 'cmd', 'u', 'a', 'completed', 1, '{\"type\":\"completed\"}');
             UPDATE metadata SET value = '26' WHERE key = 'schema_version';
             ALTER TABLE runs DROP COLUMN output_contract_json;
             ALTER TABLE runs DROP COLUMN final_output_json;",
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let (contract, final_output): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT output_contract_json, final_output_json FROM runs WHERE id = 'r'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(contract, None);
    assert_eq!(final_output, None);
}

#[test]
fn version_twenty_eight_migration_adds_the_spill_table_empty() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "UPDATE metadata SET value = '26' WHERE key = 'schema_version';
             DROP TABLE tool_spills;",
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let rows: u32 = connection
        .query_row("SELECT COUNT(*) FROM tool_spills", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn version_twenty_nine_migration_adds_the_attachment_tables_empty() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "UPDATE metadata SET value = '28' WHERE key = 'schema_version';
             DROP TABLE message_attachments;
             DROP TABLE attachment_blobs;",
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    for table in ["attachment_blobs", "message_attachments"] {
        let rows: u32 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table}");
    }
    let (connection, _) = open_database(&path).unwrap();
    drop(connection);
}

#[test]
fn version_thirty_migration_adds_the_messages_run_index() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "UPDATE metadata SET value = '28' WHERE key = 'schema_version';
             DROP INDEX messages_run_steering;",
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let plan: Vec<String> = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT output FROM messages
             WHERE run_id = 'r' AND steering = 1 AND state = 'complete'",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|step| step.contains("messages_run_steering")),
        "{plan:?}"
    );
}

#[tokio::test]
async fn spills_commit_with_their_result_and_read_back_exactly_within_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let text = "AWS_KEY=AKIAIOSFODNN7EXAMPLE\nline 2\nline 3\n";
    let (store, session_id, tool_call_id, digest) =
        store_with_one_spill(directory.path(), text).await;
    let call = tool_call_id.to_string();

    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let (result, spilled, bytes): (String, u32, u64) = connection
        .query_row(
            "SELECT c.result,
                    (SELECT COUNT(*) FROM tool_spills s WHERE s.tool_call_id = c.id),
                    (SELECT content_bytes FROM tool_spills s WHERE s.tool_call_id = c.id)
             FROM tool_calls c WHERE c.id = ?1",
            [&call],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(spilled, 1);
    assert_eq!(bytes, text.len() as u64);
    assert!(result.contains(&format!("t:read_file:{}:{}", &call[..8], &digest[..8])));
    drop(connection);

    let found = store
        .read_tool_spill(session_id, call[..8].to_owned(), digest[..8].to_owned())
        .await
        .unwrap();
    assert_eq!(
        found,
        crate::runtime::SpillRead::Found {
            tool: "read_file".to_owned(),
            text: text.to_owned(),
            omitted_from_line: 2,
        }
    );
    assert_eq!(
        store
            .read_tool_spill(session_id, call[..8].to_owned(), "00000000".to_owned())
            .await
            .unwrap(),
        crate::runtime::SpillRead::Missing
    );
    assert_eq!(
        store
            .read_tool_spill(session_id, "ffffffff".to_owned(), digest[..8].to_owned())
            .await
            .unwrap(),
        crate::runtime::SpillRead::Missing
    );
    let other = SessionId::generate().unwrap();
    assert_eq!(
        store
            .read_tool_spill(other, call[..8].to_owned(), digest[..8].to_owned())
            .await
            .unwrap(),
        crate::runtime::SpillRead::ForeignSession
    );
}

#[tokio::test]
async fn spills_past_the_session_cap_evict_the_oldest_finished_content() {
    let directory = tempfile::tempdir().unwrap();
    let big = "y".repeat(30 * 1024 * 1024);
    let (store, session_id, first_call, first_digest) =
        store_with_one_spill(directory.path(), &big).await;
    let mut second = big.clone();
    second.push('2');
    let (second_call, second_digest) = spill_one_call(&store, session_id, &second, "second").await;
    let mut third = big.clone();
    third.push('3');
    let (third_call, third_digest) = spill_one_call(&store, session_id, &third, "third").await;

    let prefix = |id: ToolCallId| id.to_string()[..8].to_owned();
    assert_eq!(
        store
            .read_tool_spill(session_id, prefix(first_call), first_digest[..8].to_owned())
            .await
            .unwrap(),
        crate::runtime::SpillRead::Evicted
    );
    for (call, digest, text) in [
        (second_call, second_digest, second),
        (third_call, third_digest, third),
    ] {
        match store
            .read_tool_spill(session_id, prefix(call), digest[..8].to_owned())
            .await
            .unwrap()
        {
            crate::runtime::SpillRead::Found { text: stored, .. } => assert_eq!(stored, text),
            other => panic!("expected the newer spill to survive: {other:?}"),
        }
    }
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    let held: u64 = connection
        .query_row(
            "SELECT COALESCE(SUM(content_bytes), 0) FROM tool_spills WHERE content IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(held <= MAX_SESSION_SPILL_BYTES, "{held}");
    let rows: u32 = connection
        .query_row("SELECT COUNT(*) FROM tool_spills", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 3, "evicted rows keep their handle");
}

#[tokio::test]
async fn run_activity_is_read_from_the_column_and_the_command_counter_tracks_the_journal() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (observed, _) = collect_until_approval_requested(&mut harness.events).await;
    let last_activity = observed
        .iter()
        .rev()
        .find_map(|event| match &event.event {
            SessionEvent::RunActivityChanged { activity, .. } => Some(*activity),
            _ => None,
        })
        .expect("a tool turn reports activity before the approval");
    let run_id = harness.run_id;
    let session_id = harness.session_id;
    let (column, summary, counted, journaled) = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            let column: Option<String> = connection.query_row(
                "SELECT activity FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let summary = load_session_summary(connection, session_id)?;
            let counted: String = connection.query_row(
                "SELECT value FROM metadata WHERE key = 'command_count'",
                [],
                |row| row.get(0),
            )?;
            let journaled: u32 =
                connection.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))?;
            Ok((column, summary, counted, journaled))
        })
        .await
        .unwrap();
    assert_eq!(column.as_deref(), Some(run_activity_column(last_activity)));
    assert_eq!(summary.activity, Some(last_activity));
    assert!(journaled >= 3, "resolve, create, submit");
    assert_eq!(counted, journaled.to_string());
    harness.runtime.shutdown().await.unwrap();
}

#[test]
fn a_current_store_reopens_without_rerunning_migrations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, store_id) = open_database(&path).unwrap();
    let version = |connection: &rusqlite::Connection| {
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    assert_eq!(version(&connection), STORE_SCHEMA_VERSION.to_string());
    assert!(has_column(&connection, "sessions", "reasoning_effort").unwrap());
    drop(connection);

    let (connection, reopened_store_id) = open_database(&path).unwrap();
    assert_eq!(reopened_store_id, store_id);
    assert_eq!(version(&connection), STORE_SCHEMA_VERSION.to_string());
}

#[test]
fn version_thirty_three_gains_the_reasoning_effort_pin_and_rejects_a_bad_shape() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute("ALTER TABLE sessions DROP COLUMN reasoning_effort", [])
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '33' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert!(has_column(&connection, "sessions", "reasoning_effort").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    connection
        .execute("ALTER TABLE sessions DROP COLUMN reasoning_effort", [])
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions ADD COLUMN reasoning_effort INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn malformed_version_seventeen_preparation_schema_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN pending_context_overflow_model_json",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions ADD COLUMN pending_context_overflow_model_json
             INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn version_fifteen_store_with_malformed_capacity_columns_is_rejected() {
    for (base, increment) in [
        (
            "context_base_bytes TEXT",
            "context_increment_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "context_base_bytes INTEGER NOT NULL",
            "context_increment_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "context_base_bytes INTEGER",
            "context_increment_bytes INTEGER",
        ),
        (
            "context_base_bytes INTEGER",
            "context_increment_bytes INTEGER NOT NULL DEFAULT 1",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        drop(open_database(&path).unwrap());
        rewrite_run_capacity_schema(&path, base, increment);

        assert!(matches!(
            open_database(&path),
            Err(SessionRuntimeError::CONSTRAINT)
        ));
    }
}

#[test]
fn version_fifteen_store_missing_a_linear_streaming_column_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute("ALTER TABLE message_chunks DROP COLUMN text", [])
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn message_loading_concatenates_legacy_base_and_ordered_chunks_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::from_bytes([1; 16]);
    let session_id = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    let message_id = MessageId::from_bytes([4; 16]);
    connection
        .execute(
            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'S', 'idle', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?4, 'completed', 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::from_bytes([5; 16]).to_string(),
                message_id.to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                                  output, refusal, created_at_ms)
             VALUES (?1, ?2, ?3, 1, 'assistant', 'complete', 'legacy-', 'old-', 1)",
            params![
                message_id.to_string(),
                session_id.to_string(),
                run_id.to_string(),
            ],
        )
        .unwrap();
    for (channel, ordinal, text) in [
        ("output", 2, "two"),
        ("output", 1, "one"),
        ("refusal", 1, "new"),
    ] {
        connection
            .execute(
                "INSERT INTO message_chunks(message_id, channel, chunk_ordinal, text)
                 VALUES (?1, ?2, ?3, ?4)",
                params![message_id.to_string(), channel, ordinal, text],
            )
            .unwrap();
    }

    let message = load_message(&connection, message_id).unwrap();

    assert_eq!(message.output, "legacy-onetwo");
    assert_eq!(message.refusal, "old-new");
}

#[test]
fn version_thirty_one_migration_keeps_old_runs_unrouted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/routing-migration')",
            [workspace_id.to_string()],
        )
        .unwrap();
    insert_accounting_session(&connection, workspace_id, session_id, None);
    let run = insert_accounting_run(&connection, session_id, "queued", None, None);
    connection
        .execute("ALTER TABLE runs DROP COLUMN routing_json", [])
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '28' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(connection);
    let (mut connection, store_id) = open_database(&path).unwrap();
    let routed: Option<String> = connection
        .query_row(
            "SELECT routing_json FROM runs WHERE id = ?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(routed.is_none());
    recover_interrupted_runs(&mut connection, store_id).unwrap();
    let status: String = connection
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "queued",
        "ordinary unstarted runs retain existing recovery behavior"
    );
}

#[test]
fn malformed_version_thirty_one_routing_column_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute("ALTER TABLE runs DROP COLUMN routing_json", [])
        .unwrap();
    connection
        .execute("ALTER TABLE runs ADD COLUMN routing_json INTEGER", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[tokio::test]
async fn a_v0_1_4_store_at_schema_thirty_five_upgrades_and_keeps_its_history() {
    let version = |connection: &rusqlite::Connection| {
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (workspace_id, session_id) = {
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("unexpected receipt")
        };
        let queued = try_submit(&runtime, session_id, "written by v0.1.4")
            .await
            .unwrap();
        let mut events = subscribe_after(&runtime, workspace_id, queued.committed_through);
        collect_through_finished(&mut events).await;
        runtime.shutdown().await.unwrap();
        (workspace_id, session_id)
    };

    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE metadata SET value = '35' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
    }

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(version(&connection), STORE_SCHEMA_VERSION.to_string());
    }
    let focused_transcript = |snapshot: &qq_protocol::WorkspaceSnapshot| {
        snapshot
            .focused
            .as_ref()
            .expect("the session is focused")
            .messages
            .iter()
            .map(|message| (message.role, message.state, message.output.clone()))
            .collect::<Vec<_>>()
    };
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    let migrated = focused_transcript(&snapshot);
    assert_eq!(migrated.len(), 2, "{migrated:?}");
    assert_eq!(migrated[0].0, qq_protocol::MessageRole::User);
    assert_eq!(migrated[0].2, "written by v0.1.4");
    assert_eq!(migrated[1].0, qq_protocol::MessageRole::Assistant);
    assert_eq!(migrated[1].1, qq_protocol::MessageState::Complete);
    assert!(!migrated[1].2.is_empty(), "the assistant reply survives");
    runtime.close().await.unwrap();

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(version(&connection), STORE_SCHEMA_VERSION.to_string());
    }
    let reopened = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(focused_transcript(&reopened), migrated);
    runtime.shutdown().await.unwrap();
}

#[test]
fn version_thirty_two_migration_preserves_legacy_model_pins() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/model-migration')",
            [workspace_id.to_string()],
        )
        .unwrap();
    insert_accounting_session(&connection, workspace_id, session_id, None);
    connection
        .execute("ALTER TABLE sessions DROP COLUMN model_is_fallback", [])
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '28' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(connection);
    for _ in 0..2 {
        let (connection, _) = open_database(&path).unwrap();
        let fallback: bool = connection
            .query_row(
                "SELECT model_is_fallback FROM sessions WHERE id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !fallback,
            "migration must not opt legacy model choices into routing"
        );
    }
}

#[test]
fn malformed_model_fallback_column_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute("ALTER TABLE sessions DROP COLUMN model_is_fallback", [])
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions ADD COLUMN model_is_fallback INTEGER NOT NULL DEFAULT 1",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn version_thirty_nine_gains_the_turn_notice_as_null_and_rejects_a_bad_shape() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v39-notice')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'Old', 'idle', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, created_at_ms
             ) VALUES (?1, ?2, 'cmd', 'u', 'a', 'completed', 1)",
            params![run_id.to_string(), session_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO messages(id, session_id, run_id, ordinal, turn_ordinal, role, state,
                                  output, created_at_ms)
             VALUES (?1, ?2, ?3, 1, 0, 'user', 'complete', 'old prompt', 1)",
            params![
                MessageId::generate().unwrap().to_string(),
                session_id.to_string(),
                run_id.to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
             VALUES (?1, 1, '[{\"type\":\"text\",\"text\":\"legacy\"}]')",
            [run_id.to_string()],
        )
        .unwrap();
    for statement in [
        "ALTER TABLE model_turns DROP COLUMN notice",
        "UPDATE metadata SET value = '39' WHERE key = 'schema_version'",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let notice: Option<String> = connection
        .query_row(
            "SELECT notice FROM model_turns WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(notice, None);
    let replay = |notice: Option<crate::runtime::TurnNotice>| {
        let mut context = Vec::new();
        append_run_turns(
            vec![StoredTurn {
                goal_notices: Vec::new(),
                strict_projection: true,
                checkpoint_projection_required: false,
                projection_json: None,
                run_id: "fixture".to_owned(),
                legacy_projection: false,
                ordinal: 1,
                content_json: "[{\"type\":\"text\",\"text\":\"legacy\"}]".to_owned(),
                truncated: false,
                notice,
            }],
            HashMap::new(),
            std::collections::VecDeque::new(),
            std::collections::VecDeque::new(),
            None,
            &mut context,
            &mut HashMap::new(),
        )
        .unwrap();
        context
    };
    assert_eq!(replay(None), [Message::assistant("legacy")]);
    assert_eq!(
        replay(Some(crate::runtime::TurnNotice::Report)),
        [
            Message::user(crate::SLICE_CHECKPOINT_NOTICE),
            Message::assistant("legacy")
        ]
    );
    connection
        .execute(
            "UPDATE model_turns SET notice = 'mystery' WHERE run_id = ?1",
            [run_id.to_string()],
        )
        .unwrap();
    let mut connection = connection;
    let transaction = connection.transaction().unwrap();
    assert!(matches!(
        load_model_context_with_rewrite_status(&transaction, session_id, u64::MAX),
        Err(SessionRuntimeError::CODEC)
    ));
    drop(transaction);
    for statement in [
        "ALTER TABLE model_turns DROP COLUMN notice",
        "ALTER TABLE model_turns ADD COLUMN notice INTEGER NOT NULL DEFAULT 0",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn version_forty_gains_the_child_delivery_table_and_rejects_a_bad_shape() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    for statement in [
        "DROP TABLE child_deliveries",
        "UPDATE metadata SET value = '39' WHERE key = 'schema_version'",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let rows: u32 = connection
        .query_row("SELECT COUNT(*) FROM child_deliveries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    for statement in [
        "DROP TABLE child_deliveries",
        "CREATE TABLE child_deliveries (child_run_id TEXT PRIMARY KEY, text INTEGER)",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn version_forty_one_gains_the_child_report_table_and_rejects_a_bad_shape() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    for statement in [
        "DROP TABLE child_reports",
        "UPDATE metadata SET value = '39' WHERE key = 'schema_version'",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let rows: u32 = connection
        .query_row("SELECT COUNT(*) FROM child_reports", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    for statement in [
        "DROP TABLE child_reports",
        "CREATE TABLE child_reports (child_run_id TEXT PRIMARY KEY, text INTEGER)",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[test]
fn version_forty_two_gains_the_prune_watermark_at_the_newest_turn() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    let workspace = WorkspaceId::from_bytes([0x42; 16]).to_string();
    let with_history = SessionId::from_bytes([0x43; 16]).to_string();
    let empty = SessionId::from_bytes([0x44; 16]).to_string();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v42-watermark')",
            [&workspace],
        )
        .unwrap();
    for session in [&with_history, &empty] {
        connection
            .execute(
                "INSERT INTO sessions(id, workspace_id, title, status, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 't', 'idle', 1, 1)",
                [session, &workspace],
            )
            .unwrap();
    }
    for (run, ordinal, turns) in [(0x51_u8, 1_u64, 2_u32), (0x52, 3, 3)] {
        let run_id = RunId::from_bytes([run; 16]).to_string();
        let message_id = MessageId::from_bytes([run; 16]).to_string();
        connection
            .execute(
                "INSERT INTO runs(id, session_id, command_id, user_message_id,
                                  assistant_message_id, status, created_at_ms)
                 VALUES (?1, ?2, ?1, ?3, ?3, 'completed', 1)",
                params![run_id, with_history, message_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(id, session_id, run_id, ordinal, role, state, output,
                                      created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 'user', 'complete', 'p', 1)",
                params![message_id, with_history, run_id, ordinal],
            )
            .unwrap();
        for turn in 1..=turns {
            connection
                .execute(
                    "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
                     VALUES (?1, ?2, '[]')",
                    params![run_id, turn],
                )
                .unwrap();
        }
    }
    for statement in [
        "ALTER TABLE sessions DROP COLUMN prune_through_ordinal",
        "ALTER TABLE sessions DROP COLUMN prune_through_turn",
        "UPDATE metadata SET value = '39' WHERE key = 'schema_version'",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        STORE_SCHEMA_VERSION.to_string()
    );
    let watermark = |session: &str| -> (Option<u64>, Option<u32>) {
        connection
            .query_row(
                "SELECT prune_through_ordinal, prune_through_turn FROM sessions WHERE id = ?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(watermark(&with_history), (Some(3), Some(3)));
    assert_eq!(watermark(&empty), (None, None));
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    for statement in [
        "ALTER TABLE sessions DROP COLUMN prune_through_turn",
        "ALTER TABLE sessions ADD COLUMN prune_through_turn TEXT",
    ] {
        connection.execute(statement, []).unwrap();
    }
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

#[tokio::test]
async fn a_store_older_than_the_first_release_fails_to_open_naming_its_schema() {
    for found in [1, 24] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '{found}');"
            ))
            .unwrap();
        drop(connection);
        let Err(error) = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        else {
            panic!("schema {found} opened");
        };
        assert_eq!(
            error,
            SessionRuntimeError::UnsupportedStoreSchema { found, oldest: 25 }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "session store schema {found} predates the oldest supported schema 25 (qq v0.0.1); move the store aside to start a new one"
            )
        );
        let connection = rusqlite::Connection::open(&path).unwrap();
        let tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT IN ('metadata','workspaces','sessions','runs','messages','message_chunks','events','commands')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "a refused store gains no migrated tables");
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            found.to_string()
        );
    }
}

#[test]
fn ab5_store_open_rejects_the_next_schema_before_loading_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    let future = STORE_SCHEMA_VERSION + 1;
    connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = 'schema_version'",
            [future.to_string()],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        future.to_string()
    );
}

#[test]
fn migrating_a_v0_1_6_store_keeps_its_structure_and_store_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, store_id) = open_database(&path).unwrap();
    let structure = connection
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value='43' WHERE key='schema_version'",
            [],
        )
        .unwrap();
    drop(connection);
    for _ in 0..2 {
        let (connection, reopened_id) = open_database(&path).unwrap();
        assert_eq!(reopened_id, store_id);
        let current = connection
            .prepare(
                "SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            current, structure,
            "migrations from schema 43 must not change an already-current structure"
        );
        let version: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, STORE_SCHEMA_VERSION.to_string());
    }
}

#[test]
fn ab5_schema_fifty_cannot_hide_a_missing_execution_receipt_column() {
    for previous in [50, STORE_SCHEMA_VERSION] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (connection, _) = open_database(&path).unwrap();
        connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='schema_version'",
                [previous.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "ALTER TABLE tool_calls DROP COLUMN execution_context_json",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            open_database(&path),
            Err(SessionRuntimeError::CONSTRAINT)
        ));
        let connection = rusqlite::Connection::open(&path).unwrap();
        let version: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, previous.to_string());
    }
}

#[tokio::test]
async fn a_store_with_retired_supervised_mode_opens_and_replays_as_auto() {
    for schema in [58] {
        retired_supervised_mode_upgrades_from(schema).await;
    }
}

async fn retired_supervised_mode_upgrades_from(schema: u32) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (workspace_id, session_id, store_id) = {
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("unexpected receipt")
        };
        let queued = try_submit(&runtime, session_id, "written by v0.1.8")
            .await
            .unwrap();
        let mut events = subscribe_after(&runtime, workspace_id, queued.committed_through);
        collect_through_finished(&mut events).await;
        let store_id = runtime.store_id();
        runtime.close().await.unwrap();
        (workspace_id, session_id, store_id)
    };
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "UPDATE sessions SET approval_mode='supervised';
                 UPDATE events SET envelope_json=json_set(envelope_json,'$.event.session.approval_mode','supervised')
                  WHERE json_extract(envelope_json,'$.event.session.approval_mode') IS NOT NULL;
                 UPDATE runs SET completion_json=json_set(completion_json,'$.event.session.approval_mode','supervised')
                  WHERE completion_json IS NOT NULL;",
            )
            .unwrap();
        if schema < 62 {
            connection
                .execute(
                    "UPDATE runs SET completion_sequence=0,completion_json=NULL",
                    [],
                )
                .unwrap();
        }
        connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='schema_version'",
                [schema.to_string()],
            )
            .unwrap();
        let journaled: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE json_extract(envelope_json,'$.event.session.approval_mode')='supervised'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            journaled >= 2,
            "from {schema}: the fixture journals legacy summaries"
        );
    }

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions[0].approval_mode, ApprovalMode::Auto);
    let start = EventCursor {
        store_id,
        workspace_id,
        sequence: 0,
    };
    let mut events = subscribe_after(&runtime, workspace_id, start);
    let replayed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut replayed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let last = event.cursor.sequence == snapshot.cursor.sequence;
            replayed.push(event);
            if last {
                break;
            }
        }
        replayed
    })
    .await
    .unwrap();
    let modes = replayed
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::SessionCreated { session }
            | SessionEvent::RunFinished { session, .. } => Some(session.approval_mode),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(modes.len() >= 2, "from {schema}: {modes:?}");
    assert!(
        modes.iter().all(|mode| *mode == ApprovalMode::Auto),
        "from {schema}: {modes:?}"
    );
    let completions = runtime
        .completed_runs(session_id, start, snapshot.cursor)
        .await
        .unwrap();
    assert_eq!(completions.len(), 1, "from {schema}: the receipt survives");
    let SessionEvent::RunFinished { session, .. } = &completions[0].receipt.event else {
        panic!("a completion is a run_finished receipt")
    };
    assert_eq!(session.approval_mode, ApprovalMode::Auto);
    runtime.close().await.unwrap();

    let connection = rusqlite::Connection::open(&path).unwrap();
    let remaining: u64 = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM sessions WHERE approval_mode='supervised')
                  + (SELECT COUNT(*) FROM events WHERE instr(envelope_json,'\"approval_mode\":\"supervised\"')>0)
                  + (SELECT COUNT(*) FROM runs WHERE instr(completion_json,'\"approval_mode\":\"supervised\"')>0)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining, 0,
        "from {schema}: the retired mode is rewritten once, at open"
    );
}

#[tokio::test]
async fn a_schema_64_store_with_reviewer_delegate_and_grant_state_opens_and_replays() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (workspace_id, session_id, store_id) = {
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("unexpected receipt")
        };
        let queued = try_submit(&runtime, session_id, "written by v0.1.9")
            .await
            .unwrap();
        let mut events = subscribe_after(&runtime, workspace_id, queued.committed_through);
        collect_through_finished(&mut events).await;
        let store_id = runtime.store_id();
        runtime.close().await.unwrap();
        (workspace_id, session_id, store_id)
    };
    let approved = ToolCallId::generate().unwrap();
    let denied = ToolCallId::generate().unwrap();
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        let run_id: String = connection
            .query_row("SELECT id FROM runs", [], |row| row.get(0))
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_grants (
                     session_id TEXT NOT NULL REFERENCES sessions(id),
                     kind TEXT NOT NULL,
                     value TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL,
                     source TEXT NOT NULL DEFAULT 'human',
                     run_id TEXT,
                     UNIQUE(session_id, kind, value)
                 );
                 CREATE TABLE pending_workspace_grant_promotions (
                     command_id TEXT NOT NULL PRIMARY KEY,
                     created_at_ms INTEGER NOT NULL,
                     promotion_json TEXT NOT NULL
                 );
                 CREATE INDEX pending_workspace_grant_promotions_fifo
                     ON pending_workspace_grant_promotions(created_at_ms, command_id);
                 CREATE TABLE native_command_owners (
                     tool_call_id TEXT NOT NULL PRIMARY KEY CHECK(length(tool_call_id)=32),
                     owner_id TEXT NOT NULL CHECK(length(owner_id)=32),
                     result_saved INTEGER NOT NULL DEFAULT 0 CHECK(result_saved IN (0,1))
                 ) STRICT;
                 CREATE INDEX native_command_ack_pending ON native_command_owners(owner_id) WHERE result_saved=1;
                 ALTER TABLE sessions ADD COLUMN approval_delegate TEXT;
                 ALTER TABLE tool_calls ADD COLUMN approval_delegated INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE tool_calls ADD COLUMN approval_escalation_json TEXT CHECK(approval_escalation_json IS NULL OR (json_valid(approval_escalation_json) AND length(CAST(approval_escalation_json AS BLOB))<=8192));
                 DROP TRIGGER IF EXISTS tool_calls_clear_approval_inputs;
                 CREATE TRIGGER tool_calls_clear_approval_inputs AFTER UPDATE OF state,approval_resolution ON tool_calls
                 WHEN (OLD.approval_inputs_json IS NOT NULL OR OLD.approval_escalation_json IS NOT NULL)
                     AND (NEW.state!='awaiting_approval' OR NEW.approval_resolution IS NOT NULL)
                 BEGIN UPDATE tool_calls SET approval_inputs_json=NULL,approval_escalation_json=NULL WHERE id=NEW.id; END;
                 UPDATE sessions SET approval_delegate='off';",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_grants(session_id,kind,value,created_at_ms)
                 VALUES (?1,'shell_prefix','cargo test',1)",
                [session_id.to_string()],
            )
            .unwrap();
        for (call, ordinal, state, resolution) in [
            (approved, 1, "completed", "approved_for_session"),
            (denied, 2, "denied", "denied_by_reviewer"),
        ] {
            connection
                .execute(
                    "INSERT INTO tool_calls(
                         id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                         arguments_json, state, result, is_error, requested_at_ms,
                         approval_resolution, approval_delegated
                     ) VALUES (?1, ?2, 1, ?3, ?4, 'shell', '{\"command\":\"cargo test\"}',
                               ?5, 'done', ?6, 1, ?7, 1)",
                    params![
                        call.to_string(),
                        run_id,
                        ordinal,
                        format!("call_{ordinal}"),
                        state,
                        state == "denied",
                        resolution
                    ],
                )
                .unwrap();
        }
        connection
            .execute_batch(&format!(
                "UPDATE events SET envelope_json=json_set(envelope_json,'$.event.session.approval_delegate','off')
                  WHERE json_extract(envelope_json,'$.event.session') IS NOT NULL;
                 UPDATE events SET envelope_json=json_set(envelope_json,'$.event.session.pending_holds',
                     json('[{{\"tool_call_id\":\"{approved}\"}},{{\"tool_call_id\":\"{denied}\",\"delegated\":true}}]'))
                  WHERE json_extract(envelope_json,'$.event.type')='session_created';
                 UPDATE runs SET completion_json=json_set(completion_json,'$.event.session.approval_delegate','on',
                     '$.event.session.pending_holds',json('[{{\"tool_call_id\":\"{denied}\",\"delegated\":true}}]'))
                  WHERE completion_json IS NOT NULL;"
            ))
            .unwrap();
        for event in [
            serde_json::json!({
                "type": "tool_approval_escalated",
                "tool_call_id": denied,
                "delegate": "reviewer",
                "reason": "outside the task",
            }),
            serde_json::json!({
                "type": "workspace_grant_promoted",
                "grant": { "type": "shell_prefix", "prefix": "cargo test" },
                "outcome": { "type": "written", "path": "/repo/.qq/config.ron" },
            }),
        ] {
            let sequence: u64 = connection
                .query_row(
                    "UPDATE workspaces SET next_sequence=next_sequence+1 WHERE id=?1
                     RETURNING next_sequence",
                    [workspace_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let envelope = serde_json::json!({
                "cursor": { "store_id": store_id, "workspace_id": workspace_id, "sequence": sequence },
                "session_id": session_id,
                "run_id": run_id,
                "occurred_at_ms": 1,
                "event": event,
            });
            connection
                .execute(
                    "INSERT INTO events(workspace_id,sequence,envelope_json) VALUES (?1,?2,?3)",
                    params![workspace_id.to_string(), sequence, envelope.to_string()],
                )
                .unwrap();
        }
        connection
            .execute(
                "UPDATE metadata SET value='64' WHERE key='schema_version'",
                [],
            )
            .unwrap();
    }

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    let start = EventCursor {
        store_id,
        workspace_id,
        sequence: 0,
    };
    let mut events = subscribe_after(&runtime, workspace_id, start);
    let replayed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut replayed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let last = event.cursor.sequence == snapshot.cursor.sequence;
            replayed.push(event);
            if last {
                break;
            }
        }
        replayed
    })
    .await
    .unwrap();
    assert!(
        replayed.iter().any(
            |envelope| matches!(&envelope.event, SessionEvent::SessionCreated { session }
                if session.pending_holds.len() == 2)
        ),
        "the journaled summary keeps its holds without the delegate flag"
    );
    assert_eq!(
        replayed
            .iter()
            .filter(|envelope| matches!(envelope.event, SessionEvent::Retired))
            .count(),
        2,
        "escalation and promotion events replay as retired"
    );
    let completions = runtime
        .completed_runs(session_id, start, snapshot.cursor)
        .await
        .unwrap();
    assert_eq!(completions.len(), 1, "the receipt survives");
    runtime.close().await.unwrap();

    let connection = rusqlite::Connection::open(&path).unwrap();
    let version: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, STORE_SCHEMA_VERSION.to_string());
    for table in [
        "session_grants",
        "pending_workspace_grant_promotions",
        "native_command_owners",
    ] {
        let present: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!present, "{table} is dropped");
    }
    assert!(!has_column(&connection, "sessions", "approval_delegate").unwrap());
    for column in ["approval_delegated", "approval_escalation_json"] {
        assert!(!has_column(&connection, "tool_calls", column).unwrap());
    }
    let remaining: u64 = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM events WHERE instr(envelope_json,'\"approval_delegate\"')>0
                         OR instr(envelope_json,'\"delegated\"')>0)
                  + (SELECT COUNT(*) FROM runs WHERE instr(completion_json,'\"approval_delegate\"')>0
                         OR instr(completion_json,'\"delegated\"')>0)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "removed summary fields are stripped");
    for (call, expected) in [
        (approved, ApprovalResolution::ApprovedOnce),
        (denied, ApprovalResolution::Denied),
    ] {
        let stored: String = connection
            .query_row(
                "SELECT approval_resolution FROM tool_calls WHERE id=?1",
                [call.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parse_approval_resolution(&stored).unwrap(), expected);
    }
}
