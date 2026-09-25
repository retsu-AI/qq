use super::*;

#[tokio::test]
async fn prune_deletes_only_idle_sessions_without_messages() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let kept = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id: kept } = kept.outcome else {
        panic!("unexpected receipt")
    };
    let mut empties = Vec::new();
    for _ in 0..2 {
        let CommandOutcome::SessionCreated { session_id } =
            create_session(&runtime, workspace_id, None).await.outcome
        else {
            panic!("unexpected receipt")
        };
        empties.push(session_id);
    }
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: kept,
                input: vec![InputPart::text("keep me".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: queued.committed_through,
        })
        .unwrap();
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

    // One SessionDeleted per victim; the prompted session survives.
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
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 8,
        })
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

    // A second prune finds nothing left to delete.
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
fn version_one_migration_is_atomic_and_marks_historical_cost_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata VALUES ('schema_version', '1');
             CREATE TABLE workspaces (
                 id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                 next_sequence INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 parent_id TEXT REFERENCES sessions(id),
                 title TEXT NOT NULL, status TEXT NOT NULL, active_run_id TEXT,
                 queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                 max_output_tokens INTEGER, organization TEXT,
                 created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO sessions VALUES (
                 'old', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                 'openai/gpt-test', 100, NULL, 1, 1
             );
             CREATE TABLE runs (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                 command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                 assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                 cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                 created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                 finished_at_ms INTEGER
             );",
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
        "36"
    );
    assert!(
        !connection
            .query_row(
                "SELECT cost_known FROM sessions WHERE id = 'old'",
                [],
                |row| { row.get::<_, bool>(0) }
            )
            .unwrap()
    );
    assert!(has_column(&connection, "runs", "usage_json").unwrap());
    assert!(has_column(&connection, "tool_calls", "provider_call_id").unwrap());
    assert!(has_column(&connection, "model_turns", "assistant_content_json").unwrap());
    assert!(has_column(&connection, "model_turns", "model_json").unwrap());
    assert!(has_column(&connection, "model_turns", "usage_json").unwrap());
    assert!(has_column(&connection, "model_turns", "estimated_cost_usd_nanos").unwrap());
    assert!(has_column(&connection, "model_turns", "completed_at_ms").unwrap());
    assert!(has_column(&connection, "tool_calls", "approval_resolution").unwrap());
    assert!(has_column(&connection, "tool_calls", "display_json").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT approval_mode FROM sessions WHERE id = 'old'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "ask"
    );
    assert!(has_column(&connection, "session_grants", "value").unwrap());
    assert!(has_column(&connection, "session_files", "content_hash").unwrap());
    assert!(has_column(&connection, "messages", "turn_ordinal").unwrap());
}

#[test]
fn version_five_migration_defaults_existing_messages_to_turn_zero() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    {
        // A version-5 store whose messages table predates turn_ordinal,
        // holding one completed legacy run.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '5');
                 CREATE TABLE workspaces (
                     id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                     next_sequence INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY,
                     workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                     parent_id TEXT REFERENCES sessions(id),
                     title TEXT NOT NULL, status TEXT NOT NULL, active_run_id TEXT,
                     queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                     max_output_tokens INTEGER, organization TEXT,
                     approval_mode TEXT NOT NULL DEFAULT 'ask',
                     estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                     cost_known INTEGER NOT NULL DEFAULT 1,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                 );
                 INSERT INTO sessions VALUES (
                     'session', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                     'openai/gpt-test', 100, NULL, 'ask', 0, 1, 1, 1
                 );
                 CREATE TABLE runs (
                     id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                     command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                     assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                     cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                     usage_json TEXT, estimated_cost_usd_nanos INTEGER,
                     created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                     finished_at_ms INTEGER
                 );
                 INSERT INTO runs VALUES (
                     'run', 'session', 'command', 'user-message', 'assistant-message',
                     'completed', 0, NULL, NULL, NULL, 1, 1, 2
                 );
                 CREATE TABLE messages (
                     id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES sessions(id),
                     run_id TEXT NOT NULL REFERENCES runs(id),
                     ordinal INTEGER NOT NULL, role TEXT NOT NULL, state TEXT NOT NULL,
                     output TEXT NOT NULL DEFAULT '', refusal TEXT NOT NULL DEFAULT '',
                     created_at_ms INTEGER NOT NULL,
                     UNIQUE(session_id, ordinal)
                 );
                 INSERT INTO messages VALUES (
                     'user-message', 'session', 'run', 1, 'user', 'complete', 'hi', '', 1
                 );
                 INSERT INTO messages VALUES (
                     'assistant-message', 'session', 'run', 2, 'assistant', 'complete',
                     'hello', '', 1
                 );
                 CREATE TABLE tool_calls (
                     id TEXT PRIMARY KEY,
                     run_id TEXT NOT NULL REFERENCES runs(id),
                     turn_ordinal INTEGER NOT NULL,
                     call_ordinal INTEGER NOT NULL,
                     provider_call_id TEXT NOT NULL,
                     name TEXT NOT NULL,
                     arguments_json TEXT NOT NULL,
                     state TEXT NOT NULL,
                     result TEXT,
                     is_error INTEGER NOT NULL DEFAULT 0,
                     approval_resolution TEXT,
                     requested_at_ms INTEGER NOT NULL,
                     started_at_ms INTEGER,
                     resolved_at_ms INTEGER,
                     finished_at_ms INTEGER
                 );",
            )
            .unwrap();
    }

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
    assert!(has_column(&connection, "tool_calls", "display_json").unwrap());
    let (turn_ordinal, output, state) = connection
        .query_row(
            "SELECT turn_ordinal, output, state FROM messages WHERE id = 'assistant-message'",
            [],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(turn_ordinal, 0);
    assert_eq!(output, "hello");
    assert_eq!(state, "complete");
}

