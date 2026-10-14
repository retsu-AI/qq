use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
use qq_core::classify_bench::{
    MAX_READ_COMMAND_BYTES, ProcessEvidenceStatus, ProcessProducerPin, read_only_shell_command,
    recorded_work_receipt, validate_process_evidence,
};
use qq_protocol::{ContentHash, RunId, StoreId, ToolCallId, ToolCallState};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{Date, OffsetDateTime};

const FORMAT: u16 = 3;
const DAY_MS: i64 = 86_400_000;
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXPORT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARGUMENT_BYTES: u64 = 256 * 1024;
const MAX_RECORDS: u64 = 1_000_000;
const COHORT: &str = "FROM tool_calls t JOIN runs r ON r.id=t.run_id JOIN sessions s ON s.id=r.session_id WHERE s.depth = 0 AND r.kind='prompt' AND r.started_at_ms>=?1 AND r.started_at_ms<?2 AND r.finished_at_ms<?3 AND t.name IN ('shell','exec')";

#[derive(Debug, Args)]
pub(crate) struct ActionBiasArgs {
    #[command(subcommand)]
    command: ActionBiasCommand,
}

#[derive(Debug, Subcommand)]
enum ActionBiasCommand {
    /// Export process calls and bounded evidence from a fixed root prompt snapshot.
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
        #[arg(long, requires = "producer_sha256")]
        producer_source: Option<String>,
        #[arg(long, requires = "producer_source")]
        producer_sha256: Option<ContentHash>,
    },
    /// Recount a complete export with this immutable classifier executable.
    Recount {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, requires = "producer_sha256")]
        producer_source: Option<String>,
        #[arg(long, requires = "producer_source")]
        producer_sha256: Option<ContentHash>,
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
    #[error("a stored call ID or state is invalid")]
    Call,
    #[error("a record exceeds the serialization bound")]
    RecordBound,
    #[error("the complete export exceeds the 512 MiB byte budget")]
    AggregateBound,
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
    #[error("exporter identity differs from this pinned recount executable")]
    ExportIdentity,
    #[error("the deployed producer pin is invalid or differs from the export")]
    ProducerPin,
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
    exporter: BuildIdentity,
    producer: Option<ProcessProducerPin>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum CommandInput {
    Command {
        text: String,
        has_requested_env: bool,
    },
    ClassifierRejected {
        command_bytes: u64,
    },
    Malformed,
    NotShell,
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
        tool: String,
        arguments_json: Option<String>,
        execution_context_json: Option<String>,
        plan_descriptor_json: Option<String>,
        plan_identity_json: Option<String>,
        oversized_inputs: u8,
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
    environment_disqualified: u64,
    lexical_read_share: Option<f64>,
    execution_effects_qualified: bool,
    shell_calls: u64,
    exec_calls: u64,
    verified_read_calls: u64,
    qualified_recorded_work_calls: u64,
    unqualified_calls: u64,
    oversized_input_calls: u64,
    unsupported_v1_calls: u64,
    execution_context_qualified: bool,
    verified_read_share: Option<f64>,
    recorded_work_calls: u64,
    execution_evidence_counts: BTreeMap<ProcessEvidenceStatus, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildIdentity {
    source_commit: String,
    classifier_source_sha256: String,
    executable_sha256: String,
    classifier_platform: String,
    classifier_arch: String,
}

#[derive(Serialize)]
struct Receipt {
    #[serde(flatten)]
    identity: BuildIdentity,
    recount: Recount,
}

fn compiled_identity() -> Result<BuildIdentity, ActionBiasError> {
    let source = option_env!("QQ_ACTION_BIAS_SOURCE_SHA")
        .ok_or(ActionBiasError::Unqualified(Validation::BuildIdentity))?;
    if !matches!(source.len(), 40 | 64)
        || !source
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ActionBiasError::Unqualified(Validation::BuildIdentity));
    }
    let mut classifier = Sha256::new();
    for source in [
        include_str!("../../crates/qq-core/src/approval.rs"),
        include_str!("../../crates/qq-core/src/approval/rules.rs"),
        include_str!("../../crates/qq-core/src/approval/classify.rs"),
        include_str!("../../crates/qq-core/src/tools/shell.rs"),
        include_str!("../../crates/qq-core/src/tools.rs"),
        include_str!("../../crates/qq-core/src/tools/process_context.rs"),
        include_str!("../../crates/qq-core/src/tools/process_context/evidence.rs"),
        include_str!("../../crates/qq-core/src/tools/process_context/provenance.rs"),
        include_str!("../../crates/qq-core/src/runtime/shell_policy.rs"),
        include_str!("../../crates/qq-core/src/plan/descriptor.rs"),
    ] {
        classifier.update((source.len() as u64).to_le_bytes());
        classifier.update(source.as_bytes());
    }
    classifier.update(
        qq_core::classify_bench::process_policy_digest()
            .to_string()
            .as_bytes(),
    );
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
    Ok(BuildIdentity {
        source_commit: source.to_owned(),
        classifier_source_sha256: format!("{:x}", classifier.finalize()),
        executable_sha256: format!("{:x}", digest.finalize()),
        classifier_platform: std::env::consts::OS.to_owned(),
        classifier_arch: std::env::consts::ARCH.to_owned(),
    })
}

