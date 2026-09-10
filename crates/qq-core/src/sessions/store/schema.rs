use std::path::PathBuf;

use qq_protocol::StoreId;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::sessions::SessionRuntimeError;

/// Current session store schema, stored as `metadata.schema_version`. Bump it
/// with every migration step appended to `open_database`.
pub const STORE_SCHEMA_VERSION: u16 = 25;

pub(in crate::sessions) fn open_database(
    path: &PathBuf,
) -> Result<(Connection, StoreId), SessionRuntimeError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(SessionRuntimeError::Persistence);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SessionRuntimeError::Persistence),
    }
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| SessionRuntimeError::Persistence)?;
    // Hot statements on the output lane are prepared once per connection.
    connection.set_prepared_statement_cache_capacity(128);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS workspaces (
                 id TEXT PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 next_sequence INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 parent_id TEXT REFERENCES sessions(id),
                 owner_run_id TEXT,
                 spawned_by_tool_call_id TEXT,
                 title TEXT NOT NULL,
                 status TEXT NOT NULL,
                 active_run_id TEXT,
                 preparing_run_id TEXT,
                 pending_context_overflow_model_json TEXT,
                 pending_context_overflow_basis_json TEXT,
                 queued_prompts INTEGER NOT NULL DEFAULT 0,
                 model TEXT,
                 max_output_tokens INTEGER,
                 organization TEXT,
                 approval_mode TEXT NOT NULL DEFAULT 'ask',
                 context_tokens INTEGER,
                 context_occupancy_json TEXT,
                 estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                 cost_known INTEGER NOT NULL DEFAULT 1,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
              CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 command_id TEXT NOT NULL UNIQUE,
                 user_message_id TEXT NOT NULL,
                 assistant_message_id TEXT NOT NULL,
                  status TEXT NOT NULL,
                  kind TEXT NOT NULL DEFAULT 'prompt',
                  auto_compaction INTEGER NOT NULL DEFAULT 0,
                  auto_compaction_for_run_id TEXT,
                  context_compaction_attempted INTEGER NOT NULL DEFAULT 0,
                  cancel_requested INTEGER NOT NULL DEFAULT 0,
                  prompt_identity_json TEXT,
                  resolved_model_json TEXT,
                   context_base_bytes INTEGER,
                   context_increment_bytes INTEGER NOT NULL DEFAULT 0,
                   outcome_json TEXT,
                   usage_json TEXT,
                   context_tokens INTEGER,
                   estimated_cost_usd_nanos INTEGER,
                   limits_json TEXT,
                 created_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 finished_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 ordinal INTEGER NOT NULL,
                 turn_ordinal INTEGER NOT NULL DEFAULT 0,
                 role TEXT NOT NULL,
                 state TEXT NOT NULL,
                 output TEXT NOT NULL DEFAULT '',
                 refusal TEXT NOT NULL DEFAULT '',
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, ordinal)
             );
             CREATE TABLE IF NOT EXISTS message_chunks (
                 message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                 channel TEXT NOT NULL CHECK(channel IN ('output', 'refusal')),
                 chunk_ordinal INTEGER NOT NULL CHECK(chunk_ordinal > 0),
                 text TEXT NOT NULL,
                 PRIMARY KEY(message_id, channel, chunk_ordinal)
             );
             CREATE TABLE IF NOT EXISTS events (
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 sequence INTEGER NOT NULL,
                 envelope_json TEXT NOT NULL,
                 PRIMARY KEY(workspace_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS commands (
                 id TEXT PRIMARY KEY,
                 request_json TEXT NOT NULL,
                 receipt_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS pending_workspace_grant_promotions (
                 command_id TEXT NOT NULL PRIMARY KEY,
                 created_at_ms INTEGER NOT NULL,
                 promotion_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS sessions_workspace_updated
                 ON sessions(workspace_id, updated_at_ms DESC);
              CREATE INDEX IF NOT EXISTS runs_ready
                  ON runs(status, created_at_ms);
              CREATE INDEX IF NOT EXISTS runs_session_started
                  ON runs(session_id, started_at_ms);
             CREATE INDEX IF NOT EXISTS messages_session_ordinal
                 ON messages(session_id, ordinal);
             CREATE INDEX IF NOT EXISTS pending_workspace_grant_promotions_fifo
                 ON pending_workspace_grant_promotions(created_at_ms, command_id);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let schema_version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    match schema_version.as_deref() {
        None => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            create_session_compactions_table(&transaction)?;
            transaction
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('schema_version', '10')",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("1") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if !has_column(&transaction, "runs", "cancel_requested")? {
                transaction
                    .execute(
                        "ALTER TABLE runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
                        [],
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            for statement in [
                "ALTER TABLE sessions ADD COLUMN estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE sessions ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 1",
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE runs ADD COLUMN usage_json TEXT",
                "ALTER TABLE runs ADD COLUMN estimated_cost_usd_nanos INTEGER",
            ] {
                transaction
                    .execute(statement, [])
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            transaction
                .execute("UPDATE sessions SET cost_known = 0", [])
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("2") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("3") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            for statement in [
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE tool_calls ADD COLUMN approval_resolution TEXT",
                "ALTER TABLE tool_calls ADD COLUMN resolved_at_ms INTEGER",
            ] {
                transaction
                    .execute(statement, [])
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("4") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("5") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("6") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("7") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("8") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("9") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some(
            "10" | "11" | "12" | "13" | "14" | "15" | "16" | "17" | "18" | "19" | "20" | "21"
            | "22" | "23" | "24" | "25",
        ) => {}
        Some(_) => return Err(SessionRuntimeError::Persistence),
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "11" | "12"
                | "13"
                | "14"
                | "15"
                | "16"
                | "17"
                | "18"
                | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
        )
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_sessions_context_tokens_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '11' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "12" | "13"
                | "14"
                | "15"
                | "16"
                | "17"
                | "18"
                | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
        )
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_sessions_owner_run_id_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '12' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "13" | "14"
                | "15"
                | "16"
                | "17"
                | "18"
                | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
        )
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_runs_prompt_identity_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '13' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some("14" | "15" | "16" | "17" | "18" | "19" | "20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_model_turn_audit_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '14' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_model_turn_audit_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some("15" | "16" | "17" | "18" | "19" | "20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_linear_streaming_storage(&transaction)?;
        validate_linear_streaming_schema(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '15' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    } else if column_shape(
        &connection,
        "pending_workspace_grant_promotions",
        "command_id",
    )? != ("TEXT".to_owned(), true, None, 1)
    {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        normalize_promotion_outbox(&transaction)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_linear_streaming_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some("16" | "17" | "18" | "19" | "20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_runs_resolved_model_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '16' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(&connection, "runs", "resolved_model_json")? {
        return Err(SessionRuntimeError::Persistence);
    }
    if !matches!(
        schema_version.as_deref(),
        Some("17" | "18" | "19" | "20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_preparing_run_storage(&transaction)?;
        validate_preparing_run_schema(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '17' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_preparing_run_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some("18" | "19" | "20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_context_occupancy_storage(&transaction)?;
        validate_context_occupancy_schema(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '18' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_context_occupancy_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some("19" | "20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_run_limits_storage(&transaction)?;
        validate_run_limits_schema(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '19' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_run_limits_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some("20" | "21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_sessions_spawned_by_tool_call_column(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '20' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some("21" | "22" | "23" | "24" | "25")
    ) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_contract_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '21' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !matches!(schema_version.as_deref(), Some("22" | "23" | "24" | "25")) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_output_truncation_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '22' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_output_truncation_schema(&connection)?;
    if !matches!(schema_version.as_deref(), Some("23" | "24" | "25")) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_session_depth_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '23' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_session_depth_schema(&connection)?;
    if !matches!(schema_version.as_deref(), Some("24" | "25")) {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_audit_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '24' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_audit_schema(&connection)?;
    if schema_version.as_deref() != Some("25") {
        let transaction = connection
            .transaction()
            .map_err(|_| SessionRuntimeError::Persistence)?;
        add_fast_path_columns(&transaction)?;
        transaction
            .execute(
                "UPDATE metadata SET value = '25' WHERE key = 'schema_version'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        transaction
            .commit()
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    validate_fast_path_schema(&connection)?;
    debug_assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .as_deref(),
        Some(STORE_SCHEMA_VERSION.to_string().as_str()),
        "the last migration step must write STORE_SCHEMA_VERSION"
    );
    let stored = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'store_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let store_id = match stored {
        Some(value) => value
            .parse()
            .map_err(|_| SessionRuntimeError::Persistence)?,
        None => {
            let id = StoreId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            connection
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('store_id', ?1)",
                    [id.to_string()],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            id
        }
    };
    Ok((connection, store_id))
}

fn add_run_limits_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "limits_json")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN limits_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Historical runs carry NULL: they were admitted without caller limits and
/// must never be reinterpreted under current defaults.
fn validate_run_limits_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "limits_json")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}

fn add_context_occupancy_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "context_occupancy_json")? {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN context_occupancy_json TEXT",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(
        connection,
        "sessions",
        "pending_context_overflow_basis_json",
    )? {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN pending_context_overflow_basis_json TEXT",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

fn validate_context_occupancy_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "sessions", "context_occupancy_json")?
        != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::Persistence);
    }
    if column_shape(
        connection,
        "sessions",
        "pending_context_overflow_basis_json",
    )? != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}

fn add_preparing_run_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "preparing_run_id")? {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN preparing_run_id TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(
        connection,
        "sessions",
        "pending_context_overflow_model_json",
    )? {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN pending_context_overflow_model_json TEXT",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(connection, "runs", "context_compaction_attempted")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN context_compaction_attempted INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(connection, "runs", "auto_compaction_for_run_id")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN auto_compaction_for_run_id TEXT",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    connection
        .execute(
            "UPDATE runs AS compaction
             SET auto_compaction_for_run_id = (
                 SELECT queued.id
                 FROM runs queued
                 WHERE queued.session_id = compaction.session_id
                   AND queued.status = 'queued'
                   AND queued.kind = 'prompt'
                 ORDER BY queued.created_at_ms, queued.rowid
                 LIMIT 1
             )
             WHERE compaction.status = 'running'
               AND compaction.kind = 'compaction'
               AND compaction.auto_compaction = 1
               AND compaction.auto_compaction_for_run_id IS NULL
               AND EXISTS (
                   SELECT 1 FROM sessions session
                   WHERE session.id = compaction.session_id
                     AND session.active_run_id = compaction.id
               )",
            [],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    connection
        .execute(
            "UPDATE runs
             SET context_compaction_attempted = 1
             WHERE id IN (
                 SELECT auto_compaction_for_run_id
                 FROM runs
                 WHERE auto_compaction_for_run_id IS NOT NULL
             )",
            [],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(())
}

fn validate_preparing_run_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let preparing = column_shape(connection, "sessions", "preparing_run_id")?;
    if preparing != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    let overflow = column_shape(
        connection,
        "sessions",
        "pending_context_overflow_model_json",
    )?;
    if overflow != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    let attempted = column_shape(connection, "runs", "context_compaction_attempted")?;
    if attempted != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    let owner = column_shape(connection, "runs", "auto_compaction_for_run_id")?;
    if owner != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}

fn column_shape(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<(String, bool, Option<String>, u8), SessionRuntimeError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| SessionRuntimeError::Persistence)?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u8>(5)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .find_map(|row| match row {
            Ok((name, ty, not_null, default_value, primary_key)) if name == column => {
                Some(Ok((ty, not_null, default_value, primary_key)))
            }
            Ok(_) => None,
            Err(_) => Some(Err(SessionRuntimeError::Persistence)),
        })
        .unwrap_or(Err(SessionRuntimeError::Persistence))
}

/// Existing runs remain explicitly unknown: no current configuration is
/// guessed for historical work.
fn add_runs_resolved_model_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "resolved_model_json")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN resolved_model_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

fn add_linear_streaming_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS message_chunks (
                 message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                 channel TEXT NOT NULL CHECK(channel IN ('output', 'refusal')),
                 chunk_ordinal INTEGER NOT NULL CHECK(chunk_ordinal > 0),
                 text TEXT NOT NULL,
                 PRIMARY KEY(message_id, channel, chunk_ordinal)
             );
             CREATE TABLE IF NOT EXISTS pending_workspace_grant_promotions (
                 command_id TEXT NOT NULL PRIMARY KEY,
                 created_at_ms INTEGER NOT NULL,
                 promotion_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS pending_workspace_grant_promotions_fifo
                 ON pending_workspace_grant_promotions(created_at_ms, command_id);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if !has_column(connection, "runs", "context_base_bytes")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN context_base_bytes INTEGER", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(connection, "runs", "context_increment_bytes")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN context_increment_bytes INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    normalize_promotion_outbox(connection)?;
    Ok(())
}

/// SQLite treats `TEXT PRIMARY KEY` as nullable. Stores created before the
/// explicit `NOT NULL` declaration fail the later shape check unless rewritten.
fn normalize_promotion_outbox(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let command_id = column_shape(
        connection,
        "pending_workspace_grant_promotions",
        "command_id",
    )?;
    if command_id == ("TEXT".to_owned(), true, None, 1) {
        return Ok(());
    }
    if command_id != ("TEXT".to_owned(), false, None, 1) {
        return Err(SessionRuntimeError::Persistence);
    }
    connection
        .execute_batch(
            "CREATE TABLE pending_workspace_grant_promotions_normalized (
                 command_id TEXT NOT NULL PRIMARY KEY,
                 created_at_ms INTEGER NOT NULL,
                 promotion_json TEXT NOT NULL
             );
             INSERT INTO pending_workspace_grant_promotions_normalized
             SELECT command_id, created_at_ms, promotion_json
             FROM pending_workspace_grant_promotions;
             DROP TABLE pending_workspace_grant_promotions;
             ALTER TABLE pending_workspace_grant_promotions_normalized
             RENAME TO pending_workspace_grant_promotions;
             CREATE INDEX IF NOT EXISTS pending_workspace_grant_promotions_fifo
                 ON pending_workspace_grant_promotions(created_at_ms, command_id);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn validate_linear_streaming_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (table, column) in [
        ("message_chunks", "message_id"),
        ("message_chunks", "channel"),
        ("message_chunks", "chunk_ordinal"),
        ("message_chunks", "text"),
        ("runs", "context_base_bytes"),
        ("runs", "context_increment_bytes"),
        ("pending_workspace_grant_promotions", "command_id"),
        ("pending_workspace_grant_promotions", "created_at_ms"),
        ("pending_workspace_grant_promotions", "promotion_json"),
    ] {
        if !has_column(connection, table, column)? {
            return Err(SessionRuntimeError::Persistence);
        }
    }
    let mut columns = connection
        .prepare("PRAGMA table_info(message_chunks)")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let columns = columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, u8>(5)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let expected = [
        ("message_id", "TEXT", true, 1_u8),
        ("channel", "TEXT", true, 2_u8),
        ("chunk_ordinal", "INTEGER", true, 3_u8),
        ("text", "TEXT", true, 0_u8),
    ];
    if columns.len() != expected.len()
        || columns
            .iter()
            .zip(expected)
            .any(|((name, ty, not_null, primary_key), expected)| {
                (name.as_str(), ty.as_str(), *not_null, *primary_key) != expected
            })
    {
        return Err(SessionRuntimeError::Persistence);
    }
    let mut run_columns = connection
        .prepare("PRAGMA table_info(runs)")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let run_columns = run_columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u8>(5)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .filter_map(|column| match column {
            Ok(column)
                if matches!(
                    column.0.as_str(),
                    "context_base_bytes" | "context_increment_bytes"
                ) =>
            {
                Some(Ok(column))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let expected_run_columns = [
        ("context_base_bytes", "INTEGER", false, None, 0_u8),
        ("context_increment_bytes", "INTEGER", true, Some("0"), 0_u8),
    ];
    if run_columns.len() != expected_run_columns.len()
        || run_columns.iter().zip(expected_run_columns).any(
            |((name, ty, not_null, default_value, primary_key), expected)| {
                (
                    name.as_str(),
                    ty.as_str(),
                    *not_null,
                    default_value.as_deref(),
                    *primary_key,
                ) != expected
            },
        )
    {
        return Err(SessionRuntimeError::Persistence);
    }
    let mut foreign_keys = connection
        .prepare("PRAGMA foreign_key_list(message_chunks)")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let foreign_keys = foreign_keys
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if foreign_keys
        != [(
            "messages".to_owned(),
            "message_id".to_owned(),
            "id".to_owned(),
            "CASCADE".to_owned(),
        )]
    {
        return Err(SessionRuntimeError::Persistence);
    }
    let schema_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'message_chunks'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let schema_sql = schema_sql
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if !schema_sql.contains("check(channel in ('output', 'refusal'))")
        || !schema_sql.contains("check(chunk_ordinal > 0)")
    {
        return Err(SessionRuntimeError::Persistence);
    }
    let mut promotion_columns = connection
        .prepare("PRAGMA table_info(pending_workspace_grant_promotions)")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let promotion_columns = promotion_columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, u8>(5)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let expected_promotion_columns = [
        ("command_id", "TEXT", true, 1_u8),
        ("created_at_ms", "INTEGER", true, 0_u8),
        ("promotion_json", "TEXT", true, 0_u8),
    ];
    if promotion_columns.len() != expected_promotion_columns.len()
        || promotion_columns
            .iter()
            .zip(expected_promotion_columns)
            .any(|((name, ty, not_null, primary_key), expected)| {
                (name.as_str(), ty.as_str(), *not_null, *primary_key) != expected
            })
    {
        return Err(SessionRuntimeError::Persistence);
    }
    let promotion_index: bool = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_schema
                 WHERE type = 'index'
                   AND name = 'pending_workspace_grant_promotions_fifo'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if !promotion_index {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}

fn create_tool_tables(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE model_turns (
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 model_json TEXT,
                 usage_json TEXT,
                 estimated_cost_usd_nanos INTEGER,
                 completed_at_ms INTEGER,
                 PRIMARY KEY(run_id, turn_ordinal)
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
                 display_json TEXT,
                 approval_resolution TEXT,
                 requested_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 resolved_at_ms INTEGER,
                 finished_at_ms INTEGER,
                 UNIQUE(run_id, turn_ordinal, provider_call_id),
                 UNIQUE(run_id, turn_ordinal, call_ordinal)
             );
             CREATE INDEX tool_calls_run_ordinal
                 ON tool_calls(run_id, turn_ordinal, call_ordinal);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn add_model_turn_audit_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS model_turns (
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 model_json TEXT,
                 usage_json TEXT,
                 estimated_cost_usd_nanos INTEGER,
                 completed_at_ms INTEGER,
                 PRIMARY KEY(run_id, turn_ordinal)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    for (column, declaration) in [
        ("model_json", "TEXT"),
        ("usage_json", "TEXT"),
        ("estimated_cost_usd_nanos", "INTEGER"),
        ("completed_at_ms", "INTEGER"),
    ] {
        if !has_column(connection, "model_turns", column)? {
            connection
                .execute(
                    &format!("ALTER TABLE model_turns ADD COLUMN {column} {declaration}"),
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
    }
    Ok(())
}

fn validate_model_turn_audit_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for column in [
        "run_id",
        "turn_ordinal",
        "assistant_content_json",
        "model_json",
        "usage_json",
        "estimated_cost_usd_nanos",
        "completed_at_ms",
    ] {
        if !has_column(connection, "model_turns", column)? {
            return Err(SessionRuntimeError::Persistence);
        }
    }
    Ok(())
}

/// Adds `messages.turn_ordinal` for stores created before per-turn assistant
/// messages. Existing rows keep 0: legacy runs render as one message with the
/// run's calls grouped after it.
fn add_messages_turn_ordinal_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "messages", "turn_ordinal")? {
        connection
            .execute(
                "ALTER TABLE messages ADD COLUMN turn_ordinal INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds `tool_calls.display_json` for stores created before tool calls
/// carried a UI display payload. Existing rows keep NULL: legacy results
/// render from their bounded result string alone.
fn add_tool_calls_display_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "tool_calls", "display_json")? {
        connection
            .execute("ALTER TABLE tool_calls ADD COLUMN display_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

fn create_grant_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_grants (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 kind TEXT NOT NULL,
                 value TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, kind, value)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn create_session_files_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_files (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 path TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, path)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

/// One committed compaction: the structured summary and the message-ordinal
/// cutoff it replaces. Assembly reads the newest row; older rows are bounded
/// history retained for a future rollback command.
fn create_session_compactions_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_compactions (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 run_id TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 cutoff_ordinal INTEGER NOT NULL,
                 before_bytes INTEGER NOT NULL,
                 after_bytes INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX session_compactions_session
                 ON session_compactions(session_id, created_at_ms DESC);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

/// Adds `runs.kind` for stores created before internal compaction runs.
/// Existing rows keep 'prompt'.
fn add_runs_kind_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "kind")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN kind TEXT NOT NULL DEFAULT 'prompt'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds `runs.context_tokens` for stores created before the last model
/// turn's input-token total was persisted separately from the run's summed
/// billing usage. Existing rows keep NULL: clients fall back to the sum.
fn add_runs_context_tokens_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "context_tokens")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN context_tokens INTEGER", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds authoritative session occupancy without guessing from historical
/// run billing totals. Existing sessions remain unknown until a prompt turn
/// reports usage.
fn add_sessions_context_tokens_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "context_tokens")? {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN context_tokens INTEGER", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Records the parent `spawn_agent` tool call that created a child, so clients
/// can place the child under its call. Historical children stay NULL.
fn add_sessions_spawned_by_tool_call_column(
    connection: &Connection,
) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "spawned_by_tool_call_id")? {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN spawned_by_tool_call_id TEXT",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Schema 21: the protocol-13 backend contract. Sessions carry a selected
/// agent profile and opaque correlation; runs carry their structured input,
/// correlation, and the secret-free plan identity and descriptor fixed at
/// start; user messages carry their structured input and a steering flag.
/// Every column is additive and nullable so historical rows read back as
/// `default` profile, empty correlation, and no plan identity.
fn add_contract_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    const COLUMNS: [(&str, &str, &str); 8] = [
        ("sessions", "profile", "TEXT"),
        ("sessions", "correlation_json", "TEXT"),
        ("runs", "input_json", "TEXT"),
        ("runs", "correlation_json", "TEXT"),
        ("runs", "plan_identity_json", "TEXT"),
        ("runs", "plan_descriptor_json", "TEXT"),
        ("messages", "input_json", "TEXT"),
        ("messages", "steering", "INTEGER NOT NULL DEFAULT 0"),
    ];
    for (table, column, kind) in COLUMNS {
        if !has_column(connection, table, column)? {
            connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
    }
    Ok(())
}

/// Records output truncation state. Historical rows read back as never
/// truncated and zero continuations, which is what they were.
fn add_output_truncation_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (table, column) in OUTPUT_TRUNCATION_COLUMNS {
        if !has_column(connection, table, column)? {
            connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"),
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
    }
    Ok(())
}

const OUTPUT_TRUNCATION_COLUMNS: [(&str, &str); 3] = [
    ("messages", "truncated"),
    ("model_turns", "truncated"),
    ("runs", "output_continuations"),
];

fn validate_output_truncation_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (table, column) in OUTPUT_TRUNCATION_COLUMNS {
        if column_shape(connection, table, column)?
            != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
        {
            return Err(SessionRuntimeError::Persistence);
        }
    }
    Ok(())
}

/// Records the parent run that owns a spawned child. Publicly created child
/// sessions and historical rows remain unowned (NULL).
fn add_sessions_owner_run_id_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "owner_run_id")? {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN owner_run_id TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds `runs.auto_compaction` for stores created before threshold-triggered
/// compaction. 1 marks a compaction run the runtime claimed automatically at
/// a context threshold; 0 (every existing row) is a user-requested run.
fn add_runs_auto_compaction_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "auto_compaction")? {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN auto_compaction INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

/// Adds the stable prompt and workspace-instruction identity prepared before
/// provider work. Existing runs remain unknown rather than being assigned the
/// current prompt after the fact.
fn add_runs_prompt_identity_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "prompt_identity_json")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN prompt_identity_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

pub(in crate::sessions) fn has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, SessionRuntimeError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(columns.iter().any(|candidate| candidate == column))
}

/// Records where a session sits in its delegation tree: `depth` (0 for a
/// root, 1 for a child, ...) and `root_run_id`, the root run whose subtree it
/// belongs to. Historical children are backfilled at depth 1 under their
/// owner run, which is exactly what they were when depth was structurally
/// one; deeper history does not exist.
fn add_session_depth_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "depth")? {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(connection, "sessions", "root_run_id")? {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN root_run_id TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    connection
        .execute(
            "UPDATE sessions SET depth = 1, root_run_id = owner_run_id
             WHERE owner_run_id IS NOT NULL AND root_run_id IS NULL",
            [],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(())
}

fn validate_session_depth_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let depth = column_shape(connection, "sessions", "depth")?;
    if depth != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    let root = column_shape(connection, "sessions", "root_run_id")?;
    if root != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}

/// Records why a session exists (`task` or `audit`) and how a run's final
/// answer was audited. Historical rows are ordinary task sessions with no
/// audit, which is what they were.
fn add_audit_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "purpose")? {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN purpose TEXT NOT NULL DEFAULT 'task'",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    if !has_column(connection, "runs", "audit_json")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN audit_json TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    Ok(())
}

fn validate_audit_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "sessions", "purpose")?
        != ("TEXT".to_owned(), true, Some("'task'".to_owned()), 0)
    {
        return Err(SessionRuntimeError::Persistence);
    }
    if column_shape(connection, "runs", "audit_json")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}

/// Schema 25: the command-acknowledgement fast path.
///
/// `runs.activity` is the run's latest `RunActivityChanged` value, written in
/// the same transaction as the event so the session summary reads a column
/// instead of scanning the event log. It is null for runs recorded before
/// this version and for runs that have not reported activity, which clients
/// already tolerate. `metadata.command_count` replaces the per-command
/// `COUNT(*)` over `commands`; it is backfilled from the table once.
fn add_fast_path_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "activity")? {
        connection
            .execute("ALTER TABLE runs ADD COLUMN activity TEXT", [])
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    connection
        .execute(
            "INSERT OR REPLACE INTO metadata(key, value)
             VALUES ('command_count', (SELECT COUNT(*) FROM commands))",
            [],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(())
}

fn validate_fast_path_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "activity")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::Persistence);
    }
    let counted: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'command_count'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if counted.is_none() {
        return Err(SessionRuntimeError::Persistence);
    }
    Ok(())
}