#[test]
fn version_six_migration_adds_the_display_column_and_keeps_existing_calls_bare() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    {
        // A version-6 store whose tool_calls table predates display_json,
        // holding one completed edit call.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '6');
                 CREATE TABLE tool_calls (
                     id TEXT PRIMARY KEY,
                     run_id TEXT NOT NULL,
                     turn_ordinal INTEGER NOT NULL,
                     call_ordinal INTEGER NOT NULL,
                     provider_call_id TEXT NOT NULL,
                     name TEXT NOT NULL,
                     arguments_json TEXT NOT NULL,
                     state TEXT NOT NULL,
                     result TEXT,
                     is_error INTEGER NOT NULL DEFAULT 0,
                     approval_resolution TEXT,
                     requested_at_ms INTEGER NOT NULL,
                     started_at_ms INTEGER,
                     resolved_at_ms INTEGER,
                     finished_at_ms INTEGER
                 );
                 INSERT INTO tool_calls VALUES (
                     'call', 'run', 1, 1, 'call_0', 'edit_file', '{}',
                     'completed', 'Edited note.txt: replaced 1 occurrence(s).',
                     0, NULL, 1, 1, NULL, 2
                 );",
            )
            .unwrap();
    }

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
    let (display_json, result) = connection
        .query_row(
            "SELECT display_json, result FROM tool_calls WHERE id = 'call'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(display_json, None);
    assert_eq!(
        result.as_deref(),
        Some("Edited note.txt: replaced 1 occurrence(s).")
    );
}

#[test]
fn version_seven_migration_adds_compaction_storage_and_run_kinds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    {
        // A version-7 store whose runs table predates internal run kinds
        // and that has no compaction storage.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '7');
                 CREATE TABLE runs (
                     id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                     command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                     assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                     cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                     usage_json TEXT, estimated_cost_usd_nanos INTEGER,
                     created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                     finished_at_ms INTEGER
                 );
                 INSERT INTO runs VALUES (
                     'run', 'session', 'command', 'user', 'assistant',
                     'completed', 0, NULL, NULL, NULL, 1, 1, 2
                 );",
            )
            .unwrap();
    }

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
    assert!(has_column(&connection, "runs", "kind").unwrap());
    assert_eq!(
        connection
            .query_row("SELECT kind FROM runs WHERE id = 'run'", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "prompt"
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM session_compactions", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[test]
fn version_ten_migration_adds_context_and_child_ownership_without_guessing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '10');
                 CREATE TABLE workspaces (
                     id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                     next_sequence INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                     parent_id TEXT REFERENCES sessions(id), title TEXT NOT NULL,
                     status TEXT NOT NULL, active_run_id TEXT,
                     queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                     max_output_tokens INTEGER, organization TEXT,
                     approval_mode TEXT NOT NULL DEFAULT 'ask',
                     estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                     cost_known INTEGER NOT NULL DEFAULT 1,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                 );
                 INSERT INTO sessions VALUES (
                     'session', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                     'openai/gpt-test', 100, NULL, 'ask', 0, 1, 1, 1
                 );",
            )
            .unwrap();
    }

    let (connection, _) = open_database(&path).unwrap();

    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
    assert!(has_column(&connection, "sessions", "context_tokens").unwrap());
    assert!(has_column(&connection, "sessions", "owner_run_id").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT context_tokens FROM sessions WHERE id = 'session'",
                [],
                |row| row.get::<_, Option<u64>>(0),
            )
            .unwrap(),
        None
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT owner_run_id FROM sessions WHERE id = 'session'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
    connection
        .execute(
            "UPDATE sessions SET context_tokens = 12_500 WHERE id = 'session'",
            [],
        )
        .unwrap();
    drop(connection);

    let (reopened, _) = open_database(&path).unwrap();
    assert_eq!(
        reopened
            .query_row(
                "SELECT context_tokens FROM sessions WHERE id = 'session'",
                [],
                |row| row.get::<_, Option<u64>>(0),
            )
            .unwrap(),
        Some(12_500)
    );
}

