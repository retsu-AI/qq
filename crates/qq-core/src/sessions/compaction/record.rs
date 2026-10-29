use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;

use super::context::COMPACTION_RECORD_BYTES;
use super::*;

const LAST_REPLY_SHARE: usize = 8 * 1024;
const FILES_SHARE: usize = 12 * 1024;
const FAILURES_SHARE: usize = 4 * 1024;
const MIN_RECORD_BYTES: usize = 2 * 1024;
const RECORD_SCAN_CALLS: u32 = 4_096;
const RECORD_LINE_BYTES: usize = 512;
const OMISSION_LINE_BYTES: usize = 32;

#[derive(Debug, Clone, Copy)]
pub(in crate::sessions) enum RecordScope {
    Session { cutoff_ordinal: u64 },
    Run { run_id: RunId, turn_cutoff: u32 },
}

pub(in crate::sessions) fn record_budget(context_window: Option<u32>) -> usize {
    context_window.map_or(COMPACTION_RECORD_BYTES, |window| {
        let window_bytes = u64::from(window).saturating_mul(context::ESTIMATED_BYTES_PER_TOKEN);
        usize::try_from(window_bytes / 8)
            .unwrap_or(usize::MAX)
            .min(COMPACTION_RECORD_BYTES)
    })
}

pub(in crate::sessions) fn render_compaction_record(
    connection: &Connection,
    session_id: SessionId,
    scope: RecordScope,
    budget: usize,
) -> Result<String, SessionRuntimeError> {
    let budget = budget.min(COMPACTION_RECORD_BYTES);
    if budget < MIN_RECORD_BYTES {
        return Ok(String::new());
    }
    let share = |full: usize| full.saturating_mul(budget) / COMPACTION_RECORD_BYTES;
    let session = session_id.to_string();

    let last_reply = match scope {
        RecordScope::Session { cutoff_ordinal } => render_last_reply(
            connection,
            &session,
            cutoff_ordinal,
            share(LAST_REPLY_SHARE),
        )?,
        RecordScope::Run { .. } => String::new(),
    };
    let (files, failures) = render_files_and_failures(
        connection,
        &session,
        scope,
        share(FILES_SHARE),
        share(FAILURES_SHARE),
    )?;

    let framing = COMPACTION_RECORD_HEADER.len() + 5;
    if last_reply.is_empty() && files.is_empty() && failures.is_empty() {
        return Ok(String::new());
    }
    let mut record =
        String::with_capacity(framing + last_reply.len() + files.len() + failures.len());
    record.push_str(COMPACTION_RECORD_HEADER);
    record.push('\n');
    for part in [last_reply, files, failures] {
        if !part.is_empty() {
            record.push('\n');
            record.push_str(&part);
        }
    }
    debug_assert!(record.len() <= budget, "{} > {budget}", record.len());
    Ok(truncate_utf8(record, budget))
}

fn render_last_reply(
    connection: &Connection,
    session: &str,
    cutoff_ordinal: u64,
    budget: usize,
) -> Result<String, SessionRuntimeError> {
    const NOTE: &str = "\n[cut for space; search_history searches the full reply]";
    let mut statement = connection.prepare_cached(
        "WITH candidates AS (
             SELECT p.ordinal AS prompt_ordinal, a.ordinal AS ordinal,
                    a.output || COALESCE((SELECT group_concat(c.text, '') FROM (
                        SELECT text FROM message_chunks
                        WHERE message_id = a.id AND channel = 'output'
                        ORDER BY chunk_ordinal
                    ) c), '') AS text
             FROM messages p
             JOIN messages a ON a.run_id = p.run_id
             WHERE p.session_id = ?1 AND p.role = 'user' AND p.steering = 0
               AND p.ordinal <= ?2
               AND a.role = 'assistant' AND a.state = 'complete'
             ORDER BY a.ordinal DESC
             LIMIT 8
         )
         SELECT prompt_ordinal, substr(text, 1, ?3), length(CAST(text AS BLOB))
         FROM candidates WHERE trim(text) != ''
         ORDER BY ordinal DESC LIMIT 1",
    )?;
    let found = statement
        .query_row(
            params![session, cutoff_ordinal, budget.saturating_add(1)],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, usize>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((prompt_ordinal, text, total_bytes)) = found else {
        return Ok(String::new());
    };
    let header = format!("Last assistant reply, to user message #{prompt_ordinal}:\n");
    let room = budget.saturating_sub(header.len() + 1);
    let body = if total_bytes <= room {
        text
    } else if room > NOTE.len() {
        let mut cut = truncate_utf8(text, room - NOTE.len());
        cut.push_str(NOTE);
        cut
    } else {
        return Ok(String::new());
    };
    Ok(format!("{header}{body}\n"))
}

