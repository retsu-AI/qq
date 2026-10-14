use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
use qq_core::classify_bench::{MAX_READ_COMMAND_BYTES, read_only_shell_command};
use qq_protocol::{RunId, StoreId, ToolCallId, ToolCallState};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{Date, OffsetDateTime};

const FORMAT: u16 = 1;
const DAY_MS: i64 = 86_400_000;
const MAX_RECORD_BYTES: usize = 128 * 1024;
const MAX_ARGUMENT_BYTES: u64 = 256 * 1024;
const MAX_RECORDS: u64 = 1_000_000;
const COHORT: &str = "FROM tool_calls t JOIN runs r ON r.id=t.run_id JOIN sessions s ON s.id=r.session_id WHERE s.parent_id IS NULL AND r.kind='prompt' AND r.started_at_ms>=?1 AND r.started_at_ms<?2 AND r.finished_at_ms<?3 AND t.name='shell'";

#[derive(Debug, Args)]
pub(crate) struct ActionBiasArgs {
    #[command(subcommand)]
    command: ActionBiasCommand,
}

#[derive(Debug, Subcommand)]
enum ActionBiasCommand {
    /// Export all shell calls in a fixed root prompt cohort from a read-only snapshot.
    Export {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long)]
        cutoff: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Recount a complete export with this immutable classifier executable.
    Recount {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Error)]