#[test]
fn version_eleven_migration_adds_child_ownership_and_preserves_context() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '11');
                 CREATE TABLE workspaces (
                     id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                     next_sequence INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                     parent_id TEXT REFERENCES sessions(id), title TEXT NOT NULL,
                     status TEXT NOT NULL, active_run_id TEXT,
                     queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                     max_output_tokens INTEGER, organization TEXT,
                     approval_mode TEXT NOT NULL DEFAULT 'ask', context_tokens INTEGER,
                     estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                     cost_known INTEGER NOT NULL DEFAULT 1,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                 );
                 INSERT INTO sessions VALUES (
                     'session', 'workspace', NULL, 'Existing', 'idle', NULL, 0,
                     'openai/gpt-test', 100, NULL, 'ask', 777, 0, 1, 1, 1
                 );",
            )
            .unwrap();
    }

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
    assert!(has_column(&connection, "sessions", "owner_run_id").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT context_tokens, owner_run_id FROM sessions WHERE id = 'session'",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap(),
        (777, None)
    );
}

#[test]
fn version_twelve_migration_adds_prompt_identity_without_guessing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '12');
                 CREATE TABLE runs (
                     id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                     command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                     assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                     kind TEXT NOT NULL DEFAULT 'prompt',
                     auto_compaction INTEGER NOT NULL DEFAULT 0,
                     cancel_requested INTEGER NOT NULL DEFAULT 0,
                     outcome_json TEXT, usage_json TEXT, context_tokens INTEGER,
                     estimated_cost_usd_nanos INTEGER, created_at_ms INTEGER NOT NULL,
                     started_at_ms INTEGER, finished_at_ms INTEGER
                 );
                 INSERT INTO runs(
                     id, session_id, command_id, user_message_id,
                     assistant_message_id, status, created_at_ms, finished_at_ms
                 ) VALUES (
                     'run', 'session', 'command', 'user', 'assistant',
                     'completed', 1, 2
                 );",
            )
            .unwrap();
    }

    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
    assert!(has_column(&connection, "runs", "prompt_identity_json").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT prompt_identity_json FROM runs WHERE id = 'run'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
}

#[test]
fn version_thirteen_migration_adds_per_turn_audit_columns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata VALUES ('schema_version', '13');
             CREATE TABLE model_turns (
                 run_id TEXT NOT NULL,
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 PRIMARY KEY(run_id, turn_ordinal)
             );",
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
        "36"
    );
    for column in [
        "model_json",
        "usage_json",
        "estimated_cost_usd_nanos",
        "completed_at_ms",
    ] {
        assert!(
            has_column(&connection, "model_turns", column).unwrap(),
            "{column}"
        );
    }
}

#[test]
fn version_fourteen_migration_adds_chunks_and_incremental_capacity_columns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata VALUES ('schema_version', '14');
             CREATE TABLE runs (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 command_id TEXT NOT NULL UNIQUE,
                 user_message_id TEXT NOT NULL,
                 assistant_message_id TEXT NOT NULL,
                 status TEXT NOT NULL,
                 kind TEXT NOT NULL DEFAULT 'prompt',
                 auto_compaction INTEGER NOT NULL DEFAULT 0,
                 cancel_requested INTEGER NOT NULL DEFAULT 0,
                 prompt_identity_json TEXT,
                 outcome_json TEXT,
                 usage_json TEXT,
                 context_tokens INTEGER,
                 estimated_cost_usd_nanos INTEGER,
                 created_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 finished_at_ms INTEGER
             );
             CREATE TABLE model_turns (
                 run_id TEXT NOT NULL,
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 model_json TEXT,
                 usage_json TEXT,
                 estimated_cost_usd_nanos INTEGER,
                 completed_at_ms INTEGER,
                 PRIMARY KEY(run_id, turn_ordinal)
             );",
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
        "36"
    );
    assert!(has_column(&connection, "message_chunks", "chunk_ordinal").unwrap());
    assert!(has_column(&connection, "message_chunks", "text").unwrap());
    assert!(has_column(&connection, "runs", "context_base_bytes").unwrap());
    assert!(has_column(&connection, "runs", "context_increment_bytes").unwrap());
    assert!(
        has_column(
            &connection,
            "pending_workspace_grant_promotions",
            "promotion_json"
        )
        .unwrap()
    );
}