pub(crate) fn run(args: ActionBiasArgs) -> Result<(), ActionBiasError> {
    let identity = compiled_identity()?;
    match args.command {
        ActionBiasCommand::Export {
            store,
            start,
            end,
            cutoff,
            output,
            producer_source,
            producer_sha256,
        } => {
            let start_ms = midnight(&start)?;
            let end_ms = midnight(&end)?;
            let cutoff_ms = midnight(&cutoff)?;
            validate_window(start_ms, end_ms, cutoff_ms)?;
            export(
                &identity,
                &store,
                start_ms,
                end_ms,
                cutoff_ms,
                &output,
                producer_pin(producer_source, producer_sha256)?.as_ref(),
            )
        }
        ActionBiasCommand::Recount {
            input,
            output,
            producer_source,
            producer_sha256,
        } => {
            let recount = recount(
                &identity,
                &input,
                producer_pin(producer_source, producer_sha256)?.as_ref(),
            )?;
            let receipt = Receipt { identity, recount };
            let mut out = temporary_output(&output)?;
            serde_json::to_writer_pretty(out.as_file_mut(), &receipt)?;
            out.as_file_mut().write_all(b"\n")?;
            publish(out, &output)
        }
    }
}

fn producer_pin(
    source: Option<String>,
    executable: Option<ContentHash>,
) -> Result<Option<ProcessProducerPin>, ActionBiasError> {
    match (source, executable) {
        (None, None) => Ok(None),
        (Some(source_revision), Some(executable_sha256))
            if source_revision.len() == 40
                && source_revision
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
        {
            Ok(Some(ProcessProducerPin {
                source_revision,
                executable_sha256,
            }))
        }
        _ => Err(ActionBiasError::Unqualified(Validation::ProducerPin)),
    }
}