pub(crate) enum ActionBiasError {
    #[error("qualification I/O failed")]
    Io(#[from] io::Error),
    #[error("read-only qualification query failed")]
    Sql(#[from] rusqlite::Error),
    #[error("qualification JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("qualification output could not be published without overwriting a file")]
    Publish(#[from] tempfile::PersistError),
    #[error("qualification is unqualified: {0}")]
    Unqualified(Validation),
}

#[derive(Debug, Error)]
pub(crate) enum Validation {
    #[error("dates must be YYYY-MM-DD and start < end <= cutoff")]
    Window,
    #[error("the observation cutoff has not elapsed")]
    FutureCutoff,
    #[error("store identity or schema metadata is invalid or unsupported")]
    Metadata,
    #[error("the cohort is empty or exceeds the bounded record count")]
    Count,
    #[error(
        "an argument envelope exceeds the inspection bound; no read-share result can be issued"
    )]
    Envelope,
    #[error("a stored call ID or state is invalid")]
    Call,
    #[error("a record exceeds the serialization bound")]
    RecordBound,
    #[error("every record must end in a newline")]
    Newline,
    #[error("header, calls and footer must occur once in that order")]
    Framing,
    #[error("call IDs must be unique and strictly increasing")]
    Order,
    #[error("record count or snapshot digest does not match the footer/header")]
    Integrity,
    #[error("classifier-rejected status does not match the classifier's byte bound")]
    CommandBound,
    #[error("build with QQ_ACTION_BIAS_SOURCE_SHA set to the immutable source commit")]
    BuildIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format: u16,
    store_id: StoreId,
    schema: u16,
    start_ms: i64,
    end_ms: i64,
    cutoff_ms: i64,
    expected_records: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum CommandInput {
    Command { text: String },
    ClassifierRejected { command_bytes: u64 },
    Malformed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExportLine {
    Header(Header),
    Call {
        id: ToolCallId,
        run_id: RunId,
        state: ToolCallState,
        command: CommandInput,
    },
    Footer {
        records: u64,
        snapshot_sha256: String,
    },
}

#[derive(Debug, Serialize)]
struct Recount {
    format: u16,
    header: Header,
    snapshot_sha256: String,
    total_calls: u64,
    read_calls: u64,
    classifier_rejected: u64,
    malformed_commands: u64,
    read_share: f64,
}

#[derive(Serialize)]
struct Receipt {
    source_commit: String,
    classifier_source_sha256: String,
    executable_sha256: String,
    classifier_platform: &'static str,
    classifier_arch: &'static str,
    recount: Recount,
}

pub(crate) fn run(args: ActionBiasArgs) -> Result<(), ActionBiasError> {
    match args.command {
        ActionBiasCommand::Export {
            store,
            start,
            end,
            cutoff,
            output,
        } => {
            let start_ms = midnight(&start)?;
            let end_ms = midnight(&end)?;
            let cutoff_ms = midnight(&cutoff)?;
            validate_window(start_ms, end_ms, cutoff_ms)?;
            export(&store, start_ms, end_ms, cutoff_ms, &output)
        }
        ActionBiasCommand::Recount { input, output } => {
            let source = option_env!("QQ_ACTION_BIAS_SOURCE_SHA")
                .ok_or(ActionBiasError::Unqualified(Validation::BuildIdentity))?;
            if !matches!(source.len(), 40 | 64)
                || !source
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(ActionBiasError::Unqualified(Validation::BuildIdentity));
            }
            let recount = recount(&input)?;
            let mut classifier = Sha256::new();
            for source in [
                include_str!("../../crates/qq-core/src/approval.rs"),
                include_str!("../../crates/qq-core/src/approval/rules.rs"),
                include_str!("../../crates/qq-core/src/approval/classify.rs"),
            ] {
                classifier.update((source.len() as u64).to_le_bytes());
                classifier.update(source.as_bytes());
            }
            #[cfg(target_os = "linux")]
            let executable = PathBuf::from("/proc/self/exe");
            #[cfg(not(target_os = "linux"))]
            let executable = std::env::current_exe()?;
            let mut file = File::open(executable)?;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let bytes = file.read(&mut buffer)?;
                if bytes == 0 {
                    break;
                }
                digest.update(&buffer[..bytes]);
            }
            let receipt = Receipt {
                source_commit: source.to_owned(),
                classifier_source_sha256: format!("{:x}", classifier.finalize()),
                executable_sha256: format!("{:x}", digest.finalize()),
                classifier_platform: std::env::consts::OS,
                classifier_arch: std::env::consts::ARCH,
                recount,
            };
            let mut out = temporary_output(&output)?;
            serde_json::to_writer_pretty(out.as_file_mut(), &receipt)?;
            out.as_file_mut().write_all(b"\n")?;
            publish(out, &output)
        }
    }
}

fn midnight(value: &str) -> Result<i64, ActionBiasError> {
    let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
        .map_err(|_| ActionBiasError::Unqualified(Validation::Window))?;
    let date = Date::parse(value, &format)
        .map_err(|_| ActionBiasError::Unqualified(Validation::Window))?;
    Ok(date.midnight().assume_utc().unix_timestamp() * 1000)
}

fn validate_window(start: i64, end: i64, cutoff: i64) -> Result<(), ActionBiasError> {
    if start >= end
        || end.checked_add(DAY_MS) != Some(cutoff)
        || [start, end, cutoff].iter().any(|value| value % DAY_MS != 0)
    {
        return Err(ActionBiasError::Unqualified(Validation::Window));
    }
    if i128::from(cutoff) > OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000 {
        return Err(ActionBiasError::Unqualified(Validation::FutureCutoff));
    }
    Ok(())
}

fn temporary_output(output: &Path) -> Result<tempfile::NamedTempFile, ActionBiasError> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tempfile::NamedTempFile::new_in(parent).map_err(Into::into)
}

fn publish(out: tempfile::NamedTempFile, output: &Path) -> Result<(), ActionBiasError> {
    out.as_file().sync_all()?;
    out.persist_noclobber(output)?;
    Ok(())
}

fn write_line(
    out: &mut File,
    value: &ExportLine,
    digest: &mut Sha256,
) -> Result<(), ActionBiasError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ActionBiasError::Unqualified(Validation::RecordBound));
    }
    out.write_all(&bytes)?;
    digest.update(&bytes);
    Ok(())
}