#[test]
fn version_fourteen_store_with_implicit_primary_key_outbox_migrates() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata VALUES ('schema_version', '14');
             CREATE TABLE runs (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 command_id TEXT NOT NULL UNIQUE,
                 user_message_id TEXT NOT NULL,
                 assistant_message_id TEXT NOT NULL,
                 status TEXT NOT NULL,
                 kind TEXT NOT NULL DEFAULT 'prompt',
                 auto_compaction INTEGER NOT NULL DEFAULT 0,
                 cancel_requested INTEGER NOT NULL DEFAULT 0,
                 prompt_identity_json TEXT,
                 outcome_json TEXT,
                 usage_json TEXT,
                 context_tokens INTEGER,
                 estimated_cost_usd_nanos INTEGER,
                 created_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 finished_at_ms INTEGER
             );
             CREATE TABLE model_turns (
                 run_id TEXT NOT NULL,
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 model_json TEXT,
                 usage_json TEXT,
                 estimated_cost_usd_nanos INTEGER,
                 completed_at_ms INTEGER,
                 PRIMARY KEY(run_id, turn_ordinal)
             );
             CREATE TABLE pending_workspace_grant_promotions (
                 command_id TEXT PRIMARY KEY,
                 created_at_ms INTEGER NOT NULL,
                 promotion_json TEXT NOT NULL
             );
             CREATE INDEX pending_workspace_grant_promotions_fifo
                 ON pending_workspace_grant_promotions(created_at_ms, command_id);",
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
        "36"
    );
    let command_id_not_null: bool = connection
        .query_row(
            "SELECT [notnull] FROM pragma_table_info('pending_workspace_grant_promotions')
             WHERE name = 'command_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(command_id_not_null);
}

#[test]
fn partially_applied_version_fourteen_linear_migration_completes_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '14' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute("ALTER TABLE runs DROP COLUMN context_increment_bytes", [])
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
        "36"
    );
    assert!(has_column(&connection, "message_chunks", "text").unwrap());
    assert!(has_column(&connection, "runs", "context_base_bytes").unwrap());
    assert!(has_column(&connection, "runs", "context_increment_bytes").unwrap());
}

#[test]
fn version_fifteen_migration_keeps_historical_resolved_model_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let command_id = CommandId::generate().unwrap();
    let user_message_id = MessageId::generate().unwrap();
    let assistant_message_id = MessageId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/legacy-resolved-model')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, title, status, approval_mode,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'Legacy', 'idle', 'ask', 1, 2)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id,
                 assistant_message_id, status, outcome_json,
                 created_at_ms, started_at_ms, finished_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'completed', ?6, 1, 1, 2)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                command_id.to_string(),
                user_message_id.to_string(),
                assistant_message_id.to_string(),
                serde_json::to_string(&RunOutcome::Completed).unwrap(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '15' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute("ALTER TABLE runs DROP COLUMN resolved_model_json", [])
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
        "36"
    );
    assert!(has_column(&connection, "runs", "resolved_model_json").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT resolved_model_json FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
    assert_eq!(load_run(&connection, run_id).unwrap().resolved_model, None);
}

#[test]
fn version_seventeen_migration_adds_preparation_and_exact_compaction_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v16-preparation')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, title, status, queued_prompts, approval_mode,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'Queued', 'queued', 1, 'ask', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::generate().unwrap().to_string(),
                MessageId::generate().unwrap().to_string(),
                MessageId::generate().unwrap().to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '16' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute("ALTER TABLE sessions DROP COLUMN preparing_run_id", [])
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN pending_context_overflow_model_json",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs DROP COLUMN context_compaction_attempted",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs DROP COLUMN auto_compaction_for_run_id",
            [],
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
        "36"
    );
    let preparing_shape: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT type, [notnull], dflt_value
             FROM pragma_table_info('sessions') WHERE name = 'preparing_run_id'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(preparing_shape, ("TEXT".to_owned(), false, None));
    let overflow_shape: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT type, [notnull], dflt_value
             FROM pragma_table_info('sessions')
             WHERE name = 'pending_context_overflow_model_json'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(overflow_shape, ("TEXT".to_owned(), false, None));
    let attempted_shape: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT type, [notnull], dflt_value
             FROM pragma_table_info('runs') WHERE name = 'context_compaction_attempted'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        attempted_shape,
        ("INTEGER".to_owned(), true, Some("0".to_owned()))
    );
    let row: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT status, context_compaction_attempted,
                    auto_compaction_for_run_id
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("queued".to_owned(), false, None));
    let (preparing, pending_overflow): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT preparing_run_id, pending_context_overflow_model_json
             FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(preparing, None);
    assert_eq!(pending_overflow, None);
}

