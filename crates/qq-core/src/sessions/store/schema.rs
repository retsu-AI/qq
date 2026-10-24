use std::path::PathBuf;

use qq_protocol::StoreId;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::sessions::{PersistenceFault, SessionRuntimeError};

/// Current session store schema, stored as `metadata.schema_version`. Bump it
/// with every migration step appended to `open_database`.
pub const STORE_SCHEMA_VERSION: u16 = 63;

// Also a reclamation outbox: pending acknowledgements survive session pruning.
const NATIVE_COMMAND_OWNERS: &str = "CREATE TABLE native_command_owners (
    tool_call_id TEXT NOT NULL PRIMARY KEY CHECK(length(tool_call_id)=32),
    owner_id TEXT NOT NULL CHECK(length(owner_id)=32),
    result_saved INTEGER NOT NULL DEFAULT 0 CHECK(result_saved IN (0,1))
) STRICT";

const GOAL_POLICY_COLUMN: &str = "policy_json TEXT CHECK(policy_json IS NULL OR (json_valid(policy_json) AND length(policy_json)<=8192))";
const GOAL_SPECIFICATION_COLUMN: &str = "specification_at_ms INTEGER CHECK(specification_at_ms IS NULL OR (specification_at_ms>=0 AND policy_json IS NOT NULL))";

/// Suffix of the sibling file whose advisory lock marks the store's owner.
pub const OWNER_LOCK_SUFFIX: &str = ".lock";

/// Exclusive ownership of one store. An OS advisory lock on a sibling
/// `<store>.lock` file: a process that exits or crashes releases it without
/// cleanup, and dropping the guard releases it deliberately. The file carries
/// no data; the database itself is opened only after the lock is held.
pub(in crate::sessions) struct StoreOwnership {
    _file: std::fs::File,
}

/// How long an opener waits for a departing owner to release the store
/// before reporting it busy. Covers the ordinary handoff (the previous owner
/// is closing its connection as the next one starts) without turning a
/// genuinely busy store into a hang. Blocking: runs on the store's own
/// thread, never on an executor.
pub(in crate::sessions) const OWNERSHIP_WAIT: std::time::Duration =
    std::time::Duration::from_millis(1_500);
const OWNERSHIP_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// Takes exclusive ownership of the store at `path` or reports that another
/// process holds it. Runs before SQLite is opened so a busy store sees no
/// I/O beyond this probe.
pub(in crate::sessions) fn acquire_ownership(
    path: &std::path::Path,
) -> Result<StoreOwnership, SessionRuntimeError> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(OWNER_LOCK_SUFFIX);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| {
            SessionRuntimeError::Persistence(PersistenceFault::Sqlite(
                rusqlite::ffi::ErrorCode::CannotOpen,
            ))
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));
    }
    let deadline = std::time::Instant::now() + OWNERSHIP_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(StoreOwnership { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(SessionRuntimeError::StoreBusy);
                }
                std::thread::sleep(OWNERSHIP_POLL);
            }
            Err(std::fs::TryLockError::Error(_)) => {
                return Err(SessionRuntimeError::Persistence(PersistenceFault::Sqlite(
                    rusqlite::ffi::ErrorCode::CannotOpen,
                )));
            }
        }
    }
}