fn export(
    store: &Path,
    start_ms: i64,
    end_ms: i64,
    cutoff_ms: i64,
    output: &Path,
) -> Result<(), ActionBiasError> {
    validate_window(start_ms, end_ms, cutoff_ms)?;
    let mut connection = Connection::open_with_flags(store, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    let transaction = connection.transaction()?;
    let schema: String = transaction.query_row("SELECT substr(value,1,8) FROM metadata WHERE key='schema_version' AND length(CAST(value AS BLOB))<=8", [], |row| row.get(0))?;
    let schema: u16 = schema
        .parse()
        .map_err(|_| ActionBiasError::Unqualified(Validation::Metadata))?;
    if !matches!(schema, 45 | 46) {
        return Err(ActionBiasError::Unqualified(Validation::Metadata));
    }
    let store_id: String = transaction.query_row(
        "SELECT value FROM metadata WHERE key='store_id' AND length(CAST(value AS BLOB))=32",
        [],
        |row| row.get(0),
    )?;
    let store_id = store_id
        .parse()
        .map_err(|_| ActionBiasError::Unqualified(Validation::Metadata))?;
    let expected_records: u64 = transaction.query_row(
        &format!("SELECT count(*) {COHORT}"),
        params![start_ms, end_ms, cutoff_ms],
        |row| row.get(0),
    )?;
    if expected_records == 0 || expected_records > MAX_RECORDS {
        return Err(ActionBiasError::Unqualified(Validation::Count));
    }
    let header = Header {
        format: FORMAT,
        store_id,
        schema,
        start_ms,
        end_ms,
        cutoff_ms,
        expected_records,
    };
    let mut out = temporary_output(output)?;
    let mut digest = Sha256::new();
    write_line(out.as_file_mut(), &ExportLine::Header(header), &mut digest)?;
    let query = format!(
        "WITH cohort AS (SELECT t.id,t.run_id,t.state,t.arguments_json {COHORT}) SELECT CASE WHEN length(CAST(id AS BLOB))=32 THEN id END,CASE WHEN length(CAST(run_id AS BLOB))=32 THEN run_id END,CASE WHEN state IN ('requested','awaiting_approval','running','completed','failed','denied','interrupted') THEN state END,length(CAST(arguments_json AS BLOB)),CASE WHEN length(CAST(arguments_json AS BLOB))<=?4 THEN CASE WHEN json_valid(arguments_json) THEN json_type(arguments_json,'$.command') END END,CASE WHEN length(CAST(arguments_json AS BLOB))<=?4 THEN arguments_json END FROM cohort ORDER BY id COLLATE BINARY"
    );
    let mut statement = transaction.prepare(&query)?;
    let mut rows = statement.query(params![start_ms, end_ms, cutoff_ms, MAX_ARGUMENT_BYTES])?;
    let mut records = 0;
    let mut last = None;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let id: ToolCallId = id
            .parse()
            .map_err(|_| ActionBiasError::Unqualified(Validation::Call))?;
        let run_id: String = row.get(1)?;
        let run_id: RunId = run_id
            .parse()
            .map_err(|_| ActionBiasError::Unqualified(Validation::Call))?;
        let state: String = row.get(2)?;
        let state: ToolCallState = serde_json::from_value(serde_json::Value::String(state))
            .map_err(|_| ActionBiasError::Unqualified(Validation::Call))?;
        if last.is_some_and(|previous| id <= previous) {
            return Err(ActionBiasError::Unqualified(Validation::Order));
        }
        last = Some(id);
        let envelope_bytes: u64 = row.get(3)?;
        if envelope_bytes > MAX_ARGUMENT_BYTES {
            return Err(ActionBiasError::Unqualified(Validation::Envelope));
        }
        let command_type: Option<String> = row.get(4)?;
        let command = if command_type.as_deref() != Some("text") {
            CommandInput::Malformed
        } else {
            let arguments: String = row.get(5)?;
            match qq_core::classify_bench::parsed_shell_command(&arguments) {
                Some(text) if text.len() > MAX_READ_COMMAND_BYTES => {
                    CommandInput::ClassifierRejected {
                        command_bytes: text.len() as u64,
                    }
                }
                Some(text) => CommandInput::Command { text },
                None => CommandInput::Malformed,
            }
        };
        write_line(
            out.as_file_mut(),
            &ExportLine::Call {
                id,
                run_id,
                state,
                command,
            },
            &mut digest,
        )?;
        records += 1;
    }
    if records != expected_records {
        return Err(ActionBiasError::Unqualified(Validation::Integrity));
    }
    let footer = ExportLine::Footer {
        records,
        snapshot_sha256: format!("{:x}", digest.finalize()),
    };
    let mut ignored = Sha256::new();
    write_line(out.as_file_mut(), &footer, &mut ignored)?;
    drop(rows);
    drop(statement);
    transaction.rollback()?;
    publish(out, output)
}