fn column_exists(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, ActionBiasError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn command_input(tool: &str, arguments: Option<&str>) -> CommandInput {
    if tool != "shell" {
        return CommandInput::NotShell;
    }
    let Some(arguments) = arguments else {
        return CommandInput::Malformed;
    };
    match qq_core::classify_bench::parsed_shell_command(arguments) {
        Some((text, _)) if text.len() > MAX_READ_COMMAND_BYTES => {
            CommandInput::ClassifierRejected {
                command_bytes: text.len() as u64,
            }
        }
        Some((text, has_requested_env)) => CommandInput::Command {
            text,
            has_requested_env,
        },
        None => CommandInput::Malformed,
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
    identity: &BuildIdentity,
    store: &Path,
    start_ms: i64,
    end_ms: i64,
    cutoff_ms: i64,
    output: &Path,
    producer: Option<&ProcessProducerPin>,
) -> Result<(), ActionBiasError> {
    validate_window(start_ms, end_ms, cutoff_ms)?;
    let mut connection = Connection::open_with_flags(store, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    let transaction = connection.transaction()?;
    let schema: String = transaction.query_row("SELECT substr(value,1,8) FROM metadata WHERE key='schema_version' AND length(CAST(value AS BLOB))<=8", [], |row| row.get(0))?;
    let schema: u16 = schema
        .parse()
        .map_err(|_| ActionBiasError::Unqualified(Validation::Metadata))?;
    if !matches!(schema, 45..=50) {
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
        exporter: identity.clone(),
        producer: producer.cloned(),
    };
    let mut out = temporary_output(output)?;
    let mut digest = Sha256::new();
    write_line(out.as_file_mut(), &ExportLine::Header(header), &mut digest)?;
    let receipt = if column_exists(&transaction, "tool_calls", "execution_context_json")? {
        "t.execution_context_json"
    } else {
        "NULL"
    };
    let descriptor = if column_exists(&transaction, "runs", "plan_descriptor_json")? {
        "r.plan_descriptor_json"
    } else {
        "NULL"
    };
    let plan_identity = if column_exists(&transaction, "runs", "plan_identity_json")? {
        "r.plan_identity_json"
    } else {
        "NULL"
    };
    let query = format!(
        "WITH cohort AS (SELECT t.id,t.run_id,t.state,t.name,t.arguments_json,{receipt} AS receipt,{descriptor} AS descriptor,{plan_identity} AS identity {COHORT}) SELECT CASE WHEN length(CAST(id AS BLOB))=32 THEN id END,CASE WHEN length(CAST(run_id AS BLOB))=32 THEN run_id END,CASE WHEN state IN ('requested','awaiting_approval','running','completed','failed','denied','interrupted') THEN state END,name,CASE WHEN length(CAST(arguments_json AS BLOB))<=?4 THEN arguments_json END,CASE WHEN length(CAST(receipt AS BLOB))<=?5 THEN receipt END,CASE WHEN length(CAST(descriptor AS BLOB))<=?4 THEN descriptor END,CASE WHEN length(CAST(identity AS BLOB))<=?5 THEN identity END,(COALESCE(length(CAST(arguments_json AS BLOB)),0)>?4)+2*(COALESCE(length(CAST(receipt AS BLOB)),0)>?5)+4*(COALESCE(length(CAST(descriptor AS BLOB)),0)>?4)+8*(COALESCE(length(CAST(identity AS BLOB)),0)>?5) FROM cohort ORDER BY id COLLATE BINARY"
    );
    let mut statement = transaction.prepare(&query)?;
    let mut rows = statement.query(params![
        start_ms,
        end_ms,
        cutoff_ms,
        MAX_ARGUMENT_BYTES,
        16 * 1024
    ])?;
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
        let tool: String = row.get(3)?;
        let arguments_json: Option<String> = row.get(4)?;
        let execution_context_json: Option<String> = row.get(5)?;
        let plan_descriptor_json: Option<String> = row.get(6)?;
        let plan_identity_json: Option<String> = row.get(7)?;
        let oversized_inputs: u8 = row.get(8)?;
        let command = command_input(&tool, arguments_json.as_deref());
        write_line(
            out.as_file_mut(),
            &ExportLine::Call {
                id,
                run_id,
                state,
                command,
                tool,
                arguments_json,
                execution_context_json,
                plan_descriptor_json,
                plan_identity_json,
                oversized_inputs,
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

fn recount(
    identity: &BuildIdentity,
    input: &Path,
    producer: Option<&ProcessProducerPin>,
) -> Result<Recount, ActionBiasError> {
    let mut reader = BufReader::new(File::open(input)?);
    let mut bytes = Vec::new();
    let mut header = None;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut reads = 0;
    let mut rejected = 0;
    let mut malformed = 0;
    let mut environment_disqualified = 0;
    let mut last = None;
    let (
        mut shell_calls,
        mut exec_calls,
        mut verified_reads,
        mut work,
        mut unqualified,
        mut oversized,
        mut unsupported_v1,
    ) = (0, 0, 0, 0, 0, 0, 0);
    let mut evidence_counts = BTreeMap::new();
    let mut recorded_work = 0;
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
                if value.format != FORMAT || !matches!(value.schema, 45..=50) {
                    return Err(ActionBiasError::Unqualified(Validation::Metadata));
                }
                if value.producer.as_ref() != producer {
                    return Err(ActionBiasError::Unqualified(Validation::ProducerPin));
                }
                if value.exporter != *identity {
                    return Err(ActionBiasError::Unqualified(Validation::ExportIdentity));
                }
                validate_window(value.start_ms, value.end_ms, value.cutoff_ms)?;
                if value.expected_records == 0 || value.expected_records > MAX_RECORDS {
                    return Err(ActionBiasError::Unqualified(Validation::Count));
                }
                header = Some(value);
            }
            ExportLine::Call {
                id,
                state,
                command,
                tool,
                arguments_json,
                execution_context_json,
                plan_descriptor_json,
                plan_identity_json,
                oversized_inputs,
                ..
            } if header.is_some() => {
                if !matches!(tool.as_str(), "shell" | "exec")
                    || oversized_inputs > 15
                    || command != command_input(&tool, arguments_json.as_deref())
                {
                    return Err(ActionBiasError::Unqualified(Validation::Call));
                }
                for (bit, value, limit) in [
                    (1, arguments_json.as_ref(), 256 * 1024),
                    (2, execution_context_json.as_ref(), 16 * 1024),
                    (4, plan_descriptor_json.as_ref(), 256 * 1024),
                    (8, plan_identity_json.as_ref(), 16 * 1024),
                ] {
                    if value.is_some_and(|value| value.len() > limit)
                        || (oversized_inputs & bit != 0 && value.is_some())
                    {
                        return Err(ActionBiasError::Unqualified(Validation::RecordBound));
                    }
                }
                shell_calls += u64::from(tool == "shell");
                exec_calls += u64::from(tool == "exec");
                oversized += u64::from(oversized_inputs != 0);
                let qualification = validate_process_evidence(
                    id,
                    state,
                    &tool,
                    arguments_json.as_deref(),
                    execution_context_json.as_deref(),
                    plan_descriptor_json.as_deref(),
                    plan_identity_json.as_deref(),
                    producer,
                );
                *evidence_counts.entry(qualification).or_insert(0) += 1;
                recorded_work += u64::from(recorded_work_receipt(
                    state,
                    execution_context_json.as_deref(),
                ));
                match qualification {
                    ProcessEvidenceStatus::VerifiedRead => verified_reads += 1,
                    ProcessEvidenceStatus::RecordedWork => work += 1,
                    ProcessEvidenceStatus::UnsupportedV1 => {
                        unsupported_v1 += 1;
                        unqualified += 1;
                    }
                    _ => unqualified += 1,
                }
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
                    CommandInput::Command {
                        text,
                        has_requested_env,
                    } => {
                        if text.len() > MAX_READ_COMMAND_BYTES {
                            return Err(ActionBiasError::Unqualified(Validation::CommandBound));
                        }
                        environment_disqualified += u64::from(has_requested_env);
                        reads += u64::from(!has_requested_env && read_only_shell_command(&text));
                    }
                    CommandInput::ClassifierRejected { command_bytes } => {
                        if command_bytes <= MAX_READ_COMMAND_BYTES as u64 {
                            return Err(ActionBiasError::Unqualified(Validation::CommandBound));
                        }
                        rejected += 1;
                    }
                    CommandInput::Malformed => malformed += 1,
                    CommandInput::NotShell => {}
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
                    environment_disqualified,
                    lexical_read_share: (shell_calls > 0)
                        .then(|| reads as f64 / shell_calls as f64),
                    shell_calls,
                    exec_calls,
                    verified_read_calls: verified_reads,
                    qualified_recorded_work_calls: work,
                    unqualified_calls: unqualified,
                    oversized_input_calls: oversized,
                    unsupported_v1_calls: unsupported_v1,
                    execution_context_qualified: unqualified == 0 && producer.is_some(),
                    verified_read_share: (unqualified == 0 && producer.is_some())
                        .then_some(verified_reads as f64 / total as f64),
                    execution_effects_qualified: false,
                    recorded_work_calls: recorded_work,
                    execution_evidence_counts: evidence_counts,
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

    fn test_identity() -> BuildIdentity {
        BuildIdentity {
            source_commit: "1".repeat(40),
            classifier_source_sha256: "2".repeat(64),
            executable_sha256: "3".repeat(64),
            classifier_platform: "linux".to_owned(),
            classifier_arch: "x86_64".to_owned(),
        }
    }

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
                "INSERT INTO tool_calls(id,run_id,state,name,arguments_json) VALUES (?1,?2,?3,'shell',?4)",
                params![
                    ToolCallId::from_bytes([index; 16]).to_string(),
                    RunId::from_bytes([run; 16]).to_string(),
                    state,
                    arguments
                ],
            )
            .unwrap();
    }

    struct NeverProvider;
    impl qq_provider::Provider for NeverProvider {
        fn stream(&self, _: qq_provider::ModelRequest) -> qq_provider::ProviderStream {
            panic!("qualification never calls a provider")
        }
    }

    #[test]
    fn ab6_format3_requires_independent_producer_pins_and_revalidates_both_process_tools() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        connection.execute_batch("UPDATE metadata SET value='50' WHERE key='schema_version'; ALTER TABLE tool_calls ADD COLUMN execution_context_json TEXT; ALTER TABLE runs ADD COLUMN plan_descriptor_json TEXT; ALTER TABLE runs ADD COLUMN plan_identity_json TEXT;").unwrap();
        let image = qq_core::ApprovedReadImage {
            family: qq_core::ReadImageFamily::Grep,
            sha256: ContentHash::from_bytes([7; 32]),
            contract: qq_core::ReadImageContract::SelfContainedReadV1,
        };
        let images = qq_core::ApprovedReadImages::new(vec![image.clone()]).unwrap();
        let runtime = qq_core::Runtime::new(NeverProvider, "test", 256)
            .unwrap()
            .with_approved_read_images(images);
        let plan = qq_core::plan::CompiledAgentPlan::compile_blocking(
            qq_core::plan::AgentProfile::embedded(
                &runtime,
                fs::canonicalize(directory.path()).unwrap(),
            ),
        )
        .unwrap();
        connection
            .execute(
                "UPDATE runs SET plan_descriptor_json=?1,plan_identity_json=?2",
                params![
                    serde_json::to_string(plan.descriptor()).unwrap(),
                    serde_json::to_string(&plan.identity()).unwrap()
                ],
            )
            .unwrap();
        let pin = ProcessProducerPin {
            source_revision: "a".repeat(40),
            executable_sha256: ContentHash::from_bytes([8; 32]),
        };
        let exec_arguments = r#"{"program":"/usr/bin/grep","args":["x","f"],"cwd":null,"stdin":null,"timeout_seconds":null,"env":[]}"#;
        let shell_arguments = r#"{"command":"pwd","cwd":null,"timeout_seconds":null,"env":[]}"#;
        let mut receipts = Vec::new();
        for (index, tool, arguments, verdict) in [
            (
                1,
                "exec",
                exec_arguments,
                serde_json::json!({"kind":"verified_read","family":"grep","sealed_image_sha256":image.sha256}),
            ),
            (
                2,
                "shell",
                shell_arguments,
                serde_json::json!({"kind":"work","reason":"shell"}),
            ),
        ] {
            let id = ToolCallId::from_bytes([index; 16]);
            let receipt = serde_json::json!({"version":"2","facts":{"context_id":id,"arguments_sha256":ContentHash::from_bytes(Sha256::digest(arguments.as_bytes()).into()),"manifest_sha256":plan.descriptor().approved_read_images.unwrap(),"base_env_present":1,"requested_env":false,"locale":"c","cwd":{"kind":"held_directory","device":1,"inode":2},"verdict":verdict,"pid":11},"manifest":[image.clone()],"policy_sha256":qq_core::classify_bench::process_policy_digest(),"producer":{"instance":ToolCallId::from_bytes([9+index;16]),"source":{"status":"clean","revision":pin.source_revision},"executable_sha256":pin.executable_sha256,"platform":format!("{}-{}",std::env::consts::ARCH,std::env::consts::OS)}});
            connection.execute("INSERT INTO tool_calls(id,run_id,state,name,arguments_json,execution_context_json) VALUES (?1,?2,'completed',?3,?4,?5)",params![id.to_string(),RunId::from_bytes([1;16]).to_string(),tool,arguments,receipt.to_string()]).unwrap();
            receipts.push(receipt);
        }
        let path = directory.path().join("v2.jsonl");
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            Some(&pin),
        )
        .unwrap();
        let result = recount(&test_identity(), &path, Some(&pin)).unwrap();
        assert_eq!(
            (
                result.total_calls,
                result.shell_calls,
                result.exec_calls,
                result.verified_read_calls,
                result.qualified_recorded_work_calls,
                result.recorded_work_calls
            ),
            (2, 1, 1, 1, 1, 1)
        );
        assert_ne!(
            receipts[0]["producer"]["instance"],
            receipts[1]["producer"]["instance"]
        );
        assert!(
            result.execution_context_qualified,
            "same pinned build may restart"
        );
        assert_eq!(result.verified_read_share, Some(0.5));
        for (field, changed) in [
            (
                "source",
                serde_json::json!({"status":"clean","revision":"b".repeat(40)}),
            ),
            (
                "executable_sha256",
                serde_json::json!(ContentHash::from_bytes([6; 32])),
            ),
        ] {
            let mut mismatched = receipts[1].clone();
            mismatched["producer"][field] = changed;
            connection
                .execute(
                    "UPDATE tool_calls SET execution_context_json=?1 WHERE name='shell'",
                    [mismatched.to_string()],
                )
                .unwrap();
            let mixed_path = directory.path().join(format!("mixed-{field}.jsonl"));
            export(
                &test_identity(),
                &store,
                DAY_MS,
                2 * DAY_MS,
                3 * DAY_MS,
                &mixed_path,
                Some(&pin),
            )
            .unwrap();
            let mixed = recount(&test_identity(), &mixed_path, Some(&pin)).unwrap();
            assert!(!mixed.execution_context_qualified);
            assert_eq!(mixed.unqualified_calls, 1);
            assert_eq!(
                mixed.execution_evidence_counts[&ProcessEvidenceStatus::UnqualifiedProducer],
                1
            );
        }
        connection
            .execute(
                "UPDATE tool_calls SET execution_context_json=?1 WHERE name='shell'",
                [receipts[1].to_string()],
            )
            .unwrap();
        assert!(
            !result.execution_effects_qualified,
            "Work is not proof of mutation"
        );
        assert!(matches!(
            recount(&test_identity(), &path, None),
            Err(ActionBiasError::Unqualified(Validation::ProducerPin))
        ));
        let mut wrong = pin.clone();
        wrong.executable_sha256 = ContentHash::from_bytes([6; 32]);
        assert!(recount(&test_identity(), &path, Some(&wrong)).is_err());
        let original = fs::read_to_string(&path).unwrap();
        for call_index in [1, 2] {
            for state in [
                "requested",
                "awaiting_approval",
                "running",
                "denied",
                "interrupted",
            ] {
                let mut lines = original
                    .lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .collect::<Vec<_>>();
                lines[call_index]["state"] = state.into();
                let mut bytes = Vec::new();
                for line in &lines[..lines.len() - 1] {
                    serde_json::to_writer(&mut bytes, line).unwrap();
                    bytes.push(b'\n');
                }
                lines.last_mut().unwrap()["snapshot_sha256"] =
                    format!("{:x}", Sha256::digest(&bytes)).into();
                serde_json::to_writer(&mut bytes, lines.last().unwrap()).unwrap();
                bytes.push(b'\n');
                let altered = directory
                    .path()
                    .join(format!("state-{call_index}-{state}.jsonl"));
                fs::write(&altered, bytes).unwrap();
                let result = recount(&test_identity(), &altered, Some(&pin)).unwrap();
                assert_eq!(result.total_calls, 2);
                assert!(
                    !result.execution_context_qualified,
                    "{state} cannot qualify settled process evidence"
                );
                assert_eq!(result.unqualified_calls, 1);
            }
        }
        let mut legacy = receipts[1]["facts"].clone();
        legacy["version"] = "1".into();
        connection
            .execute(
                "UPDATE tool_calls SET execution_context_json=?1 WHERE name='shell'",
                [legacy.to_string()],
            )
            .unwrap();
        let legacy_path = directory.path().join("v1.jsonl");
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &legacy_path,
            Some(&pin),
        )
        .unwrap();
        let result = recount(&test_identity(), &legacy_path, Some(&pin)).unwrap();
        assert_eq!(result.unsupported_v1_calls, 1);
        assert_eq!(
            result.recorded_work_calls, 1,
            "V1 retains its recorded accounting verdict"
        );
        assert!(!result.execution_context_qualified);
        assert!(result.verified_read_share.is_none());
    }

    #[test]
    fn ab6_format3_bounds_each_sql_input_without_dropping_the_call() {
        for (field, limit, bit) in [
            ("arguments_json", 256 * 1024, 1),
            ("execution_context_json", 16 * 1024, 2),
            ("plan_descriptor_json", 256 * 1024, 4),
            ("plan_identity_json", 16 * 1024, 8),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (store, connection) = fixture(directory.path());
            call(&connection, 1, 1, "failed", r#"{"command":"pwd"}"#);
            connection.execute_batch("UPDATE metadata SET value='50' WHERE key='schema_version'; ALTER TABLE tool_calls ADD COLUMN execution_context_json TEXT; ALTER TABLE runs ADD COLUMN plan_descriptor_json TEXT; ALTER TABLE runs ADD COLUMN plan_identity_json TEXT;").unwrap();
            let table = if field.starts_with("plan_") {
                "runs"
            } else {
                "tool_calls"
            };
            connection
                .execute(
                    &format!("UPDATE {table} SET {field}=?1"),
                    ["x".repeat(limit + 1)],
                )
                .unwrap();
            let path = directory.path().join("bounded.jsonl");
            export(
                &test_identity(),
                &store,
                DAY_MS,
                2 * DAY_MS,
                3 * DAY_MS,
                &path,
                None,
            )
            .unwrap();
            let result = recount(&test_identity(), &path, None).unwrap();
            assert_eq!(
                (
                    result.total_calls,
                    result.oversized_input_calls,
                    result.unqualified_calls
                ),
                (1, 1, 1)
            );
            assert!(result.verified_read_share.is_none());
            let line: serde_json::Value =
                serde_json::from_str(fs::read_to_string(&path).unwrap().lines().nth(1).unwrap())
                    .unwrap();
            assert_eq!(line["oversized_inputs"], bit);
            assert!(line[field].is_null());
        }
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &export_path,
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read(&store).unwrap(),
            before,
            "export must not migrate or modify the database"
        );
        let result = recount(&test_identity(), &export_path, None).unwrap();
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
        let receipt = serde_json::to_value(&result).unwrap();
        assert_eq!(receipt["execution_effects_qualified"], false);
        assert_eq!(
            receipt["lexical_read_share"],
            result.read_calls as f64 / result.total_calls as f64
        );
        assert!(receipt.get("read_share").is_none());
        assert!(
            export(
                &test_identity(),
                &store,
                DAY_MS,
                2 * DAY_MS,
                3 * DAY_MS,
                &export_path,
                None
            )
            .is_err(),
            "existing exports must not be overwritten"
        );
        assert_eq!(
            recount(&test_identity(), &export_path, None)
                .unwrap()
                .snapshot_sha256,
            result.snapshot_sha256
        );
    }

    #[test]
    fn ab6_zero_shell_denominator_has_no_lexical_share() {
        for exec_only in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let (store, connection) = fixture(directory.path());
            if exec_only {
                call(
                    &connection,
                    1,
                    1,
                    "completed",
                    r#"{"program":"true","args":[]}"#,
                );
                connection
                    .execute("UPDATE tool_calls SET name='exec'", [])
                    .unwrap();
            }
            let path = directory.path().join("zero-shell.jsonl");
            let exported = export(
                &test_identity(),
                &store,
                DAY_MS,
                2 * DAY_MS,
                3 * DAY_MS,
                &path,
                None,
            );
            if !exec_only {
                assert!(matches!(
                    exported,
                    Err(ActionBiasError::Unqualified(Validation::Count))
                ));
                continue;
            }
            exported.unwrap();
            let result = recount(&test_identity(), &path, None).unwrap();
            assert_eq!(result.shell_calls, 0);
            assert_eq!(result.exec_calls, u64::from(exec_only));
            assert!(serde_json::to_value(&result).unwrap()["lexical_read_share"].is_null());
        }
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
            r#"{"command":"pwd","timeout_seconds":0}"#,
            r#"{"command":"pwd","timeout_seconds":601}"#,
            r#"{"command":"   "}"#,
            r#"{"command":"pwd\u0000"}"#,
            r#"{"command":"pwd","cwd":""}"#,
            r#"{"command":"pwd","cwd":"/tmp"}"#,
            r#"{"command":"pwd","cwd":".."}"#,
            r#"{"command":"pwd","cwd":"sub/../../"}"#,
            r#"{"command":"pwd","env":{"bad=name":"value"}}"#,
        ]
        .iter()
        .enumerate()
        {
            call(&connection, index as u8 + 1, 1, "failed", arguments);
        }
        let path = directory.path().join("cohort.jsonl");
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        let result = recount(&test_identity(), &path, None).unwrap();
        assert_eq!(
            (
                result.total_calls,
                result.read_calls,
                result.malformed_commands
            ),
            (17, 0, 17)
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        assert_eq!(
            recount(&test_identity(), &path, None).unwrap().total_calls,
            1
        );
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        let result = recount(&test_identity(), &path, None).unwrap();
        assert_eq!((result.total_calls, result.read_calls), (1, 0));
    }

    #[test]
    fn ab6_export_header_binds_the_transforming_build() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(&connection, 1, 1, "completed", r#"{"command":"pwd"}"#);
        let path = directory.path().join("cohort.jsonl");
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(header["format"], 3);
        for field in [
            "source_commit",
            "classifier_source_sha256",
            "executable_sha256",
            "classifier_platform",
            "classifier_arch",
        ] {
            assert!(
                header["exporter"][field].is_string(),
                "missing exporter {field}"
            );
        }
    }

    #[test]
    fn ab6_public_export_requires_immutable_build_identity() {
        if option_env!("QQ_ACTION_BIAS_SOURCE_SHA").is_some() {
            return; // The missing-build case is exercised only in an unpinned build.
        }
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(&connection, 1, 1, "completed", r#"{"command":"pwd"}"#);
        let output = directory.path().join("cohort.jsonl");
        let result = run(ActionBiasArgs {
            command: ActionBiasCommand::Export {
                store,
                start: "1970-01-02".to_owned(),
                end: "1970-01-03".to_owned(),
                cutoff: "1970-01-04".to_owned(),
                output: output.clone(),
                producer_source: None,
                producer_sha256: None,
            },
        });
        assert!(matches!(
            result,
            Err(ActionBiasError::Unqualified(Validation::BuildIdentity))
        ));
        assert!(!output.exists());
    }

    #[test]
    fn ab6_recount_rejects_other_exporters_and_identityless_formats() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        call(
            &connection,
            1,
            1,
            "completed",
            r#"{"command":"pwd","timeout_seconds":1,"env":["LANG"],"cwd":"."}"#,
        );
        let path = directory.path().join("cohort.jsonl");
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        assert_eq!(
            recount(&test_identity(), &path, None).unwrap().read_calls,
            0
        );
        let original = fs::read_to_string(&path).unwrap();
        for field in [
            "source_commit",
            "classifier_source_sha256",
            "executable_sha256",
            "classifier_platform",
            "classifier_arch",
            "missing",
            "missing_env",
            "format",
        ] {
            let mut lines = original
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
            match field {
                "missing" => {
                    lines[0].as_object_mut().unwrap().remove("exporter");
                }
                "missing_env" => {
                    lines[1]["command"]
                        .as_object_mut()
                        .unwrap()
                        .remove("has_requested_env");
                }
                "format" => lines[0]["format"] = 1.into(),
                _ => lines[0]["exporter"][field] = "other-build".into(),
            }
            let mut bytes = Vec::new();
            for line in &lines[..lines.len() - 1] {
                serde_json::to_writer(&mut bytes, line).unwrap();
                bytes.push(b'\n');
            }
            let last = lines.last_mut().unwrap();
            last["snapshot_sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
            serde_json::to_writer(&mut bytes, last).unwrap();
            bytes.push(b'\n');
            let bad = directory.path().join(format!("{field}.jsonl"));
            fs::write(&bad, bytes).unwrap();
            let error = recount(&test_identity(), &bad, None).unwrap_err();
            match field {
                "missing" | "missing_env" => assert!(matches!(error, ActionBiasError::Json(_))),
                "format" => assert!(matches!(
                    error,
                    ActionBiasError::Unqualified(Validation::Metadata)
                )),
                _ => assert!(matches!(
                    error,
                    ActionBiasError::Unqualified(Validation::ExportIdentity)
                )),
            }
        }
    }

    #[test]
    fn ab6_requested_process_environments_remain_nonreads() {
        let directory = tempfile::tempdir().unwrap();
        let (store, connection) = fixture(directory.path());
        for (index, arguments) in [
            r#"{"command":"pwd"}"#,
            r#"{"command":"pwd","env":[]}"#,
            r#"{"command":"pwd","env":["LANG"]}"#,
            r#"{"command":"pwd","env":["LD_PRELOAD"]}"#,
        ]
        .iter()
        .enumerate()
        {
            call(&connection, index as u8 + 1, 1, "completed", arguments);
        }
        let path = directory.path().join("cohort.jsonl");
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        let result = recount(&test_identity(), &path, None).unwrap();
        assert_eq!(
            (
                result.total_calls,
                result.read_calls,
                result.malformed_commands
            ),
            (4, 2, 0)
        );
        assert_eq!(result.environment_disqualified, 2);
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        let counted = recount(&test_identity(), &path, None).unwrap();
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &overflow,
            None,
        )
        .unwrap();
        let counted = recount(&test_identity(), &overflow, None).unwrap();
        assert_eq!(counted.total_calls, 2);
        assert_eq!(counted.oversized_input_calls, 1);
        assert!(!counted.execution_context_qualified);
        assert!(counted.verified_read_share.is_none());
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
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
            assert!(
                recount(&test_identity(), &bad_path, None).is_err(),
                "accepted {name}"
            );
        }
        let huge = directory.path().join("huge");
        fs::write(&huge, vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(matches!(
            recount(&test_identity(), &huge, None),
            Err(ActionBiasError::Unqualified(Validation::RecordBound))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ab6_export_total_bytes_are_bounded_before_writing() {
        use std::io::{Seek, SeekFrom};
        let mut output = tempfile::NamedTempFile::new().unwrap();
        output
            .as_file_mut()
            .seek(SeekFrom::Start(MAX_EXPORT_BYTES))
            .unwrap();
        let footer = ExportLine::Footer {
            records: 1,
            snapshot_sha256: "0".repeat(64),
        };
        assert!(matches!(
            write_line(output.as_file_mut(), &footer, &mut Sha256::new()),
            Err(ActionBiasError::Unqualified(Validation::AggregateBound))
        ));
        assert_eq!(output.as_file().metadata().unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn ab6_recount_rejects_total_bytes_before_parsing() {
        let input = tempfile::NamedTempFile::new().unwrap();
        input.as_file().set_len(MAX_EXPORT_BYTES + 1).unwrap();
        assert!(matches!(
            recount(&test_identity(), input.path(), None),
            Err(ActionBiasError::Unqualified(Validation::AggregateBound))
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
        export(
            &test_identity(),
            &store,
            DAY_MS,
            2 * DAY_MS,
            3 * DAY_MS,
            &path,
            None,
        )
        .unwrap();
        assert!(
            fs::read(&path)
                .unwrap()
                .split(|byte| *byte == b'\n')
                .all(|line| line.len() < MAX_RECORD_BYTES)
        );
        assert_eq!(
            recount(&test_identity(), &path, None).unwrap().total_calls,
            1
        );
    }
}