pub(in crate::sessions) fn open_database(
    path: &PathBuf,
) -> Result<(Connection, StoreId), SessionRuntimeError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SessionRuntimeError::CONSTRAINT),
    }
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    // Hot statements on the output lane are prepared once per connection.
    connection.set_prepared_statement_cache_capacity(128);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|_| {
            SessionRuntimeError::Persistence(PersistenceFault::Sqlite(
                rusqlite::ffi::ErrorCode::CannotOpen,
            ))
        })?;
    }
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TEMP TABLE native_observation_runs (
                 run_id TEXT PRIMARY KEY
             ) WITHOUT ROWID;
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
    )?;
    let schema_version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match schema_version.as_deref() {
        None => {
            let transaction = connection.transaction()?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            create_session_compactions_table(&transaction)?;
            transaction.execute(
                "INSERT INTO metadata(key, value) VALUES ('schema_version', '10')",
                [],
            )?;
            transaction.commit()?;
        }
        Some("1") => {
            let transaction = connection.transaction()?;
            if !has_column(&transaction, "runs", "cancel_requested")? {
                transaction.execute(
                    "ALTER TABLE runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            for statement in [
                "ALTER TABLE sessions ADD COLUMN estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE sessions ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 1",
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE runs ADD COLUMN usage_json TEXT",
                "ALTER TABLE runs ADD COLUMN estimated_cost_usd_nanos INTEGER",
            ] {
                transaction.execute(statement, [])?;
            }
            transaction.execute("UPDATE sessions SET cost_known = 0", [])?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("2") => {
            let transaction = connection.transaction()?;
            transaction.execute(
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                [],
            )?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("3") => {
            let transaction = connection.transaction()?;
            for statement in [
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE tool_calls ADD COLUMN approval_resolution TEXT",
                "ALTER TABLE tool_calls ADD COLUMN resolved_at_ms INTEGER",
            ] {
                transaction.execute(statement, [])?;
            }
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("4") => {
            let transaction = connection.transaction()?;
            create_session_files_table(&transaction)?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("5") => {
            let transaction = connection.transaction()?;
            add_messages_turn_ordinal_column(&transaction)?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("6") => {
            let transaction = connection.transaction()?;
            add_tool_calls_display_column(&transaction)?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("7") => {
            let transaction = connection.transaction()?;
            create_session_compactions_table(&transaction)?;
            add_runs_kind_column(&transaction)?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("8") => {
            let transaction = connection.transaction()?;
            add_runs_context_tokens_column(&transaction)?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some("9") => {
            let transaction = connection.transaction()?;
            add_runs_auto_compaction_column(&transaction)?;
            transaction.execute(
                "UPDATE metadata SET value = '10' WHERE key = 'schema_version'",
                [],
            )?;
            transaction.commit()?;
        }
        Some(
            "10" | "11" | "12" | "13" | "14" | "15" | "16" | "17" | "18" | "19" | "20" | "21"
            | "22" | "23" | "24" | "25" | "26" | "27" | "28" | "29" | "30" | "31" | "32" | "33"
            | "34" | "35" | "36" | "37" | "38" | "39" | "40" | "41" | "42" | "43" | "44" | "45"
            | "46" | "47" | "48" | "49" | "50" | "51" | "52" | "53" | "54" | "55" | "56" | "57"
            | "58" | "59" | "60" | "61" | "62" | "63",
        ) => {}
        Some(_) => return Err(SessionRuntimeError::CONSTRAINT),
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
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_sessions_context_tokens_column(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '11' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
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
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_sessions_owner_run_id_column(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '12' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
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
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_runs_prompt_identity_column(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '13' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "14" | "15"
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
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_model_turn_audit_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '14' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_model_turn_audit_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "15" | "16"
                | "17"
                | "18"
                | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_linear_streaming_storage(&transaction)?;
        validate_linear_streaming_schema(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '15' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    } else if column_shape(
        &connection,
        "pending_workspace_grant_promotions",
        "command_id",
    )? != ("TEXT".to_owned(), true, None, 1)
    {
        let transaction = connection.transaction()?;
        normalize_promotion_outbox(&transaction)?;
        transaction.commit()?;
    }
    validate_linear_streaming_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "16" | "17"
                | "18"
                | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_runs_resolved_model_column(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '16' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if !has_column(&connection, "runs", "resolved_model_json")? {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "17" | "18"
                | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_preparing_run_storage(&transaction)?;
        validate_preparing_run_schema(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '17' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_preparing_run_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "18" | "19"
                | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_context_occupancy_storage(&transaction)?;
        validate_context_occupancy_schema(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '18' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_context_occupancy_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "19" | "20"
                | "21"
                | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_run_limits_storage(&transaction)?;
        validate_run_limits_schema(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '19' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_run_limits_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "20" | "21"
                | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_sessions_spawned_by_tool_call_column(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '20' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "21" | "22"
                | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_contract_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '21' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if !matches!(
        schema_version.as_deref(),
        Some(
            "22" | "23"
                | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_output_truncation_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '22' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_output_truncation_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "23" | "24"
                | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_session_depth_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '23' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_session_depth_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "24" | "25"
                | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_audit_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '24' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_audit_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "25" | "26"
                | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_fast_path_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '25' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_fast_path_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "26" | "27"
                | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_tool_call_effect_column(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '26' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_tool_call_effect_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "27" | "28"
                | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        add_output_contract_columns(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '27' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_output_contract_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "28" | "29"
                | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        create_tool_spills_table(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '28' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_tool_spills_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "29" | "30"
                | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        create_attachment_tables(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '29' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_attachment_schema(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "30" | "31"
                | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        create_messages_run_index(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '30' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_messages_run_index(&connection)?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "31" | "32"
                | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "runs", "routing_json")? {
            transaction.execute("ALTER TABLE runs ADD COLUMN routing_json TEXT", [])?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '31' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    let routing_shape: (String, bool, Option<String>) = connection.query_row(
        "SELECT type, [notnull], dflt_value FROM pragma_table_info('runs') WHERE name = 'routing_json'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if routing_shape != ("TEXT".to_owned(), false, None) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }

    if !matches!(
        schema_version.as_deref(),
        Some(
            "32" | "33"
                | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "sessions", "model_is_fallback")? {
            transaction.execute(
                "ALTER TABLE sessions ADD COLUMN model_is_fallback INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '32' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    let model_choice_shape: (String, bool, Option<String>) = connection.query_row(
        "SELECT type, [notnull], dflt_value FROM pragma_table_info('sessions') WHERE name = 'model_is_fallback'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if model_choice_shape != ("INTEGER".to_owned(), true, Some("0".to_owned())) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }

    // 33: an in-run compaction marker names the run whose earlier turns its
    // summary replaces and the last turn ordinal it covers. Between-run
    // markers leave both NULL and keep their prompt-ordinal semantics.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "33" | "34"
                | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        // Test fixtures for very old versions never created the table; the
        // ≥10 branch above is a no-op for them. Create before altering.
        if !has_table(&transaction, "session_compactions")? {
            create_session_compactions_table(&transaction)?;
        }
        if !has_column(&transaction, "session_compactions", "scope_run_id")? {
            transaction.execute(
                "ALTER TABLE session_compactions ADD COLUMN scope_run_id TEXT",
                [],
            )?;
        }
        if !has_column(&transaction, "session_compactions", "turn_cutoff")? {
            transaction.execute(
                "ALTER TABLE session_compactions ADD COLUMN turn_cutoff INTEGER",
                [],
            )?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '33' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    for (column, expected) in [("scope_run_id", "TEXT"), ("turn_cutoff", "INTEGER")] {
        if column_shape(&connection, "session_compactions", column)?
            != (expected.to_owned(), false, None, 0)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    // 34: optional per-session reasoning effort pin. NULL means the compiled
    // plan's configured or profile choice; a stored value is applied at claim.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "34" | "35"
                | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "sessions", "reasoning_effort")? {
            transaction.execute("ALTER TABLE sessions ADD COLUMN reasoning_effort TEXT", [])?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '34' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if column_shape(&connection, "sessions", "reasoning_effort")?
        != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 35: who recorded a session grant, and for a delegate, which run. `human`
    // is every grant that existed before (approve-for-session,
    // approve-for-workspace, config seed) and stays NULL for `run_id`.
    // `delegate` is a reviewer's exact-command or exact-host grant: it
    // matches only the exact string, never lifts `Forbidden`, is capped per
    // run by `run_id`, and is never promoted to workspace config.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "35" | "36"
                | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_table(&transaction, "session_grants")? {
            create_grant_table(&transaction)?;
        }
        if !has_column(&transaction, "session_grants", "source")? {
            transaction.execute(
                "ALTER TABLE session_grants ADD COLUMN source TEXT NOT NULL DEFAULT 'human'",
                [],
            )?;
        }
        if !has_column(&transaction, "session_grants", "run_id")? {
            transaction.execute("ALTER TABLE session_grants ADD COLUMN run_id TEXT", [])?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '35' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if column_shape(&connection, "session_grants", "source")?
        != ("TEXT".to_owned(), true, Some("'human'".to_owned()), 0)
        || column_shape(&connection, "session_grants", "run_id")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 36: a per-session override of who settles held calls
    // (`set_approval_delegate`). NULL means the configured `approval_delegate`
    // applies; a stored value (`by_mode`, `on`, `off`) is read by the gate at
    // each held call and inherited by spawned children.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "36" | "37"
                | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "sessions", "approval_delegate")? {
            transaction.execute("ALTER TABLE sessions ADD COLUMN approval_delegate TEXT", [])?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '36' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if column_shape(&connection, "sessions", "approval_delegate")?
        != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 37: persisted `max` effort requires a reader that understands it.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "37" | "38"
                | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        connection.execute(
            "UPDATE metadata SET value = '37' WHERE key = 'schema_version'",
            [],
        )?;
    }
    // 38: model turns may contain a provider continuation envelope.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "38" | "39"
                | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        connection.execute(
            "UPDATE metadata SET value = '38' WHERE key = 'schema_version'",
            [],
        )?;
    }
    // 39: explicit provider-default effort is distinct from inheritance.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "39" | "40"
                | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        connection.execute(
            "UPDATE metadata SET value = '39' WHERE key = 'schema_version'",
            [],
        )?;
    }
    // 40: a model turn records the runtime notice its request first carried
    // (ADR-0054 § 2): `report` or `continuation`, NULL for none. Assembly
    // replays it before the turn.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "40" | "41"
                | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "model_turns", "notice")? {
            transaction.execute("ALTER TABLE model_turns ADD COLUMN notice TEXT", [])?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '40' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if column_shape(&connection, "model_turns", "notice")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 41: a non-blocking child's answer is delivered into its parent's
    // context once (ADR-0054 § 4). One row per delivery, keyed by the child
    // run, so a second delivery of the same answer is a constraint failure.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "41" | "42"
                | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        create_child_deliveries_table(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '41' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_child_deliveries_table(&connection)?;
    // 42: a running child's interim reports reach its parent, each once
    // (ADR-0054 § 4). A row is written only when a report is delivered, keyed
    // by the child's report turn.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "42" | "43"
                | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        create_child_reports_table(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '42' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_child_reports_table(&connection)?;
    // 43: assembly stubs stale read-only results only up to a durable
    // watermark (ADR-0056 § 6). An upgraded session's watermark is its
    // newest committed turn, so it assembles exactly as before the upgrade
    // and then holds still until the next seam.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "43" | "44"
                | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        for column in ["prune_through_ordinal", "prune_through_turn"] {
            if !has_column(&transaction, "sessions", column)? {
                transaction.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {column} INTEGER"),
                    [],
                )?;
            }
        }
        transaction.execute(
            "UPDATE sessions SET (prune_through_ordinal, prune_through_turn) = (
                 SELECT m.ordinal, t.turn_ordinal
                 FROM messages m JOIN model_turns t ON t.run_id = m.run_id
                 WHERE m.session_id = sessions.id AND m.role = 'user' AND m.steering = 0
                 ORDER BY m.ordinal DESC, t.turn_ordinal DESC
                 LIMIT 1
             )",
            [],
        )?;
        transaction.execute(
            "UPDATE metadata SET value = '43' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    for column in ["prune_through_ordinal", "prune_through_turn"] {
        if column_shape(&connection, "sessions", column)? != ("INTEGER".to_owned(), false, None, 0)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    // 44: isolated side questions (SQ1; docs/design/side-questions.md). Rows never enter
    // the main transcript; at most one runs per session.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "44" | "45"
                | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS side_questions (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                thread_id TEXT NOT NULL,
                question TEXT NOT NULL CHECK(length(CAST(question AS BLOB)) <= 8192),
                captured_context_json TEXT NOT NULL CHECK(length(CAST(captured_context_json AS BLOB)) <= 262144),
                state TEXT NOT NULL CHECK(state IN ('running', 'completed', 'failed', 'interrupted', 'cancelled', 'timed_out')),
                answer TEXT NOT NULL DEFAULT '' CHECK(length(CAST(answer AS BLOB)) <= 131072),
                usage_json TEXT,
                estimated_cost_usd_nanos INTEGER,
                model_turns INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                finished_at_ms INTEGER
             );
             CREATE INDEX IF NOT EXISTS side_questions_session ON side_questions(session_id, created_at_ms);
             CREATE UNIQUE INDEX IF NOT EXISTS side_questions_active ON side_questions(session_id) WHERE state = 'running';"
        )?;
        transaction.execute(
            "UPDATE metadata SET value = '44' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    for (column, expected) in [
        ("id", ("TEXT".to_owned(), false, None, 1)),
        ("session_id", ("TEXT".to_owned(), true, None, 0)),
        ("captured_context_json", ("TEXT".to_owned(), true, None, 0)),
        ("state", ("TEXT".to_owned(), true, None, 0)),
    ] {
        if column_shape(&connection, "side_questions", column)? != expected {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    // 45: who a held call waits on. Existing schema-45 stores already
    // contain this flag; never recreate or reset their delegated authority.
    let has_tool_calls = has_table(&connection, "tool_calls")?;
    if !matches!(
        schema_version.as_deref(),
        Some(
            "45" | "46"
                | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if has_tool_calls && !has_column(&transaction, "tool_calls", "approval_delegated")? {
            transaction.execute(
                "ALTER TABLE tool_calls ADD COLUMN approval_delegated INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '45' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if has_tool_calls
        && column_shape(&connection, "tool_calls", "approval_delegated")?
            != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 46: owned internal compaction attempts belong to their prompt's spend
    // receipt. Add the bounded owner lookup after main's schema-45 migration.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "46" | "47"
                | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE INDEX IF NOT EXISTS runs_owned_compaction
             ON runs(auto_compaction_for_run_id)
             WHERE kind = 'compaction' AND auto_compaction_for_run_id IS NOT NULL;",
        )?;
        validate_owned_compaction_index(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '46' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_owned_compaction_index(&connection)?;
    // 47: evidence captured by the process executor, independent of output.
    // Historical calls remain NULL and cannot qualify execution effects.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "47" | "48"
                | "49"
                | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if has_tool_calls && !has_column(&transaction, "tool_calls", "execution_context_json")? {
            transaction.execute("ALTER TABLE tool_calls ADD COLUMN execution_context_json TEXT CHECK (execution_context_json IS NULL OR length(CAST(execution_context_json AS BLOB)) <= 16384)", [])?;
        }
        transaction.execute(
            "UPDATE metadata SET value = '47' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if has_tool_calls
        && column_shape(&connection, "tool_calls", "execution_context_json")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 49: exact model-facing tool replies and checkpoint correction notice.
    // Published experimental schema 48 belongs to the later root-stall slice.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "49" | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "model_turns", "tool_results_projection_json")? {
            transaction.execute("ALTER TABLE model_turns ADD COLUMN tool_results_projection_json TEXT CHECK (tool_results_projection_json IS NULL OR length(CAST(tool_results_projection_json AS BLOB)) <= 2097152)", [])?;
        }
        if column_shape(&transaction, "model_turns", "tool_results_projection_json")?
            != ("TEXT".to_owned(), false, None, 0)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        // Schema48 already contains notices that a schema49 reader cannot decode.
        // Publish its projection storage and compatible version in one transaction.
        let projection_version = if schema_version.as_deref() == Some("48") {
            "50"
        } else {
            "49"
        };
        transaction.execute(
            "UPDATE metadata SET value=?1 WHERE key='schema_version'",
            [projection_version],
        )?;
        transaction.commit()?;
    }
    if !has_table(&connection, "model_turns")?
        || column_shape(&connection, "model_turns", "tool_results_projection_json")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    // 50: root_stall_report is a replay value. Experimental48 already used
    // it; this reader retains its codec and adds projection storage first.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "48" | "50"
                | "51"
                | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE metadata SET value='50' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    // 51: durable session goals retain history and fence client generations.
    if !matches!(
        schema_version.as_deref(),
        Some(
            "51" | "52"
                | "53"
                | "54"
                | "55"
                | "56"
                | "57"
                | "58"
                | "59"
                | "60"
                | "61"
                | "62"
                | "63"
        )
    ) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_goals (
                goal_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                revision INTEGER NOT NULL CHECK(revision > 0),
                live INTEGER NOT NULL CHECK(live IN (0, 1)),
                payload_json TEXT NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS session_goals_live
             ON session_goals(session_id) WHERE live = 1;",
        )?;
        for (column, definition) in [
            ("goal_id", "TEXT"),
            ("goal_revision", "INTEGER"),
            ("goal_snapshot_json", "TEXT"),
            ("origin_json", "TEXT"),
            ("goal_initial_notice", "TEXT"),
        ] {
            if !has_column(&transaction, "runs", column)? {
                transaction.execute(
                    &format!("ALTER TABLE runs ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        for table in ["model_turns", "session_compactions"] {
            if has_table(&transaction, table)? && !has_column(&transaction, table, "goal_notice")? {
                transaction.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN goal_notice TEXT"),
                    [],
                )?;
            }
        }
        validate_goal_schema(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value = '51' WHERE key = 'schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_goal_schema(&connection)?;
    // 52: exact request and helper-work evidence.
    if !matches!(
        schema_version.as_deref(),
        Some("52" | "53" | "54" | "55" | "56" | "57" | "58" | "59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "runs", "goal_request_pending")? {
            transaction.execute_batch("ALTER TABLE runs ADD COLUMN goal_request_pending INTEGER NOT NULL DEFAULT 0 CHECK(goal_request_pending IN (0,1));")?;
        }
        if !has_column(&transaction, "runs", "goal_helper_admitted")? {
            transaction.execute_batch("ALTER TABLE runs ADD COLUMN goal_helper_admitted INTEGER NOT NULL DEFAULT 0 CHECK(goal_helper_admitted IN (0,1));")?;
        }
        if !has_column(&transaction, "model_turns", "work_disposition")? {
            transaction.execute_batch("ALTER TABLE model_turns ADD COLUMN work_disposition TEXT CHECK(work_disposition IN ('committed','rejected_before_work','uncertain'));")?;
        }
        validate_goal_request_schema(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value='52' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_goal_request_schema(&connection)?;
    // 53: immutable initial delegated paid-owner anchors.
    if !matches!(
        schema_version.as_deref(),
        Some("53" | "54" | "55" | "56" | "57" | "58" | "59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "runs", "goal_paid_owner_run_id")? {
            transaction.execute_batch(
                "ALTER TABLE runs ADD COLUMN goal_paid_owner_run_id TEXT REFERENCES runs(id);",
            )?;
        }
        validate_goal_paid_owner_schema(&transaction)?;
        restore_pending_paid_owners(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value='53' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    validate_goal_paid_owner_schema(&connection)?;
    // 54: recover work evidence omitted by the nullable schema52 migration.
    if !matches!(
        schema_version.as_deref(),
        Some("54" | "55" | "56" | "57" | "58" | "59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        restore_unfinished_model_work(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value='54' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }

    // 55: preserve legacy admissions that have no completed model-turn row.
    if !matches!(
        schema_version.as_deref(),
        Some("55" | "56" | "57" | "58" | "59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        restore_unfinished_goal_dispatch(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value='55' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }

    // 56: GoalCheck is a persisted run kind. Older readers cannot recover it.
    if !matches!(
        schema_version.as_deref(),
        Some("56" | "57" | "58" | "59" | "60" | "61" | "62" | "63")
    ) {
        connection.execute(
            "UPDATE metadata SET value='56' WHERE key='schema_version'",
            [],
        )?;
    }

    if !matches!(
        schema_version.as_deref(),
        Some("57" | "58" | "59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "runs", "check_acknowledgment_pending")? {
            transaction.execute_batch("ALTER TABLE runs ADD COLUMN check_acknowledgment_pending INTEGER NOT NULL DEFAULT 0 CHECK(check_acknowledgment_pending IN (0,1) AND (check_acknowledgment_pending=0 OR kind='goal_check'));")?;
        }
        transaction.execute_batch("CREATE INDEX IF NOT EXISTS runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1;")?;
        validate_check_acknowledgment_schema(&transaction)?;
        transaction.execute_batch("UPDATE runs SET check_acknowledgment_pending=1 WHERE kind='goal_check' AND status='interrupted';
            UPDATE metadata SET value='57' WHERE key='schema_version';")?;
        transaction.commit()?;
    }
    validate_check_acknowledgment_schema(&connection)?;

    if !matches!(
        schema_version.as_deref(),
        Some("58" | "59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "session_goals", "policy_json")? {
            transaction.execute_batch(&format!(
                "ALTER TABLE session_goals ADD COLUMN {name};",
                name = GOAL_POLICY_COLUMN
            ))?;
        }
        if !has_column(&transaction, "session_goals", "specification_at_ms")? {
            transaction.execute_batch(&format!(
                "ALTER TABLE session_goals ADD COLUMN {name};",
                name = GOAL_SPECIFICATION_COLUMN
            ))?;
        }
        validate_goal_policy_schema(&transaction)?;
        transaction.execute_batch("UPDATE metadata SET value='58' WHERE key='schema_version';")?;
        transaction.commit()?;
    }
    validate_goal_policy_schema(&connection)?;

    if !matches!(
        schema_version.as_deref(),
        Some("59" | "60" | "61" | "62" | "63")
    ) {
        let transaction = connection.transaction()?;
        if !has_table(&transaction, "native_command_owners")? {
            transaction.execute_batch(NATIVE_COMMAND_OWNERS)?;
        }
        transaction.execute_batch("CREATE INDEX IF NOT EXISTS native_command_ack_pending ON native_command_owners(owner_id) WHERE result_saved=1;")?;
        transaction.execute(
            "UPDATE metadata SET value='59' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    let native_sql: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='native_command_owners'",
        [],
        |row| row.get(0),
    )?;
    if native_sql.split_whitespace().collect::<String>()
        != NATIVE_COMMAND_OWNERS.split_whitespace().collect::<String>()
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }

    // Additive projection index: assembling human answers must not revisit
    // every historical shell/read call after each automatic compaction.
    if has_tool_calls {
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS tool_calls_answered_inputs
         ON tool_calls(run_id, turn_ordinal, call_ordinal)
         WHERE name = 'ask_user' AND state = 'completed'
           AND approval_resolution = 'answered' AND is_error = 0 AND result IS NOT NULL;",
        )?;
    }

    // 60: persisted native scopes are an execution boundary. Older runtimes
    // must refuse this store rather than ignore its authority metadata.
    if !matches!(schema_version.as_deref(), Some("60" | "61" | "62" | "63")) {
        connection.execute(
            "UPDATE metadata SET value='60' WHERE key='schema_version'",
            [],
        )?;
    }

    // 61: recovery evidence survives independently of delivery replay.
    if !matches!(schema_version.as_deref(), Some("61" | "62" | "63")) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "model_turns", "legacy_checkpoint_required")? {
            transaction.execute_batch("ALTER TABLE model_turns ADD COLUMN legacy_checkpoint_required INTEGER NOT NULL DEFAULT 0 CHECK(legacy_checkpoint_required IN (0,1));")?;
        }
        if has_tool_calls {
            // Read the archive once, rather than probing its JSON for every
            // retained turn on every context assembly. This is only legacy
            // evidence: current runtimes do not emit checkpoint events.
            let legacy: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE CASE WHEN json_valid(plan_descriptor_json)
                 THEN json_extract(plan_descriptor_json,'$.checkpoint') END IS NOT NULL)",
                [],
                |row| row.get(0),
            )?;
            if legacy {
                transaction.execute_batch("UPDATE model_turns SET legacy_checkpoint_required=1
                    WHERE (run_id,turn_ordinal) IN (
                        SELECT c.run_id,c.turn_ordinal FROM events e
                        JOIN tool_calls c ON c.id=json_extract(e.envelope_json,'$.event.tool_call_id')
                        JOIN runs r ON r.id=c.run_id JOIN sessions s ON s.id=r.session_id
                        WHERE e.workspace_id=s.workspace_id
                          AND json_extract(e.envelope_json,'$.run_id')=c.run_id
                          AND json_extract(e.envelope_json,'$.event.type') IN ('checkpoint_started','checkpoint_reviewed')
                    );")?;
            }
            if !has_column(&transaction, "tool_calls", "approval_inputs_json")? {
                transaction.execute_batch("ALTER TABLE tool_calls ADD COLUMN approval_inputs_json TEXT CHECK(approval_inputs_json IS NULL OR (json_valid(approval_inputs_json) AND length(CAST(approval_inputs_json AS BLOB))<=1048576));")?;
            }
            crate::sessions::tool_calls::migrate_pending_approval_inputs(&transaction)?;
            transaction.execute_batch(
                "CREATE TRIGGER IF NOT EXISTS tool_calls_clear_approval_inputs
                AFTER UPDATE OF state, approval_resolution ON tool_calls
                WHEN OLD.approval_inputs_json IS NOT NULL
                 AND (NEW.state!='awaiting_approval' OR NEW.approval_resolution IS NOT NULL)
                BEGIN UPDATE tool_calls SET approval_inputs_json=NULL WHERE id=NEW.id; END;",
            )?;
        }
        transaction.execute(
            "UPDATE metadata SET value='61' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }
    if column_shape(&connection, "model_turns", "legacy_checkpoint_required")?
        != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if has_tool_calls
        && column_shape(&connection, "tool_calls", "approval_inputs_json")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }

    // 62: terminal receipts belong to runs, independently of delivery replay.
    if !matches!(schema_version.as_deref(), Some("62" | "63")) {
        let transaction = connection.transaction()?;
        if !has_column(&transaction, "runs", "completion_sequence")? {
            transaction.execute_batch("ALTER TABLE runs ADD COLUMN completion_sequence INTEGER NOT NULL DEFAULT 0 CHECK(completion_sequence>=0);
                ALTER TABLE runs ADD COLUMN completion_json TEXT CHECK((completion_json IS NULL AND completion_sequence=0) OR (completion_json IS NOT NULL AND completion_sequence>0 AND json_valid(completion_json) AND length(CAST(completion_json AS BLOB))<=1048576));
                CREATE INDEX runs_completion ON runs(session_id,completion_sequence) WHERE completion_sequence>0;")?;
        }
        crate::sessions::completions::migrate(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value='62' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }

    if column_shape(&connection, "runs", "completion_sequence")?
        != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
        || column_shape(&connection, "runs", "completion_json")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }

    // 63: snapshots carry the original hold details and delegate escalation.
    if schema_version.as_deref() != Some("63") {
        let transaction = connection.transaction()?;
        if has_tool_calls {
            if !has_column(&transaction, "tool_calls", "approval_escalation_json")? {
                transaction.execute_batch("ALTER TABLE tool_calls ADD COLUMN approval_escalation_json TEXT CHECK(approval_escalation_json IS NULL OR (json_valid(approval_escalation_json) AND length(CAST(approval_escalation_json AS BLOB))<=8192));")?;
            }
            transaction.execute_batch("DROP TRIGGER IF EXISTS tool_calls_clear_approval_inputs;
                CREATE TRIGGER tool_calls_clear_approval_inputs AFTER UPDATE OF state,approval_resolution ON tool_calls
                WHEN (OLD.approval_inputs_json IS NOT NULL OR OLD.approval_escalation_json IS NOT NULL)
                    AND (NEW.state!='awaiting_approval' OR NEW.approval_resolution IS NOT NULL)
                BEGIN UPDATE tool_calls SET approval_inputs_json=NULL,approval_escalation_json=NULL WHERE id=NEW.id; END;")?;
            crate::sessions::tool_calls::migrate_approval_escalations(&transaction)?;
        }
        if !has_column(&transaction, "messages", "chunks_ordered")? {
            transaction.execute_batch("ALTER TABLE messages ADD COLUMN chunks_ordered INTEGER NOT NULL DEFAULT 0 CHECK(chunks_ordered IN (0,1));")?;
        }
        transaction.execute_batch("CREATE INDEX IF NOT EXISTS message_chunks_delivery ON message_chunks(message_id,chunk_ordinal);")?;
        transaction.execute_batch("CREATE INDEX IF NOT EXISTS messages_run_reply ON messages(run_id,turn_ordinal DESC) WHERE role='assistant';")?;
        crate::sessions::events::initialize_retention(&transaction)?;
        transaction.execute(
            "UPDATE metadata SET value='63' WHERE key='schema_version'",
            [],
        )?;
        transaction.commit()?;
    }

    if column_shape(&connection, "messages", "chunks_ordered")?
        != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if has_tool_calls
        && column_shape(&connection, "tool_calls", "approval_escalation_json")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }

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
        .optional()?;
    let store_id = match stored {
        Some(value) => value.parse().map_err(|_| SessionRuntimeError::CODEC)?,
        None => {
            let id = StoreId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            connection.execute(
                "INSERT INTO metadata(key, value) VALUES ('store_id', ?1)",
                [id.to_string()],
            )?;
            id
        }
    };
    Ok((connection, store_id))
}

fn validate_owned_compaction_index(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = 'runs_owned_compaction' AND tbl_name = 'runs'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Err(SessionRuntimeError::CONSTRAINT);
    };
    if !sql.split_whitespace().eq(
        "CREATE INDEX runs_owned_compaction ON runs(auto_compaction_for_run_id) WHERE kind = 'compaction' AND auto_compaction_for_run_id IS NOT NULL".split_whitespace(),
    ) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn validate_goal_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (column, expected) in [
        ("goal_id", ("TEXT".to_owned(), false, None, 1)),
        ("session_id", ("TEXT".to_owned(), true, None, 0)),
        ("revision", ("INTEGER".to_owned(), true, None, 0)),
        ("live", ("INTEGER".to_owned(), true, None, 0)),
        ("payload_json", ("TEXT".to_owned(), true, None, 0)),
    ] {
        if column_shape(connection, "session_goals", column)? != expected {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    for (name, kind, expected) in [
        (
            "session_goals",
            "table",
            "CREATE TABLE session_goals ( goal_id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, revision INTEGER NOT NULL CHECK(revision > 0), live INTEGER NOT NULL CHECK(live IN (0, 1)), payload_json TEXT NOT NULL )",
        ),
        (
            "session_goals_live",
            "index",
            "CREATE UNIQUE INDEX session_goals_live ON session_goals(session_id) WHERE live = 1",
        ),
    ] {
        let sql: Option<String> = connection.query_row(
            "SELECT sql FROM sqlite_schema WHERE name = ?1 AND type = ?2 AND tbl_name = 'session_goals'",
            rusqlite::params![name, kind], |row| row.get(0),
        ).optional()?;
        let valid = sql.as_deref().is_some_and(|sql| {
            let mut actual: String = sql.split_whitespace().collect();
            if kind == "table" {
                // The base constraints survive schema58's private columns.
                for definition in [GOAL_POLICY_COLUMN, GOAL_SPECIFICATION_COLUMN] {
                    let addition =
                        format!(",{}", definition.split_whitespace().collect::<String>());
                    actual = actual.replace(&addition, "");
                }
            }
            actual == expected.split_whitespace().collect::<String>()
        });
        if !valid {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    for (column, kind) in [
        ("goal_id", "TEXT"),
        ("goal_revision", "INTEGER"),
        ("goal_snapshot_json", "TEXT"),
        ("origin_json", "TEXT"),
        ("goal_initial_notice", "TEXT"),
    ] {
        if column_shape(connection, "runs", column)? != (kind.to_owned(), false, None, 0) {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    for table in ["model_turns", "session_compactions"] {
        if has_table(connection, table)?
            && column_shape(connection, table, "goal_notice")?
                != ("TEXT".to_owned(), false, None, 0)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    Ok(())
}

fn add_run_limits_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "limits_json")? {
        connection.execute("ALTER TABLE runs ADD COLUMN limits_json TEXT", [])?;
    }
    Ok(())
}

/// Historical runs carry NULL: they were admitted without caller limits and
/// must never be reinterpreted under current defaults.
fn validate_run_limits_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "limits_json")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn add_context_occupancy_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "context_occupancy_json")? {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN context_occupancy_json TEXT",
            [],
        )?;
    }
    if !has_column(
        connection,
        "sessions",
        "pending_context_overflow_basis_json",
    )? {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN pending_context_overflow_basis_json TEXT",
            [],
        )?;
    }
    Ok(())
}

fn validate_context_occupancy_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "sessions", "context_occupancy_json")?
        != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if column_shape(
        connection,
        "sessions",
        "pending_context_overflow_basis_json",
    )? != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn add_preparing_run_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "preparing_run_id")? {
        connection.execute("ALTER TABLE sessions ADD COLUMN preparing_run_id TEXT", [])?;
    }
    if !has_column(
        connection,
        "sessions",
        "pending_context_overflow_model_json",
    )? {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN pending_context_overflow_model_json TEXT",
            [],
        )?;
    }
    if !has_column(connection, "runs", "context_compaction_attempted")? {
        connection.execute(
            "ALTER TABLE runs ADD COLUMN context_compaction_attempted INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(connection, "runs", "auto_compaction_for_run_id")? {
        connection.execute(
            "ALTER TABLE runs ADD COLUMN auto_compaction_for_run_id TEXT",
            [],
        )?;
    }
    connection.execute(
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
    )?;
    connection.execute(
        "UPDATE runs
             SET context_compaction_attempted = 1
             WHERE id IN (
                 SELECT auto_compaction_for_run_id
                 FROM runs
                 WHERE auto_compaction_for_run_id IS NOT NULL
             )",
        [],
    )?;
    Ok(())
}

fn validate_preparing_run_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let preparing = column_shape(connection, "sessions", "preparing_run_id")?;
    if preparing != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let overflow = column_shape(
        connection,
        "sessions",
        "pending_context_overflow_model_json",
    )?;
    if overflow != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let attempted = column_shape(connection, "runs", "context_compaction_attempted")?;
    if attempted != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let owner = column_shape(connection, "runs", "auto_compaction_for_run_id")?;
    if owner != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn column_shape(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<(String, bool, Option<String>, u8), SessionRuntimeError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u8>(5)?,
            ))
        })?
        .find_map(|row| match row {
            Ok((name, ty, not_null, default_value, primary_key)) if name == column => {
                Some(Ok((ty, not_null, default_value, primary_key)))
            }
            Ok(_) => None,
            Err(_) => Some(Err(SessionRuntimeError::CONSTRAINT)),
        })
        .unwrap_or(Err(SessionRuntimeError::CONSTRAINT))
}

/// Existing runs remain explicitly unknown: no current configuration is
/// guessed for historical work.
fn add_runs_resolved_model_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "resolved_model_json")? {
        connection.execute("ALTER TABLE runs ADD COLUMN resolved_model_json TEXT", [])?;
    }
    Ok(())
}

fn add_linear_streaming_storage(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection.execute_batch(
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
    )?;
    if !has_column(connection, "runs", "context_base_bytes")? {
        connection.execute("ALTER TABLE runs ADD COLUMN context_base_bytes INTEGER", [])?;
    }
    if !has_column(connection, "runs", "context_increment_bytes")? {
        connection.execute(
            "ALTER TABLE runs ADD COLUMN context_increment_bytes INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
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
        return Err(SessionRuntimeError::CONSTRAINT);
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
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
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
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    let mut columns = connection.prepare("PRAGMA table_info(message_chunks)")?;
    let columns = columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, u8>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
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
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let mut run_columns = connection.prepare("PRAGMA table_info(runs)")?;
    let run_columns = run_columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u8>(5)?,
            ))
        })?
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
        .collect::<Result<Vec<_>, _>>()?;
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
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let mut foreign_keys = connection.prepare("PRAGMA foreign_key_list(message_chunks)")?;
    let foreign_keys = foreign_keys
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if foreign_keys
        != [(
            "messages".to_owned(),
            "message_id".to_owned(),
            "id".to_owned(),
            "CASCADE".to_owned(),
        )]
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let schema_sql: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'message_chunks'",
        [],
        |row| row.get(0),
    )?;
    let schema_sql = schema_sql
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if !schema_sql.contains("check(channel in ('output', 'refusal'))")
        || !schema_sql.contains("check(chunk_ordinal > 0)")
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let mut promotion_columns =
        connection.prepare("PRAGMA table_info(pending_workspace_grant_promotions)")?;
    let promotion_columns = promotion_columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, u8>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
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
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let promotion_index: bool = connection.query_row(
        "SELECT EXISTS(
                 SELECT 1 FROM sqlite_schema
                 WHERE type = 'index'
                   AND name = 'pending_workspace_grant_promotions_fifo'
             )",
        [],
        |row| row.get(0),
    )?;
    if !promotion_index {
        return Err(SessionRuntimeError::CONSTRAINT);
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
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn add_model_turn_audit_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection.execute_batch(
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
    )?;
    for (column, declaration) in [
        ("model_json", "TEXT"),
        ("usage_json", "TEXT"),
        ("estimated_cost_usd_nanos", "INTEGER"),
        ("completed_at_ms", "INTEGER"),
    ] {
        if !has_column(connection, "model_turns", column)? {
            connection.execute(
                &format!("ALTER TABLE model_turns ADD COLUMN {column} {declaration}"),
                [],
            )?;
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
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    Ok(())
}

/// Adds `messages.turn_ordinal` for stores created before per-turn assistant
/// messages. Existing rows keep 0: legacy runs render as one message with the
/// run's calls grouped after it.
fn add_messages_turn_ordinal_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "messages", "turn_ordinal")? {
        connection.execute(
            "ALTER TABLE messages ADD COLUMN turn_ordinal INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Adds `tool_calls.display_json` for stores created before tool calls
/// carried a UI display payload. Existing rows keep NULL: legacy results
/// render from their bounded result string alone.
fn add_tool_calls_display_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "tool_calls", "display_json")? {
        connection.execute("ALTER TABLE tool_calls ADD COLUMN display_json TEXT", [])?;
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
                 source TEXT NOT NULL DEFAULT 'human',
                 run_id TEXT,
                 UNIQUE(session_id, kind, value)
             );",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
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
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
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
                 created_at_ms INTEGER NOT NULL,
                 scope_run_id TEXT,
                 turn_cutoff INTEGER
             );
             CREATE INDEX session_compactions_session
                 ON session_compactions(session_id, created_at_ms DESC);",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

/// Adds `runs.kind` for stores created before internal compaction runs.
/// Existing rows keep 'prompt'.
fn add_runs_kind_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "kind")? {
        connection.execute(
            "ALTER TABLE runs ADD COLUMN kind TEXT NOT NULL DEFAULT 'prompt'",
            [],
        )?;
    }
    Ok(())
}

/// Adds `runs.context_tokens` for stores created before the last model
/// turn's input-token total was persisted separately from the run's summed
/// billing usage. Existing rows keep NULL: clients fall back to the sum.
fn add_runs_context_tokens_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "context_tokens")? {
        connection.execute("ALTER TABLE runs ADD COLUMN context_tokens INTEGER", [])?;
    }
    Ok(())
}

/// Adds authoritative session occupancy without guessing from historical
/// run billing totals. Existing sessions remain unknown until a prompt turn
/// reports usage.
fn add_sessions_context_tokens_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "context_tokens")? {
        connection.execute("ALTER TABLE sessions ADD COLUMN context_tokens INTEGER", [])?;
    }
    Ok(())
}

/// Records the parent `spawn_agent` tool call that created a child, so clients
/// can place the child under its call. Historical children stay NULL.
fn add_sessions_spawned_by_tool_call_column(
    connection: &Connection,
) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "spawned_by_tool_call_id")? {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN spawned_by_tool_call_id TEXT",
            [],
        )?;
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
            connection.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// Records output truncation state. Historical rows read back as never
/// truncated and zero continuations, which is what they were.
fn add_output_truncation_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (table, column) in OUTPUT_TRUNCATION_COLUMNS {
        if !has_column(connection, table, column)? {
            connection.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"),
                [],
            )?;
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
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    Ok(())
}

/// Records the parent run that owns a spawned child. Publicly created child
/// sessions and historical rows remain unowned (NULL).
fn add_sessions_owner_run_id_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "owner_run_id")? {
        connection.execute("ALTER TABLE sessions ADD COLUMN owner_run_id TEXT", [])?;
    }
    Ok(())
}

/// Adds `runs.auto_compaction` for stores created before threshold-triggered
/// compaction. 1 marks a compaction run the runtime claimed automatically at
/// a context threshold; 0 (every existing row) is a user-requested run.
fn add_runs_auto_compaction_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "auto_compaction")? {
        connection.execute(
            "ALTER TABLE runs ADD COLUMN auto_compaction INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Adds the stable prompt and workspace-instruction identity prepared before
/// provider work. Existing runs remain unknown rather than being assigned the
/// current prompt after the fact.
fn add_runs_prompt_identity_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "prompt_identity_json")? {
        connection.execute("ALTER TABLE runs ADD COLUMN prompt_identity_json TEXT", [])?;
    }
    Ok(())
}

pub(in crate::sessions) fn has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, SessionRuntimeError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|candidate| candidate == column))
}

/// Records where a session sits in its delegation tree: `depth` (0 for a
/// root, 1 for a child, ...) and `root_run_id`, the root run whose subtree it
/// belongs to. Historical children are backfilled at depth 1 under their
/// owner run, which is exactly what they were when depth was structurally
/// one; deeper history does not exist.
fn add_session_depth_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "depth")? {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(connection, "sessions", "root_run_id")? {
        connection.execute("ALTER TABLE sessions ADD COLUMN root_run_id TEXT", [])?;
    }
    connection.execute(
        "UPDATE sessions SET depth = 1, root_run_id = owner_run_id
             WHERE owner_run_id IS NOT NULL AND root_run_id IS NULL",
        [],
    )?;
    Ok(())
}

fn validate_session_depth_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let depth = column_shape(connection, "sessions", "depth")?;
    if depth != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let root = column_shape(connection, "sessions", "root_run_id")?;
    if root != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

/// Records why a session exists (`task` or `audit`) and how a run's final
/// answer was audited. Historical rows are ordinary task sessions with no
/// audit, which is what they were.
fn add_audit_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "sessions", "purpose")? {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN purpose TEXT NOT NULL DEFAULT 'task'",
            [],
        )?;
    }
    if !has_column(connection, "runs", "audit_json")? {
        connection.execute("ALTER TABLE runs ADD COLUMN audit_json TEXT", [])?;
    }
    Ok(())
}

fn validate_audit_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "sessions", "purpose")?
        != ("TEXT".to_owned(), true, Some("'task'".to_owned()), 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if column_shape(connection, "runs", "audit_json")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
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
        connection.execute("ALTER TABLE runs ADD COLUMN activity TEXT", [])?;
    }
    connection.execute(
        "INSERT OR REPLACE INTO metadata(key, value)
             VALUES ('command_count', (SELECT COUNT(*) FROM commands))",
        [],
    )?;
    Ok(())
}

/// Schema 26: `tool_calls.effect` is the catalog effect class the call was
/// admitted with (`read_only`, `mutating`, `shell`, `external`), so context
/// assembly decides what it may prune from the stored kind rather than from
/// the tool name. Null for calls recorded before this version; assembly falls
/// back to the built-in read-only names for those rows.
fn add_tool_call_effect_column(connection: &Connection) -> Result<(), SessionRuntimeError> {
    // Every store created since schema 10 has the table; the guard keeps the
    // migration a no-op on partial historical stores that never recorded a
    // tool call (test fixtures), where there is nothing to annotate.
    if has_table(connection, "tool_calls")? && !has_column(connection, "tool_calls", "effect")? {
        connection.execute("ALTER TABLE tool_calls ADD COLUMN effect TEXT", [])?;
    }
    Ok(())
}

fn validate_tool_call_effect_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if has_table(connection, "tool_calls")?
        && column_shape(connection, "tool_calls", "effect")? != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

/// Complete tool outputs whose model-facing text was cut. One row per tool
/// call, keyed by the call so the result row and its spill are written in
/// the same transaction and deleted together. `digest` is the SHA-256 of
/// `content`; the handle the marker carries is `t:<tool>:<call8>:<digest8>`.
/// `evicted_at_ms` marks a row whose content the per-session cap reclaimed;
/// the row stays so a read of the handle answers `spill_evicted`, not
/// `spill_missing`.
fn create_tool_spills_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if has_table(connection, "tool_spills")? {
        return Ok(());
    }
    connection
        .execute_batch(
            "CREATE TABLE tool_spills (
                 tool_call_id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 run_id TEXT NOT NULL,
                 tool TEXT NOT NULL,
                 digest TEXT NOT NULL,
                 content BLOB,
                 content_bytes INTEGER NOT NULL,
                 omitted_from_line INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 evicted_at_ms INTEGER
             );
             CREATE INDEX tool_spills_session_created
                 ON tool_spills(session_id, created_at_ms);",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn validate_tool_spills_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (column, kind) in [
        ("session_id", "TEXT"),
        ("digest", "TEXT"),
        ("content_bytes", "INTEGER"),
        ("omitted_from_line", "INTEGER"),
    ] {
        if column_shape(connection, "tool_spills", column)? != (kind.to_owned(), true, None, 0) {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    Ok(())
}

/// The file bytes a prompt attached, exactly as the model saw them, so a
/// later run reconstructs the same request after the file changes or
/// disappears. `message_attachments` has one row per `<attached-file>`
/// block in prompt order; `attachment_blobs` holds the bytes once per
/// `(session, digest, window)` so a file attached to several prompts is
/// stored once. `evicted_at_ms` marks a blob the per-session cap reclaimed;
/// the row stays so reconstruction renders an explicit evicted block rather
/// than the bare `@path` placeholder.
fn create_attachment_tables(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if has_table(connection, "attachment_blobs")? && has_table(connection, "message_attachments")? {
        return Ok(());
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS attachment_blobs (
                 session_id TEXT NOT NULL,
                 blob_key TEXT NOT NULL,
                 content BLOB,
                 content_bytes INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 evicted_at_ms INTEGER,
                 PRIMARY KEY (session_id, blob_key)
             );
             CREATE TABLE IF NOT EXISTS message_attachments (
                 message_id TEXT NOT NULL,
                 ordinal INTEGER NOT NULL,
                 session_id TEXT NOT NULL,
                 path TEXT NOT NULL,
                 digest TEXT NOT NULL,
                 blob_key TEXT NOT NULL,
                 window_start INTEGER,
                 window_end INTEGER,
                 window_total INTEGER,
                 PRIMARY KEY (message_id, ordinal)
             );
             CREATE INDEX IF NOT EXISTS message_attachments_session
                 ON message_attachments(session_id);",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn validate_attachment_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (column, kind) in [
        ("session_id", "TEXT"),
        ("blob_key", "TEXT"),
        ("content_bytes", "INTEGER"),
        ("created_at_ms", "INTEGER"),
    ] {
        let (shape, not_null, _, _) = column_shape(connection, "attachment_blobs", column)?;
        if shape != kind || !not_null {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    for (column, kind) in [
        ("message_id", "TEXT"),
        ("ordinal", "INTEGER"),
        ("session_id", "TEXT"),
        ("path", "TEXT"),
        ("digest", "TEXT"),
        ("blob_key", "TEXT"),
    ] {
        let (shape, not_null, _, _) = column_shape(connection, "message_attachments", column)?;
        if shape != kind || !not_null {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    Ok(())
}

/// Every per-run message lookup — the steering rows context assembly joins
/// to each retained prompt, the queued prompt a run start completes, the
/// streaming assistant row settlement finalizes — filtered `messages` by
/// `run_id` through a table scan before schema 30. Scan cost grew with the
/// whole session's message count, which made assembly of a fixed retained
/// context slow down as the archive behind the compaction cutoff grew (F06).
fn create_messages_run_index(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS messages_run_steering
                 ON messages(run_id, steering, state);",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn validate_messages_run_index(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let present: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
                       WHERE type = 'index' AND name = 'messages_run_steering')",
        [],
        |row| row.get(0),
    )?;
    if present {
        Ok(())
    } else {
        Err(SessionRuntimeError::CONSTRAINT)
    }
}

/// One non-blocking child (ADR-0054 § 4), inserted atomically with the
/// child's admission. Delivery stamps the row exactly once: the notice text
/// the parent's context replays, and the parent turn whose request first
/// carried it (`turn_ordinal`; NULL when the parent settled first, so the
/// notice follows that run). `child_run_id` has no foreign key: a delivered
/// answer is the parent's context and outlives the child session.
fn create_child_deliveries_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS child_deliveries (
                 child_run_id TEXT PRIMARY KEY,
                 parent_session_id TEXT NOT NULL REFERENCES sessions(id),
                 parent_run_id TEXT NOT NULL REFERENCES runs(id),
                 created_at_ms INTEGER NOT NULL,
                 delivered_at_ms INTEGER,
                 delivery_ordinal INTEGER,
                 turn_ordinal INTEGER,
                 text TEXT
             );
             CREATE INDEX IF NOT EXISTS child_deliveries_parent_run
                 ON child_deliveries(parent_run_id, delivery_ordinal);",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn validate_child_deliveries_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let expected = [
        ("child_run_id", ("TEXT".to_owned(), false, None, 1)),
        ("parent_session_id", ("TEXT".to_owned(), true, None, 0)),
        ("parent_run_id", ("TEXT".to_owned(), true, None, 0)),
        ("created_at_ms", ("INTEGER".to_owned(), true, None, 0)),
        ("delivered_at_ms", ("INTEGER".to_owned(), false, None, 0)),
        ("delivery_ordinal", ("INTEGER".to_owned(), false, None, 0)),
        ("turn_ordinal", ("INTEGER".to_owned(), false, None, 0)),
        ("text", ("TEXT".to_owned(), false, None, 0)),
    ];
    for (column, shape) in expected {
        if column_shape(connection, "child_deliveries", column)? != shape {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    // Delivery and assembly read by parent run in delivery order.
    let indexed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
                       WHERE type = 'index' AND name = 'child_deliveries_parent_run')",
        [],
        |row| row.get(0),
    )?;
    if !indexed {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn create_child_reports_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS child_reports (
                 child_run_id TEXT NOT NULL,
                 child_turn_ordinal INTEGER NOT NULL,
                 parent_session_id TEXT NOT NULL REFERENCES sessions(id),
                 parent_run_id TEXT NOT NULL REFERENCES runs(id),
                 delivered_at_ms INTEGER NOT NULL,
                 delivery_ordinal INTEGER NOT NULL,
                 turn_ordinal INTEGER NOT NULL,
                 text TEXT NOT NULL,
                 PRIMARY KEY(child_run_id, child_turn_ordinal)
             );
             CREATE INDEX IF NOT EXISTS child_reports_parent_run
                 ON child_reports(parent_run_id, delivery_ordinal);",
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn validate_child_reports_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let expected = [
        ("child_run_id", ("TEXT".to_owned(), true, None, 1)),
        ("child_turn_ordinal", ("INTEGER".to_owned(), true, None, 2)),
        ("parent_session_id", ("TEXT".to_owned(), true, None, 0)),
        ("parent_run_id", ("TEXT".to_owned(), true, None, 0)),
        ("delivered_at_ms", ("INTEGER".to_owned(), true, None, 0)),
        ("delivery_ordinal", ("INTEGER".to_owned(), true, None, 0)),
        ("turn_ordinal", ("INTEGER".to_owned(), true, None, 0)),
        ("text", ("TEXT".to_owned(), true, None, 0)),
    ];
    for (column, shape) in expected {
        if column_shape(connection, "child_reports", column)? != shape {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    let indexed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
                       WHERE type = 'index' AND name = 'child_reports_parent_run')",
        [],
        |row| row.get(0),
    )?;
    if !indexed {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn has_table(connection: &Connection, table: &str) -> Result<bool, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

fn validate_fast_path_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "activity")? != ("TEXT".to_owned(), false, None, 0) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let counted: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'command_count'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if counted.is_none() {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

/// Schema 27: the typed-output contract. `runs.output_contract_json` is the
/// `OutputContract` a prompt was submitted with, so a claim (including one
/// after restart) enforces the contract the caller accepted;
/// `runs.final_output_json` is the durable `FinalOutput` verdict, written in
/// the settlement transaction before `run_finished` is published. Both are
/// null for runs without a contract and for every historical row.
fn add_output_contract_columns(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if !has_column(connection, "runs", "output_contract_json")? {
        connection.execute("ALTER TABLE runs ADD COLUMN output_contract_json TEXT", [])?;
    }
    if !has_column(connection, "runs", "final_output_json")? {
        connection.execute("ALTER TABLE runs ADD COLUMN final_output_json TEXT", [])?;
    }
    Ok(())
}

fn validate_output_contract_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for column in ["output_contract_json", "final_output_json"] {
        if column_shape(connection, "runs", column)? != ("TEXT".to_owned(), false, None, 0) {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    Ok(())
}

fn validate_goal_request_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "goal_request_pending")?
        != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
        || column_shape(connection, "runs", "goal_helper_admitted")?
            != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
        || column_shape(connection, "model_turns", "work_disposition")?
            != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn validate_check_acknowledgment_schema(
    connection: &Connection,
) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "check_acknowledgment_pending")?
        != ("INTEGER".to_owned(), true, Some("0".to_owned()), 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let table: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='runs'",
        [],
        |row| row.get(0),
    )?;
    let compact: String = table.split_whitespace().collect();
    if !compact.contains("check_acknowledgment_pendingINTEGERNOTNULLDEFAULT0CHECK(check_acknowledgment_pendingIN(0,1)AND(check_acknowledgment_pending=0ORkind='goal_check'))") {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let index: Option<String> = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='index' AND name='runs_pending_check_ack' AND tbl_name='runs'",
        [], |row| row.get(0),
    ).optional()?;
    if !index.as_deref().is_some_and(|sql| sql.split_whitespace().eq(
        "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1".split_whitespace()
    )) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn validate_goal_paid_owner_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    if column_shape(connection, "runs", "goal_paid_owner_run_id")?
        != ("TEXT".to_owned(), false, None, 0)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

fn restore_pending_paid_owners(connection: &Connection) -> Result<(), SessionRuntimeError> {
    use crate::sessions::MAX_CHILD_DEPTH;
    use qq_protocol::{RunId, RunOrigin};
    let mut pending = connection.prepare(
        "SELECT r.id FROM runs r JOIN sessions s ON s.id=r.session_id
         JOIN messages m ON m.id=r.user_message_id AND m.run_id=r.id
           AND m.session_id=r.session_id AND m.ordinal=1 AND m.role='user'
         WHERE r.kind='prompt' AND (r.status IN ('queued','running') OR EXISTS(
               SELECT 1 FROM runs helper WHERE helper.kind='compaction' AND helper.auto_compaction=1
                 AND helper.auto_compaction_for_run_id=r.id AND helper.session_id=r.session_id
                 AND helper.status IN ('queued','running')))
           AND s.owner_run_id IS NOT NULL AND r.origin_json IS NOT NULL",
    )?;
    let mut rows = pending.query([])?;
    while let Some(row) = rows.next()? {
        let mut run: String = row.get(0)?;
        let mut edges = Vec::new();
        let mut required = 0;
        let mut expected = None;
        let mut finished = false;
        for _ in 0..=MAX_CHILD_DEPTH {
            let (session,workspace,depth,parent,owner,origin,kind,initial,goal)=connection.query_row(
                "SELECT r.session_id,s.workspace_id,s.depth,s.parent_id,s.owner_run_id,
                    r.origin_json,r.kind,EXISTS(SELECT 1 FROM messages m WHERE m.id=r.user_message_id
                    AND m.run_id=r.id AND m.session_id=r.session_id AND m.ordinal=1 AND m.role='user'),
                    r.goal_id IS NOT NULL
                 FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1",
                [&run],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u16>(2)?,
                    row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,Option<String>>(5)?,
                    row.get::<_,String>(6)?,row.get::<_,bool>(7)?,row.get::<_,bool>(8)?)),
            )?;
            if kind != "prompt"
                || depth > MAX_CHILD_DEPTH
                || expected
                    .as_ref()
                    .is_some_and(|expected| expected != &(session, workspace.clone(), depth))
            {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            if goal {
                required = edges.len();
            }
            let origin = origin
                .as_deref()
                .map(serde_json::from_str::<RunOrigin>)
                .transpose()?;
            let Some(RunOrigin::Delegated { owner_run_id, .. }) = origin else {
                finished = true;
                break;
            };
            let anchor = owner_run_id.to_string();
            if !initial
                || depth == 0
                || owner.as_deref() != Some(anchor.as_str())
                || run == anchor
                || edges.iter().any(|(previous, _)| previous == &anchor)
            {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            let parent = parent.ok_or(SessionRuntimeError::CONSTRAINT)?;
            expected = Some((parent, workspace, depth - 1));
            edges.push((run, anchor.clone()));
            run = anchor;
        }
        if !finished {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        for (child, owner) in edges.into_iter().take(required) {
            connection.execute(
                "UPDATE runs SET goal_paid_owner_run_id=?2 WHERE id=?1",
                [child, owner],
            )?;
        }
        // Validate the restored chain with the same rules used for claims and settlement.
        let seed: String = row.get(0)?;
        let seed: RunId = seed.parse().map_err(|_| SessionRuntimeError::CONSTRAINT)?;
        crate::sessions::goals::paid_goal_owners(connection, seed)?;
    }
    Ok(())
}

fn restore_unfinished_model_work(connection: &Connection) -> Result<(), SessionRuntimeError> {
    let mut statement = connection.prepare(
        "SELECT t.rowid,t.assistant_content_json,t.usage_json,t.truncated
         FROM model_turns t JOIN runs r ON r.id=t.run_id
         WHERE r.outcome_json IS NULL AND t.work_disposition IS NULL ORDER BY t.rowid",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let content = row
            .get_ref(1)?
            .as_str()
            .map_err(|_| SessionRuntimeError::CONSTRAINT)?;
        let usage = row
            .get_ref(2)?
            .as_str_or_null()
            .map_err(|_| SessionRuntimeError::CONSTRAINT)?;
        // Match the durable transcript's 64 MiB envelope budget before decoding.
        if content.len().saturating_add(usage.map_or(0, str::len)) > 64 * 1024 * 1024 {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let truncated: bool = row.get(3)?;
        let content = serde_json::from_str::<crate::sessions::codec::PersistedTurn>(content)?;
        let has_content = match content {
            crate::sessions::codec::PersistedTurn::Legacy(content) => !content.is_empty(),
            crate::sessions::codec::PersistedTurn::Replay { content, replay } => {
                !content.is_empty() || !replay.is_empty()
            }
        };
        let usage = usage
            .map(serde_json::from_str::<qq_protocol::TokenUsage>)
            .transpose()?;
        let disposition = if has_content || usage.is_some() || truncated {
            "committed"
        } else {
            "uncertain"
        };
        connection.execute(
            "UPDATE model_turns SET work_disposition=?1 WHERE rowid=?2 AND work_disposition IS NULL",
            rusqlite::params![disposition, id],
        )?;
    }
    Ok(())
}

/// Reconstruct unresolved sends without retaining a transcript-sized event
/// buffer or rescanning all events for every unfinished run.
fn restore_unfinished_goal_dispatch(connection: &Connection) -> Result<(), SessionRuntimeError> {
    use qq_protocol::{RunActivity, SessionEvent, SessionEventEnvelope};
    connection.execute_batch(
        "CREATE TEMP TABLE goal_dispatch_repair (
        id TEXT PRIMARY KEY, session TEXT NOT NULL, workspace TEXT NOT NULL,
        waiting INTEGER NOT NULL DEFAULT 0, completed INTEGER NOT NULL DEFAULT 0
    );
    INSERT INTO goal_dispatch_repair(id,session,workspace)
    SELECT r.id,r.session_id,s.workspace_id FROM runs r JOIN sessions s ON s.id=r.session_id
    WHERE r.outcome_json IS NULL AND r.status IN ('queued','running')
      AND ((r.kind='prompt' AND (r.goal_id IS NOT NULL OR r.goal_paid_owner_run_id IS NOT NULL))
        OR (r.kind='compaction' AND r.auto_compaction=1 AND EXISTS(
          SELECT 1 FROM runs owner WHERE owner.id=r.auto_compaction_for_run_id
            AND owner.session_id=r.session_id AND owner.kind='prompt'
            AND (owner.goal_id IS NOT NULL OR owner.goal_paid_owner_run_id IS NOT NULL))));",
    )?;
    let any_candidates: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM goal_dispatch_repair)",
        [],
        |row| row.get(0),
    )?;
    if !any_candidates {
        connection.execute_batch("DROP TABLE goal_dispatch_repair")?;
        return Ok(());
    }
    // The subsequent scan is global so mismatched-workspace evidence cannot
    // silently disappear. Bound every scanned envelope before JSON parsing.
    let oversized: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE length(CAST(envelope_json AS BLOB))>1048576)",
        [],
        |row| row.get(0),
    )?;
    if oversized {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let store_id: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key='store_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let mut statement = connection.prepare(
        "SELECT e.workspace_id,e.sequence,e.envelope_json,c.id,c.session,c.workspace
         FROM events e JOIN goal_dispatch_repair c
           ON json_extract(e.envelope_json,'$.run_id')=c.id
         WHERE json_extract(e.envelope_json,'$.event.type') IN ('run_activity_changed','model_turn_completed')
         ORDER BY e.workspace_id,e.sequence",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let workspace: String = row.get(0)?;
        let sequence: u64 = row.get(1)?;
        let encoded = row
            .get_ref(2)?
            .as_str()
            .map_err(|_| SessionRuntimeError::CONSTRAINT)?;
        if encoded.len() > 1024 * 1024 {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let run: String = row.get(3)?;
        let session: String = row.get(4)?;
        let expected_workspace: String = row.get(5)?;
        let envelope: SessionEventEnvelope = serde_json::from_str(encoded)?;
        if workspace != expected_workspace
            || store_id
                .as_ref()
                .is_none_or(|stored| envelope.cursor.store_id.to_string() != *stored)
            || envelope.cursor.workspace_id.to_string() != workspace
            || envelope.cursor.sequence != sequence
            || envelope.session_id.to_string() != session
            || envelope.run_id.map(|id| id.to_string()).as_deref() != Some(run.as_str())
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let column = match envelope.event {
            SessionEvent::RunActivityChanged { run_id, activity } => {
                if run_id.to_string() != run {
                    return Err(SessionRuntimeError::CONSTRAINT);
                }
                if activity != RunActivity::WaitingForProvider {
                    continue;
                }
                "waiting"
            }
            SessionEvent::ModelTurnCompleted { run_id, .. } => {
                if run_id.to_string() != run {
                    return Err(SessionRuntimeError::CONSTRAINT);
                }
                "completed"
            }
            _ => return Err(SessionRuntimeError::CONSTRAINT),
        };
        connection.execute(
            &format!("UPDATE goal_dispatch_repair SET {column}=?2 WHERE id=?1"),
            rusqlite::params![run, sequence],
        )?;
    }
    drop(rows);
    drop(statement);
    connection.execute_batch(
        "UPDATE runs SET goal_request_pending=1,usage_json=NULL,estimated_cost_usd_nanos=NULL
        WHERE id IN (SELECT id FROM goal_dispatch_repair WHERE waiting>completed);
    UPDATE runs SET goal_request_pending=1,goal_helper_admitted=1
        WHERE id IN (SELECT id FROM goal_dispatch_repair) AND kind='compaction'
          AND usage_json IS NULL AND estimated_cost_usd_nanos IS NULL;
    UPDATE runs SET goal_helper_admitted=1
        WHERE id IN (SELECT id FROM goal_dispatch_repair) AND routing_json IS NOT NULL
          AND (usage_json IS NULL OR estimated_cost_usd_nanos IS NULL
            OR estimated_cost_usd_nanos>0 OR json_extract(usage_json,'$.input_tokens')>0
            OR json_extract(usage_json,'$.output_tokens')>0);
    DROP TABLE goal_dispatch_repair;",
    )?;
    Ok(())
}

fn validate_goal_policy_schema(connection: &Connection) -> Result<(), SessionRuntimeError> {
    for (column, kind) in [("policy_json", "TEXT"), ("specification_at_ms", "INTEGER")] {
        if column_shape(connection, "session_goals", column)? != (kind.to_owned(), false, None, 0) {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    let sql: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='session_goals'",
        [],
        |row| row.get(0),
    )?;
    let compact: String = sql.split_whitespace().collect();
    if [GOAL_POLICY_COLUMN, GOAL_SPECIFICATION_COLUMN]
        .into_iter()
        .any(|definition| !compact.contains(&definition.split_whitespace().collect::<String>()))
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}