#[test]
fn version_eighteen_migration_adds_unknown_occupancy_basis_without_losing_the_meter() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let mut legacy_model = test_resolved_model("test/model", "test-model", 256, None);
    legacy_model.version = qq_protocol::ResolvedModelVersion::new(1).unwrap();
    legacy_model.request_shape = None;
    let legacy_overflow = serde_json::to_string(&legacy_model).unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v17-occupancy')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, title, status, context_tokens,
                 pending_context_overflow_model_json,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'Measured', 'idle', 12500, ?3, 1, 1)",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                legacy_overflow,
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '17' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN context_occupancy_json",
            [],
        )
        .unwrap();
    if has_column(
        &connection,
        "sessions",
        "pending_context_overflow_basis_json",
    )
    .unwrap()
    {
        connection
            .execute(
                "ALTER TABLE sessions DROP COLUMN pending_context_overflow_basis_json",
                [],
            )
            .unwrap();
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
        "36"
    );
    let occupancy_shape: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT type, [notnull], dflt_value
             FROM pragma_table_info('sessions')
             WHERE name = 'context_occupancy_json'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(occupancy_shape, ("TEXT".to_owned(), false, None));
    let overflow_basis_shape: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT type, [notnull], dflt_value
             FROM pragma_table_info('sessions')
             WHERE name = 'pending_context_overflow_basis_json'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(overflow_basis_shape, ("TEXT".to_owned(), false, None));
    let measured: (u64, Option<String>, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT context_tokens, context_occupancy_json,
                    pending_context_overflow_basis_json,
                    pending_context_overflow_model_json
             FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(measured.0, 12_500);
    assert_eq!(measured.1, None);
    assert_eq!(measured.2, None);
    assert_eq!(
        serde_json::from_str::<ResolvedModel>(measured.3.as_deref().unwrap()).unwrap(),
        legacy_model
    );
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

#[test]
fn partially_applied_version_eighteen_migration_completes_atomically() {
    for missing in [
        "context_occupancy_json",
        "pending_context_overflow_basis_json",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (connection, _) = open_database(&path).unwrap();
        connection
            .execute(
                "UPDATE metadata SET value = '17' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        connection
            .execute(&format!("ALTER TABLE sessions DROP COLUMN {missing}"), [])
            .unwrap();
        drop(connection);

        let (connection, _) = open_database(&path).unwrap();
        assert!(has_column(&connection, "sessions", "context_occupancy_json").unwrap());
        assert!(
            has_column(
                &connection,
                "sessions",
                "pending_context_overflow_basis_json"
            )
            .unwrap()
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "36"
        );
    }
}

#[test]
fn failed_version_eighteen_validation_rolls_back_schema_and_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '17' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN pending_context_overflow_basis_json",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN context_occupancy_json",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions ADD COLUMN context_occupancy_json
             INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "17"
    );
    assert!(
        !has_column(
            &connection,
            "sessions",
            "pending_context_overflow_basis_json"
        )
        .unwrap()
    );
}

#[test]
fn version_twenty_migration_adds_spawn_call_ownership_without_guessing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let parent_id = SessionId::generate().unwrap();
    let child_id = SessionId::generate().unwrap();
    let owner_run = RunId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v19-spawn')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'Parent', 'idle', 1, 1)",
            params![parent_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, parent_id, owner_run_id, title, status,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 'Child', 'idle', 2, 2)",
            params![
                child_id.to_string(),
                workspace_id.to_string(),
                parent_id.to_string(),
                owner_run.to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '19' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN spawned_by_tool_call_id",
            [],
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
        "36"
    );
    // A historical child keeps its parent run but has no recorded call:
    // the summary says so explicitly instead of inventing one.
    let child = load_session_summary(&connection, child_id).unwrap();
    assert_eq!(
        child.spawned_by,
        Some(SpawnOrigin {
            run_id: owner_run,
            tool_call_id: None,
            depth: 1,
        })
    );
    let parent = load_session_summary(&connection, parent_id).unwrap();
    assert_eq!(parent.spawned_by, None);
    assert_eq!(parent.activity, None);
}

/// Schema 25: `runs.activity` is added null and the command counter is
/// backfilled from the journal, so a schema-24 store reopens with its
/// summaries intact and its command bound still enforced.
#[test]
fn version_twenty_five_migration_adds_activity_and_backfills_the_command_counter() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v24-fast-path')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, active_run_id,
                                  created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'Old', 'running', ?3, 1, 1)",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                run_id.to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, created_at_ms
             ) VALUES (?1, ?2, 'cmd', 'u', 'a', 'running', 1)",
            params![run_id.to_string(), session_id.to_string()],
        )
        .unwrap();
    for index in 0..3 {
        connection
            .execute(
                "INSERT INTO commands(id, request_json, receipt_json) VALUES (?1, '{}', '{}')",
                [format!("cmd-{index}")],
            )
            .unwrap();
    }
    for statement in [
        "UPDATE metadata SET value = '24' WHERE key = 'schema_version'",
        "DELETE FROM metadata WHERE key = 'command_count'",
        "ALTER TABLE runs DROP COLUMN activity",
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
        "36"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'command_count'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "3",
        "the counter is backfilled from the journal"
    );
    let activity: Option<String> = connection
        .query_row(
            "SELECT activity FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(activity, None);
    // A historical running run reports no activity, which is what the
    // event-log scan reported for a run with no activity events.
    let summary = load_session_summary(&connection, session_id).unwrap();
    assert_eq!(summary.active_run_id, Some(run_id));
    assert_eq!(summary.activity, None);
}