macro_rules! record_call_columns {
    () => {
        "c.name,
         CASE WHEN c.name IN ('read_file', 'write_file') AND json_valid(c.arguments_json)
              THEN CASE WHEN json_type(c.arguments_json, '$.path') = 'text'
                        THEN json_array(json_extract(c.arguments_json, '$.path')) END
              WHEN c.name = 'edit_file' AND json_valid(c.arguments_json)
              THEN CASE WHEN COALESCE(json_extract(c.arguments_json, '$.dry_run'), 0) != 1
                        THEN (SELECT json_group_array(json_extract(e.value, '$.path'))
                              FROM json_each(c.arguments_json, '$.edits') e
                              WHERE json_type(e.value) = 'object'
                                AND json_type(e.value, '$.path') = 'text') END
         END,
         c.state = 'completed' AND c.is_error = 0,
         CASE WHEN c.state = 'failed' THEN substr(c.result, 1, 1024) END,
         c.turn_ordinal"
    };
}

fn render_files_and_failures(
    connection: &Connection,
    session: &str,
    scope: RecordScope,
    files_budget: usize,
    failures_budget: usize,
) -> Result<(String, String), SessionRuntimeError> {
    struct Call {
        name: String,
        paths: Option<String>,
        completed: bool,
        failure: Option<String>,
        turn: u32,
        prompt_ordinal: u64,
    }
    let map = |row: &rusqlite::Row<'_>| {
        Ok(Call {
            name: row.get(0)?,
            paths: row.get(1)?,
            completed: row.get(2)?,
            failure: row.get(3)?,
            turn: row.get(4)?,
            prompt_ordinal: row.get(5)?,
        })
    };
    let calls: Vec<Call> = match scope {
        RecordScope::Session { cutoff_ordinal } => {
            let mut statement = connection.prepare_cached(concat!(
                "SELECT ",
                record_call_columns!(),
                ", p.ordinal
                     FROM messages p
                     JOIN tool_calls c ON c.run_id = p.run_id
                     WHERE p.session_id = ?1 AND p.role = 'user' AND p.steering = 0
                       AND p.ordinal <= ?2
                     ORDER BY p.ordinal DESC, c.turn_ordinal DESC, c.call_ordinal DESC
                     LIMIT ?3"
            ))?;
            statement
                .query_map(params![session, cutoff_ordinal, RECORD_SCAN_CALLS], map)?
                .collect::<Result<_, _>>()?
        }
        RecordScope::Run {
            run_id,
            turn_cutoff,
        } => {
            let mut statement = connection.prepare_cached(concat!(
                "SELECT ",
                record_call_columns!(),
                ", 0
                     FROM tool_calls c
                     WHERE c.run_id = ?1 AND c.turn_ordinal <= ?2
                     ORDER BY c.turn_ordinal DESC, c.call_ordinal DESC
                     LIMIT ?3"
            ))?;
            statement
                .query_map(
                    params![run_id.to_string(), turn_cutoff, RECORD_SCAN_CALLS],
                    map,
                )?
                .collect::<Result<_, _>>()?
        }
    };

    let mut modified = BTreeSet::new();
    let mut read = BTreeSet::new();
    let mut failures: Vec<String> = Vec::new();
    let mut seen_failures = HashSet::new();
    for call in calls {
        if let Some(result) = call.failure
            && let Some(line) = result.lines().map(str::trim).find(|line| !line.is_empty())
            && !line.starts_with("not executed:")
            && seen_failures.insert((call.name.clone(), line.to_owned()))
        {
            let at = match scope {
                RecordScope::Session { .. } => {
                    format!("user message #{} turn {}", call.prompt_ordinal, call.turn)
                }
                RecordScope::Run { .. } => format!("turn {}", call.turn),
            };
            failures.push(format!(
                "- {}: {} ({at})\n",
                call.name,
                truncate_utf8(line.to_owned(), RECORD_LINE_BYTES)
            ));
        }
        if !call.completed {
            continue;
        }
        let Some(paths) = call.paths else {
            continue;
        };
        let paths: Vec<String> = serde_json::from_str(&paths)?;
        let paths = paths
            .into_iter()
            .map(|path| truncate_utf8(path, RECORD_LINE_BYTES));
        match call.name.as_str() {
            "read_file" => read.extend(paths),
            _ => modified.extend(paths),
        }
    }

    let mut files = String::new();
    let read_only: Vec<String> = read.difference(&modified).cloned().collect();
    for (heading, paths) in [
        (
            "Files modified:\n",
            modified.into_iter().collect::<Vec<_>>(),
        ),
        ("Files read, not modified:\n", read_only),
    ] {
        if paths.is_empty() {
            continue;
        }
        if files.len() + heading.len() + OMISSION_LINE_BYTES > files_budget {
            break;
        }
        files.push_str(heading);
        let total = paths.len();
        for (index, path) in paths.into_iter().enumerate() {
            if files.len() + path.len() + 3 + OMISSION_LINE_BYTES > files_budget {
                let _ = writeln!(files, "- ... {} more", total - index);
                break;
            }
            let _ = writeln!(files, "- {path}");
        }
    }

    let mut failed = String::new();
    let heading = "Failed tool calls, newest first:\n";
    if !failures.is_empty() && heading.len() + OMISSION_LINE_BYTES <= failures_budget {
        failed.push_str(heading);
        let total = failures.len();
        for (index, line) in failures.into_iter().enumerate() {
            if failed.len() + line.len() + OMISSION_LINE_BYTES > failures_budget {
                let _ = writeln!(failed, "- ... {} older", total - index);
                break;
            }
            failed.push_str(&line);
        }
    }
    Ok((files, failed))
}