fn recount(input: &Path) -> Result<Recount, ActionBiasError> {
    let mut reader = BufReader::new(File::open(input)?);
    let mut bytes = Vec::new();
    let mut header = None;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut reads = 0;
    let mut rejected = 0;
    let mut malformed = 0;
    let mut last = None;
    loop {
        bytes.clear();
        let count = (&mut reader)
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            return Err(ActionBiasError::Unqualified(Validation::Framing));
        }
        if count > MAX_RECORD_BYTES {
            return Err(ActionBiasError::Unqualified(Validation::RecordBound));
        }
        if bytes.last() != Some(&b'\n') {
            return Err(ActionBiasError::Unqualified(Validation::Newline));
        }
        let line: ExportLine = serde_json::from_slice(&bytes)?;
        match line {
            ExportLine::Header(value) if header.is_none() && total == 0 => {
                if value.format != FORMAT || !matches!(value.schema, 45 | 46) {
                    return Err(ActionBiasError::Unqualified(Validation::Metadata));
                }
                validate_window(value.start_ms, value.end_ms, value.cutoff_ms)?;
                if value.expected_records == 0 || value.expected_records > MAX_RECORDS {
                    return Err(ActionBiasError::Unqualified(Validation::Count));
                }
                header = Some(value);
            }
            ExportLine::Call { id, command, .. } if header.is_some() => {
                if total >= MAX_RECORDS
                    || total >= header.as_ref().expect("header checked").expected_records
                {
                    return Err(ActionBiasError::Unqualified(Validation::Count));
                }
                if last.is_some_and(|previous| id <= previous) {
                    return Err(ActionBiasError::Unqualified(Validation::Order));
                }
                last = Some(id);
                total += 1;
                match command {
                    CommandInput::Command { text } => {
                        if text.len() > MAX_READ_COMMAND_BYTES {
                            return Err(ActionBiasError::Unqualified(Validation::CommandBound));
                        }
                        reads += u64::from(read_only_shell_command(&text));
                    }
                    CommandInput::ClassifierRejected { command_bytes } => {
                        if command_bytes <= MAX_READ_COMMAND_BYTES as u64 {
                            return Err(ActionBiasError::Unqualified(Validation::CommandBound));
                        }
                        rejected += 1;
                    }
                    CommandInput::Malformed => malformed += 1,
                }
            }
            ExportLine::Footer {
                records,
                snapshot_sha256,
            } if header.is_some() => {
                let expected = header.take().expect("header checked");
                let actual = format!("{:x}", digest.finalize());
                if total != records
                    || total != expected.expected_records
                    || snapshot_sha256 != actual
                {
                    return Err(ActionBiasError::Unqualified(Validation::Integrity));
                }
                let mut trailing = [0_u8; 1];
                if reader.read(&mut trailing)? != 0 {
                    return Err(ActionBiasError::Unqualified(Validation::Framing));
                }
                return Ok(Recount {
                    format: FORMAT,
                    header: expected,
                    snapshot_sha256: actual,
                    total_calls: total,
                    read_calls: reads,
                    classifier_rejected: rejected,
                    malformed_commands: malformed,
                    read_share: reads as f64 / total as f64,
                });
            }
            _ => return Err(ActionBiasError::Unqualified(Validation::Framing)),
        }
        digest.update(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(directory: &Path) -> (PathBuf, Connection) {
        let path = directory.join("store.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE sessions(id TEXT PRIMARY KEY,parent_id TEXT,depth INTEGER NOT NULL); CREATE TABLE runs(id TEXT PRIMARY KEY,session_id TEXT,kind TEXT,started_at_ms INTEGER,finished_at_ms INTEGER); CREATE TABLE tool_calls(id TEXT PRIMARY KEY,run_id TEXT,state TEXT,name TEXT,arguments_json TEXT);").unwrap();
        connection
            .execute("INSERT INTO metadata VALUES ('schema_version','45')", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO metadata VALUES ('store_id',?1)",
                [StoreId::from_bytes([7; 16]).to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions VALUES ('root',NULL,0),('child','root',1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runs VALUES (?1,'root','prompt',86400000,86400001)",
                [RunId::from_bytes([1; 16]).to_string()],
            )
            .unwrap();
        (path, connection)
    }

    fn call(connection: &Connection, index: u8, run: u8, state: &str, arguments: &str) {
        connection
            .execute(
                "INSERT INTO tool_calls VALUES (?1,?2,?3,'shell',?4)",
                params![
                    ToolCallId::from_bytes([index; 16]).to_string(),
                    RunId::from_bytes([run; 16]).to_string(),
                    state,
                    arguments
                ],
            )
            .unwrap();
    }

    #[test]
    fn ab6_qualification_windows_require_utc_days_and_exact_cutoff() {
        const DAY: i64 = 86_400_000;
        assert!(validate_window(DAY, 2 * DAY, 3 * DAY).is_ok());
        for (start, end, cutoff) in [
            (DAY, 2 * DAY, 4 * DAY),
            (DAY + 1, 2 * DAY, 3 * DAY),
            (DAY, 2 * DAY + 1, 3 * DAY + 1),
            (DAY, i64::MAX, i64::MAX),
        ] {
            assert!(
                matches!(
                    validate_window(start, end, cutoff),
                    Err(ActionBiasError::Unqualified(Validation::Window))
                ),
                "{start}/{end}/{cutoff}"
            );
        }
    }

    #[test]
    fn ab6_export_recount_retains_every_state_and_uses_the_progress_predicate() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        for (index, state) in [
            "requested",
            "awaiting_approval",
            "running",
            "completed",
            "failed",
            "denied",
            "interrupted",
        ]
        .iter()
        .enumerate()
        {
            call(
                &connection,
                index as u8 + 1,
                1,
                state,
                r#"{"command":"cd sub && grep x f"}"#,
            );
        }
        call(
            &connection,
            8,
            1,
            "completed",
            r#"{"command":"cargo test"}"#,
        );
        call(&connection, 9, 1, "failed", "not JSON");
        call(&connection, 10, 1, "failed", r#"{"command":42}"#);
        let oversized =
            serde_json::json!({"command":"x".repeat(MAX_READ_COMMAND_BYTES+1)}).to_string();
        call(&connection, 11, 1, "failed", &oversized);
        let before = fs::read(&store).unwrap();
        let export_path = directory.path().join("cohort.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &export_path).unwrap();
        assert_eq!(
            fs::read(&store).unwrap(),
            before,
            "export must not migrate or modify the database"
        );
        let result = recount(&export_path).unwrap();
        assert_eq!(
            (
                result.total_calls,
                result.read_calls,
                result.malformed_commands,
                result.classifier_rejected
            ),
            (11, 7, 2, 1)
        );
        assert_eq!(result.header.schema, 45);
        assert!(
            export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &export_path).is_err(),
            "existing exports must not be overwritten"
        );
        assert_eq!(
            recount(&export_path).unwrap().snapshot_sha256,
            result.snapshot_sha256
        );
    }

    #[test]
    fn ab6_malformed_shell_envelopes_cannot_become_reads() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        for (index, arguments) in [
            r#"{"command":"pwd","command":"cargo test"}"#,
            r#"{"command":"pwd","cwd":42}"#,
            r#"{"command":"pwd","cwd":".","cwd":"other"}"#,
            r#"{"command":"pwd","timeout_seconds":"bad"}"#,
            r#"{"command":"pwd","env":42}"#,
            r#"{"command":"pwd","unknown":true}"#,
            r#"{"command":"pwd","timeout_seconds":1,"timeout_seconds":2}"#,
            r#"{"cwd":"."}"#,
        ]
        .iter()
        .enumerate()
        {
            call(&connection, index as u8 + 1, 1, "failed", arguments);
        }
        let path = directory.path().join("cohort.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &path).unwrap();
        let result = recount(&path).unwrap();
        assert_eq!(
            (
                result.total_calls,
                result.read_calls,
                result.malformed_commands
            ),
            (8, 0, 8)
        );
    }

    #[test]
    fn ab6_cohort_excludes_children_nonprompts_end_boundary_and_late_completions() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(&connection, 1, 1, "completed", r#"{"command":"cat f"}"#);
        for (id, session, kind, start, finished) in [
            (2, "child", "prompt", DAY_MS, Some(DAY_MS + 1)),
            (3, "root", "side_question", DAY_MS, Some(DAY_MS + 1)),
            (4, "root", "prompt", 2 * DAY_MS, Some(2 * DAY_MS + 1)),
            (5, "root", "prompt", DAY_MS, Some(3 * DAY_MS)),
            (6, "root", "prompt", DAY_MS - 1, Some(DAY_MS + 1)),
            (7, "root", "prompt", DAY_MS, None),
        ] {
            connection
                .execute(
                    "INSERT INTO runs VALUES (?1,?2,?3,?4,?5)",
                    params![
                        RunId::from_bytes([id; 16]).to_string(),
                        session,
                        kind,
                        start,
                        finished
                    ],
                )
                .unwrap();
            call(&connection, id, id, "completed", r#"{"command":"cat f"}"#);
        }
        let path = directory.path().join("cohort.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &path).unwrap();
        assert_eq!(recount(&path).unwrap().total_calls, 1);
    }

    #[test]
    fn ab6_orphaned_child_retains_its_historical_role() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(
            &connection,
            1,
            1,
            "completed",
            r#"{"command":"cargo test"}"#,
        );
        connection
            .execute(
                "INSERT INTO runs VALUES (?1,'child','prompt',86400000,86400001)",
                [RunId::from_bytes([2; 16]).to_string()],
            )
            .unwrap();
        call(&connection, 2, 2, "completed", r#"{"command":"pwd"}"#);
        // Parent deletion clears the link; the persisted depth remains one.
        connection
            .execute("UPDATE sessions SET parent_id=NULL WHERE id='child'", [])
            .unwrap();
        let path = directory.path().join("cohort.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &path).unwrap();
        let result = recount(&path).unwrap();
        assert_eq!((result.total_calls, result.read_calls), (1, 0));
    }

    #[test]
    fn ab6_envelope_overflow_is_unqualified_and_exact_command_bound_is_retained() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        let command = format!("echo {}", "x".repeat(MAX_READ_COMMAND_BYTES - 5));
        call(
            &connection,
            1,
            1,
            "completed",
            &serde_json::json!({"command":command}).to_string(),
        );
        let path = directory.path().join("exact.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &path).unwrap();
        let counted = recount(&path).unwrap();
        assert_eq!((counted.read_calls, counted.classifier_rejected), (1, 0));
        call(
            &connection,
            2,
            1,
            "failed",
            &serde_json::json!({"command":"cat f","extra":"x".repeat(MAX_ARGUMENT_BYTES as usize)})
                .to_string(),
        );
        let overflow = directory.path().join("overflow.jsonl");
        assert!(matches!(
            export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &overflow),
            Err(ActionBiasError::Unqualified(Validation::Envelope))
        ));
        assert!(!overflow.exists());
    }

    #[test]
    fn ab6_recount_rejects_truncation_tampering_duplicate_ids_and_extra_records() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(&connection, 1, 1, "completed", r#"{"command":"cat f"}"#);
        call(
            &connection,
            2,
            1,
            "completed",
            r#"{"command":"cargo test"}"#,
        );
        let path = directory.path().join("good.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &path).unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let lines = original.lines().collect::<Vec<_>>();
        for (name, bad) in [
            ("missing-footer", format!("{}\n", lines[..3].join("\n"))),
            ("changed-command", original.replace("cat f", "cat g")),
            (
                "duplicate",
                format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[1], lines[3]),
            ),
            ("extra", format!("{original}{}\n", lines[1])),
            ("no-newline", original.trim_end().to_owned()),
        ] {
            let bad_path = directory.path().join(name);
            fs::write(&bad_path, bad).unwrap();
            assert!(recount(&bad_path).is_err(), "accepted {name}");
        }
        let huge = directory.path().join("huge");
        fs::write(&huge, vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(matches!(
            recount(&huge),
            Err(ActionBiasError::Unqualified(Validation::RecordBound))
        ));
    }

    #[test]
    fn ab6_worst_case_json_escaping_stays_within_the_record_bound() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(
            &connection,
            1,
            1,
            "completed",
            &serde_json::json!({"command":"\u{1}".repeat(MAX_READ_COMMAND_BYTES)}).to_string(),
        );
        let path = directory.path().join("escaped.jsonl");
        export(&store, DAY_MS, 2 * DAY_MS, 3 * DAY_MS, &path).unwrap();
        assert!(
            fs::read(&path)
                .unwrap()
                .split(|byte| *byte == b'\n')
                .all(|line| line.len() < MAX_RECORD_BYTES)
        );
        assert_eq!(recount(&path).unwrap().total_calls, 1);
    }
}