/// Every `PersistenceFault` variant is reachable from a real store fault:
/// a `RAISE(ABORT)` trigger is a constraint, a row that no longer decodes
/// is a codec fault, and a read-only database keeps its SQLite code.
#[tokio::test]
async fn every_persistence_fault_variant_is_reachable() {
    let (directory, store, claimed) = claimed_store_fixture().await;
    // Constraint: a trigger rejects the write.
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
        .append_run_activity(&claimed, RunActivity::WaitingForProvider)
        .await
        .unwrap_err();
    assert_eq!(
        constraint,
        SessionRuntimeError::Persistence(PersistenceFault::Constraint)
    );
    assert!(constraint.to_string().contains("invariant"), "{constraint}");
    // Codec: the run's limits no longer decode.
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
    // Sqlite: the connection is read-only, so a write keeps the code.
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
        "36"
    );
    // A historical call has no recorded effect: assembly falls back to
    // the name rather than guessing a class for it.
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
        "36"
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
            "UPDATE metadata SET value = '27' WHERE key = 'schema_version';
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
        "36"
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
        "36"
    );
    for table in ["attachment_blobs", "message_attachments"] {
        let rows: u32 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table}");
    }
    // A store that stops at 28 reopens: nothing in 29 rewrites older rows.
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
            "UPDATE metadata SET value = '29' WHERE key = 'schema_version';
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
        "36"
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

    // Same transaction: the result row and the spill row exist together
    // and the persisted result cites the handle.
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

    // Exact, unmasked bytes come back for the owning session.
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
    // A digest that disagrees is not this output.
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
    // Another session holding the handle reads nothing.
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
    // Three spills of 30 MiB against a 64 MiB cap: the third evicts the
    // first; its row and handle stay, its content does not.
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

/// D6: activity is a column written with its event, and the summary reads
/// D6: activity is a column written with its event and the summary reads
/// the column; the command counter tracks the journal exactly.
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
    // The run is parked at an approval. The last RunActivityChanged the
    // subscriber saw, the column, and the summary published by the next
    // command must all agree.
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
        .inner
        .store
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
fn version_twenty_two_migration_adds_truncation_state_as_never_truncated() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let message_id = MessageId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v21-truncation')",
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
             VALUES (?1, ?2, ?3, 1, 1, 'assistant', 'complete', 'legacy', 1)",
            params![
                message_id.to_string(),
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
        "UPDATE metadata SET value = '21' WHERE key = 'schema_version'",
        "ALTER TABLE messages DROP COLUMN truncated",
        "ALTER TABLE model_turns DROP COLUMN truncated",
        "ALTER TABLE runs DROP COLUMN output_continuations",
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
        "36"
    );
    let message = load_message(&connection, message_id).unwrap();
    assert!(!message.truncated);
    assert_eq!(message.output, "legacy");
    let (truncated, continuations): (bool, u16) = connection
        .query_row(
            "SELECT t.truncated, r.output_continuations
             FROM model_turns t JOIN runs r ON r.id = t.run_id WHERE r.id = ?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(!truncated);
    assert_eq!(continuations, 0);
    // Historical context replays without a continuation notice.
    let (ordinal, content_json, truncated): (u32, String, bool) = connection
        .query_row(
            "SELECT turn_ordinal, assistant_content_json, truncated FROM model_turns
             WHERE run_id = ?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let mut context = Vec::new();
    append_run_turns(
        vec![(ordinal, content_json, truncated)],
        HashMap::new(),
        std::collections::VecDeque::new(),
        None,
        &mut context,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(context.len(), 1);
    assert_eq!(context[0].role(), Role::Assistant);
}

#[test]
fn version_nineteen_migration_keeps_historical_runs_unlimited() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/v18-limits')",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'Historical', 'idle', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, outcome_json, created_at_ms
             ) VALUES (?1, ?2, 'cmd', 'user', 'assistant', 'completed',
                       '{\"type\":\"completed\"}', 1)",
            params![run_id.to_string(), session_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '18' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute("ALTER TABLE runs DROP COLUMN limits_json", [])
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
        "36"
    );
    let shape: (String, bool, Option<String>) = connection
        .query_row(
            "SELECT type, [notnull], dflt_value FROM pragma_table_info('runs')
             WHERE name = 'limits_json'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(shape, ("TEXT".to_owned(), false, None));
    let historical = load_run(&connection, run_id).unwrap();
    assert_eq!(historical.limits, None);
    assert_eq!(historical.outcome, Some(RunOutcome::Completed));

    // A malformed shape and a malformed stored row are both persistence
    // faults, never silently reinterpreted under current defaults.
    connection
        .execute(
            "UPDATE runs SET limits_json = '{not-json' WHERE id = ?1",
            [run_id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        load_run(&connection, run_id),
        Err(SessionRuntimeError::CODEC)
    ));
    connection
        .execute("ALTER TABLE runs DROP COLUMN limits_json", [])
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs ADD COLUMN limits_json INTEGER NOT NULL DEFAULT 0",
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
fn partially_applied_version_seventeen_migration_completes_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '16' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs DROP COLUMN auto_compaction_for_run_id",
            [],
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    assert!(has_column(&connection, "sessions", "preparing_run_id").unwrap());
    assert!(
        has_column(
            &connection,
            "sessions",
            "pending_context_overflow_model_json"
        )
        .unwrap()
    );
    assert!(has_column(&connection, "runs", "context_compaction_attempted").unwrap());
    assert!(has_column(&connection, "runs", "auto_compaction_for_run_id").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "36"
    );
}

/// Regression: a fully migrated store must reopen. Every earlier migration
/// step lists the versions it is already applied to; forgetting the current one
/// re-runs `ALTER TABLE ... ADD COLUMN` on reopen and SQLite rejects it.
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

/// Schema 35 marks who recorded each session grant. A pre-35 store's rows are
/// all `human`; the delegate columns arrive with their defaults and a column
/// already present with the wrong shape is refused.
#[test]
fn version_thirty_four_gains_grant_provenance_and_keeps_old_rows_human() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE session_grants DROP COLUMN source;
             ALTER TABLE session_grants DROP COLUMN run_id;
             INSERT INTO workspaces(id, path) VALUES ('ws', '/tmp/ws');
             INSERT INTO sessions(
                 id, workspace_id, title, status, approval_mode, created_at_ms, updated_at_ms
             ) VALUES ('s1', 'ws', 'old', 'idle', 'ask', 0, 0);
             INSERT INTO session_grants(session_id, kind, value, created_at_ms)
                 VALUES ('s1', 'shell_prefix', 'git push --force', 0);
             UPDATE metadata SET value = '34' WHERE key = 'schema_version';",
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
        "36"
    );
    let (source, run_id): (String, Option<String>) = connection
        .query_row(
            "SELECT source, run_id FROM session_grants WHERE session_id = 's1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(source, "human", "every pre-35 grant was a human grant");
    assert_eq!(run_id, None);
    connection
        .execute_batch(
            "ALTER TABLE session_grants DROP COLUMN source;
             ALTER TABLE session_grants ADD COLUMN source INTEGER NOT NULL DEFAULT 0;",
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}

/// Schema 34 adds the nullable effort pin to a pre-34 store and refuses a
/// column that already exists with the wrong shape.
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
        "36"
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
fn failed_version_seventeen_validation_rolls_back_schema_and_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '16' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE sessions DROP COLUMN pending_context_overflow_model_json",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs DROP COLUMN context_compaction_attempted",
            [],
        )
        .unwrap();
    connection
        .execute(
            "ALTER TABLE runs ADD COLUMN context_compaction_attempted TEXT",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "16"
    );
    assert!(
        !has_column(
            &connection,
            "sessions",
            "pending_context_overflow_model_json"
        )
        .unwrap()
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT type FROM pragma_table_info('runs')
                 WHERE name = 'context_compaction_attempted'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "TEXT"
    );
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
fn failed_version_fourteen_capacity_validation_never_advances_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = '14' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(connection);
    rewrite_run_capacity_schema(
        &path,
        "context_base_bytes INTEGER",
        "context_increment_bytes INTEGER",
    );

    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "14"
    );
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
fn version_fifteen_store_with_malformed_promotion_outbox_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute(
            "ALTER TABLE pending_workspace_grant_promotions DROP COLUMN promotion_json",
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
fn version_fifteen_store_with_implicit_primary_key_outbox_migrates() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE pending_workspace_grant_promotions;
             CREATE TABLE pending_workspace_grant_promotions (
                 command_id TEXT PRIMARY KEY,
                 created_at_ms INTEGER NOT NULL,
                 promotion_json TEXT NOT NULL
             );
             CREATE INDEX pending_workspace_grant_promotions_fifo
                 ON pending_workspace_grant_promotions(created_at_ms, command_id);
             INSERT INTO pending_workspace_grant_promotions
             VALUES ('legacy-command', 1, '{}');",
        )
        .unwrap();
    drop(connection);

    let (connection, _) = open_database(&path).unwrap();
    let command_id_not_null: bool = connection
        .query_row(
            "SELECT [notnull] FROM pragma_table_info('pending_workspace_grant_promotions')
             WHERE name = 'command_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(command_id_not_null);
    let promotion_json: String = connection
        .query_row(
            "SELECT promotion_json FROM pending_workspace_grant_promotions
             WHERE command_id = 'legacy-command'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(promotion_json, "{}");
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
fn version_fourteen_migration_preserves_legacy_snapshot_and_context() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (mut connection, store_id) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::from_bytes([1; 16]);
    let session_id = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    let user_message_id = MessageId::from_bytes([4; 16]);
    let assistant_message_id = MessageId::from_bytes([5; 16]);
    connection
        .execute(
            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, model,
                                  created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'Legacy', 'idle', 'test/model', 1, 2)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, outcome_json,
                              created_at_ms, started_at_ms, finished_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'completed', ?6, 1, 1, 2)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::from_bytes([6; 16]).to_string(),
                user_message_id.to_string(),
                assistant_message_id.to_string(),
                serde_json::to_string(&RunOutcome::Completed).unwrap(),
            ],
        )
        .unwrap();
    for (id, ordinal, role, output) in [
        (user_message_id, 1, "user", "legacy prompt"),
        (assistant_message_id, 2, "assistant", "legacy answer"),
    ] {
        connection
            .execute(
                "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                                      output, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'complete', ?6, 1)",
                params![
                    id.to_string(),
                    session_id.to_string(),
                    run_id.to_string(),
                    ordinal,
                    role,
                    output,
                ],
            )
            .unwrap();
    }
    let request = SnapshotRequest {
        workspace_id,
        focused_session_id: Some(session_id),
        include_sessions: Vec::new(),
        session_limit: 1,
        message_limit: 8,
    };
    let before_snapshot = load_snapshot(&mut connection, store_id, request.clone()).unwrap();
    let transaction = connection.transaction().unwrap();
    let before_context = load_model_context(&transaction, session_id, u64::MAX).unwrap();
    transaction.rollback().unwrap();
    connection
        .execute_batch(
            "UPDATE metadata SET value = '14' WHERE key = 'schema_version';
             DROP TABLE message_chunks;
             ALTER TABLE runs DROP COLUMN context_base_bytes;
             ALTER TABLE runs DROP COLUMN context_increment_bytes;",
        )
        .unwrap();
    drop(connection);

    let (mut connection, reopened_store_id) = open_database(&path).unwrap();
    assert_eq!(reopened_store_id, store_id);
    let after_snapshot = load_snapshot(&mut connection, reopened_store_id, request).unwrap();
    let transaction = connection.transaction().unwrap();
    let after_context = load_model_context(&transaction, session_id, u64::MAX).unwrap();

    assert_eq!(after_snapshot, before_snapshot);
    assert_eq!(after_context, before_context);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release-mode R4 diagnostic; run with --release --ignored --nocapture"]
fn r4_append_only_chunk_scaling_diagnostic() {
    const CHILD_BYTES: &str = "QQ_R4_STREAM_DIAGNOSTIC_BYTES";
    const SAMPLES_PER_SIZE: usize = 3;
    if let Some(bytes) = std::env::var_os(CHILD_BYTES) {
        let measurement =
            measure_r4_append_only_stream(bytes.to_str().unwrap().parse::<usize>().unwrap());
        eprintln!(
            "r4_stream bytes={} transactions={} elapsed_ns={} peak_temporary_rss_bytes={}",
            measurement.bytes,
            measurement.transactions,
            measurement.elapsed_ns,
            measurement.peak_temporary_rss_bytes,
        );
        return;
    }

    let executable = std::env::current_exe().unwrap();
    let mut measurements = Vec::new();
    eprintln!("r4_stream samples_per_size={SAMPLES_PER_SIZE}");
    for bytes in [
        64 * 1024,
        512 * 1024,
        1024 * 1024,
        2 * 1024 * 1024,
        4 * 1024 * 1024,
    ] {
        let mut samples = Vec::with_capacity(SAMPLES_PER_SIZE);
        for _ in 0..SAMPLES_PER_SIZE {
            let output = run_r4_stream_diagnostic_child(&executable, bytes, CHILD_BYTES);
            assert!(
                output.status.success(),
                "R4 child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let mut combined = output.stdout;
            combined.extend_from_slice(&output.stderr);
            samples.push(parse_r4_stream_measurement(&combined));
        }
        samples.sort_unstable_by_key(|sample| sample.elapsed_ns);
        let transactions = samples[0].transactions;
        assert!(
            samples
                .iter()
                .all(|sample| { sample.bytes == bytes && sample.transactions == transactions })
        );
        let measurement = R4StreamMeasurement {
            bytes,
            transactions,
            elapsed_ns: samples[SAMPLES_PER_SIZE / 2].elapsed_ns,
            peak_temporary_rss_bytes: samples
                .iter()
                .map(|sample| sample.peak_temporary_rss_bytes)
                .max()
                .unwrap(),
        };
        eprintln!(
            "r4_stream bytes={} transactions={} elapsed_ns={} peak_temporary_rss_bytes={}",
            measurement.bytes,
            measurement.transactions,
            measurement.elapsed_ns,
            measurement.peak_temporary_rss_bytes,
        );
        measurements.push(measurement);
    }
    for pair in measurements[1..].windows(2) {
        let smaller = pair[0].elapsed_ns;
        let larger = pair[1].elapsed_ns;
        assert!(
            larger.saturating_mul(1_000) <= smaller.saturating_mul(2_200),
            "doubling {} to {} bytes exceeded the 2.2x R4 limit: {:?} -> {:?}",
            pair[0].bytes,
            pair[1].bytes,
            pair[0].elapsed_ns,
            pair[1].elapsed_ns,
        );
    }
}

#[test]
fn version_fourteen_store_missing_audit_columns_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata VALUES ('schema_version', '14');
             CREATE TABLE model_turns (
                 run_id TEXT NOT NULL,
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 PRIMARY KEY(run_id, turn_ordinal)
             );",
        )
        .unwrap();
    drop(connection);

    assert_eq!(
        open_database(&path).unwrap_err(),
        SessionRuntimeError::CONSTRAINT
    );
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
            "UPDATE metadata SET value = '30' WHERE key = 'schema_version'",
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
            "UPDATE metadata SET value = '31' WHERE key = 'schema_version'",
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
