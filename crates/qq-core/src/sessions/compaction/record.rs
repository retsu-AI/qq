//! The compaction record (ADR-0055): the exact part of a compaction summary,
//! rendered by QQ from stored rows inside the commit transaction and stored
//! after the model's narrative. It is rebuilt from rows at every compaction
//! and never folded, so it stays exact however many times a session
//! compacts.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;

use super::context::COMPACTION_RECORD_BYTES;
use super::*;

/// Shares of a full-size record. User messages take what the other parts
/// leave, so they get at least the remaining 40 KiB less the header and the
/// citation reserve.
const LAST_REPLY_SHARE: usize = 8 * 1024;
const FILES_SHARE: usize = 12 * 1024;
const FAILURES_SHARE: usize = 4 * 1024;
/// Held back inside the user-message budget for the omitted-message line.
const CITATION_RESERVE: usize = 640;
/// Below this budget no record is rendered: the parts' fixed framing would
/// leave no room for content.
const MIN_RECORD_BYTES: usize = 2 * 1024;
/// Tool calls scanned for files and failures, newest first. Matches the
/// session file-state bound.
const RECORD_SCAN_CALLS: u32 = 4_096;
/// Longest single path or failure line in the record.
const RECORD_LINE_BYTES: usize = 512;
/// A message that does not fit is cut rather than omitted while at least
/// this much budget remains.
const MIN_CUT_BYTES: usize = 2 * 1024;
/// Kept for an omission line at the end of a list, so a list never ends
/// silently.
const OMISSION_LINE_BYTES: usize = 32;

/// What a record covers.
#[derive(Debug, Clone, Copy)]
pub(in crate::sessions) enum RecordScope {
    /// A between-run compaction: every prompt at or before the cutoff, with
    /// its run's steering, calls, and replies, across all earlier compactions.
    Session { cutoff_ordinal: u64 },
    /// An in-run compaction: what the replaced turns of one prompt run held
    /// that the kept transcript does not — its steering, files, and failures.
    Run { run_id: RunId, turn_cutoff: u32 },
}

/// The record's byte budget: `COMPACTION_RECORD_BYTES`, lowered by a declared
/// window to an eighth of its estimated bytes (the context planner's
/// bytes-per-token ratio) so a small model keeps room to work.
pub(in crate::sessions) fn record_budget(context_window: Option<u32>) -> usize {
    context_window.map_or(COMPACTION_RECORD_BYTES, |window| {
        let window_bytes = u64::from(window).saturating_mul(context::ESTIMATED_BYTES_PER_TOKEN);
        usize::try_from(window_bytes / 8)
            .unwrap_or(usize::MAX)
            .min(COMPACTION_RECORD_BYTES)
    })
}

/// Renders the record, or an empty string when the scope has nothing to
/// record or the budget is too small to hold any of it. The result never
/// exceeds `budget` bytes and ends on a line boundary.
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
    // Part budgets scale with the record, so a small window keeps the same
    // proportions as a full one.
    let share = |full: usize| full.saturating_mul(budget) / COMPACTION_RECORD_BYTES;
    let session = session_id.to_string();

    let last_reply = match scope {
        RecordScope::Session { cutoff_ordinal } => render_last_reply(
            connection,
            &session,
            cutoff_ordinal,
            share(LAST_REPLY_SHARE),
        )?,
        // The kept turns of a running prompt are verbatim; its last reply is
        // in them.
        RecordScope::Run { .. } => String::new(),
    };
    let (files, failures) = render_files_and_failures(
        connection,
        &session,
        scope,
        share(FILES_SHARE),
        share(FAILURES_SHARE),
    )?;

    // The header line plus one blank line before each part.
    let framing = COMPACTION_RECORD_HEADER.len() + 1 + 4;
    let message_budget = budget
        .saturating_sub(framing)
        .saturating_sub(last_reply.len())
        .saturating_sub(files.len())
        .saturating_sub(failures.len());
    let messages = render_user_messages(connection, &session, scope, message_budget)?;

    if messages.is_empty() && last_reply.is_empty() && files.is_empty() && failures.is_empty() {
        return Ok(String::new());
    }
    let mut record = String::with_capacity(
        framing + messages.len() + last_reply.len() + files.len() + failures.len(),
    );
    record.push_str(COMPACTION_RECORD_HEADER);
    record.push('\n');
    for part in [messages, last_reply, files, failures] {
        if !part.is_empty() {
            record.push('\n');
            record.push_str(&part);
        }
    }
    // Each part is built within its own budget; the cut is only a backstop
    // that keeps the stored row bounded if that accounting is ever wrong.
    debug_assert!(record.len() <= budget, "{} > {budget}", record.len());
    Ok(truncate_utf8(record, budget))
}

/// Every user message the scope covers, verbatim, newest kept first and
/// printed oldest first. Prompts that do not fit are cited by ordinal for
/// `search_history`; omitted steering is counted.
fn render_user_messages(
    connection: &Connection,
    session: &str,
    scope: RecordScope,
    budget: usize,
) -> Result<String, SessionRuntimeError> {
    let heading = match scope {
        RecordScope::Session { .. } => "User messages, verbatim, oldest first:\n",
        RecordScope::Run { .. } => "Steering applied during the summarized turns, verbatim:\n",
    };
    if budget < heading.len() + CITATION_RESERVE {
        return Ok(String::new());
    }
    // Ids first; text is read only for the messages that fit, and only up to
    // the budget, so a long session costs one scan plus the kept bytes.
    struct Row {
        id: String,
        ordinal: u64,
        prompt_ordinal: u64,
        steering: bool,
        turn: u32,
    }
    let map = |row: &rusqlite::Row<'_>| {
        Ok(Row {
            id: row.get(0)?,
            ordinal: row.get(1)?,
            prompt_ordinal: row.get(2)?,
            steering: row.get(3)?,
            turn: row.get(4)?,
        })
    };
    let rows: Vec<Row> = match scope {
        RecordScope::Session { cutoff_ordinal } => {
            let mut statement = connection.prepare_cached(
                "SELECT m.id, m.ordinal, p.ordinal, m.steering, m.turn_ordinal
                     FROM messages p
                     JOIN messages m ON m.run_id = p.run_id AND m.role = 'user'
                     WHERE p.session_id = ?1 AND p.role = 'user' AND p.steering = 0
                       AND p.ordinal <= ?2
                       AND p.state IN ('complete', 'cancelled', 'failed', 'interrupted')
                       AND ((m.steering = 0
                             AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted'))
                            OR (m.steering = 1 AND m.state = 'complete'))
                     ORDER BY m.ordinal DESC",
            )?;
            statement
                .query_map(params![session, cutoff_ordinal], map)?
                .collect::<Result<_, _>>()?
        }
        RecordScope::Run {
            run_id,
            turn_cutoff,
        } => {
            // Exactly the steering replay drops with the replaced turns:
            // everything applied up to the first kept turn
            // (`append_run_turns`), or up to the cutoff when none is kept.
            let mut statement = connection.prepare_cached(
                "SELECT id, ordinal, ordinal, steering, turn_ordinal FROM messages
                     WHERE session_id = ?1 AND run_id = ?2 AND role = 'user'
                       AND steering = 1 AND state = 'complete'
                       AND turn_ordinal <= COALESCE(
                           (SELECT MIN(turn_ordinal) FROM model_turns
                            WHERE run_id = ?2 AND turn_ordinal > ?3),
                           ?3)
                     ORDER BY ordinal DESC",
            )?;
            statement
                .query_map(params![session, run_id.to_string(), turn_cutoff], map)?
                .collect::<Result<_, _>>()?
        }
    };
    if rows.is_empty() {
        return Ok(String::new());
    }

    let entries_budget = budget - heading.len() - CITATION_RESERVE;
    let mut kept: Vec<String> = Vec::new();
    let mut spent = 0_usize;
    let mut omitted_prompts: Vec<u64> = Vec::new();
    let mut omitted_steering = 0_usize;
    let mut full = false;
    for row in &rows {
        if full {
            if row.steering {
                omitted_steering += 1;
            } else {
                omitted_prompts.push(row.ordinal);
            }
            continue;
        }
        let label = match (scope, row.steering) {
            (RecordScope::Session { .. }, false) => format!("user message #{}", row.ordinal),
            (RecordScope::Session { .. }, true) => format!(
                "steering during user message #{} turn {}",
                row.prompt_ordinal, row.turn
            ),
            (RecordScope::Run { .. }, _) => format!("steering before turn {}", row.turn),
        };
        let remaining = entries_budget.saturating_sub(spent);
        let (text, total_bytes) = message_prefix(connection, &row.id, remaining)?;
        // `--- {label} ---\n{text}\n`
        let entry_bytes = label.len() + total_bytes + 10;
        if entry_bytes <= remaining {
            spent += entry_bytes;
            kept.push(format!("--- {label} ---\n{text}\n"));
            continue;
        }
        full = true;
        if kept.is_empty() || remaining >= MIN_CUT_BYTES {
            let note = if row.steering {
                "\n[cut for space]\n"
            } else {
                "\n[cut for space; search_history matches against the full message]\n"
            };
            // `--- {label} ---\n{cut}{note}`
            let room = remaining.saturating_sub(label.len() + 9 + note.len());
            if room > 0 {
                let cut = truncate_utf8(text, room);
                spent += label.len() + 9 + cut.len() + note.len();
                kept.push(format!("--- {label} ---\n{cut}{note}"));
                continue;
            }
        }
        if row.steering {
            omitted_steering += 1;
        } else {
            omitted_prompts.push(row.ordinal);
        }
    }

    let mut rendered = String::with_capacity(budget);
    rendered.push_str(heading);
    if !omitted_prompts.is_empty() || omitted_steering > 0 {
        rendered.push_str(&omitted_citation(&mut omitted_prompts, omitted_steering));
    }
    for entry in kept.iter().rev() {
        rendered.push_str(entry);
    }
    Ok(rendered)
}

/// One line naming the omitted messages: prompt ordinals as ranges, for
/// `search_history`, and a count of steering messages. At most
/// `CITATION_RESERVE` bytes.
fn omitted_citation(prompts: &mut [u64], steering: usize) -> String {
    prompts.sort_unstable();
    let mut line = String::from("[older messages omitted for space;");
    if !prompts.is_empty() {
        line.push_str(" search_history searches user messages ");
        let mut index = 0;
        let mut first = true;
        while index < prompts.len() {
            let start = prompts[index];
            let mut end = start;
            while index + 1 < prompts.len() && prompts[index + 1] == end + 1 {
                index += 1;
                end = prompts[index];
            }
            if !first {
                line.push_str(", ");
            }
            first = false;
            // Writing to a String cannot fail.
            let _ = if start == end {
                write!(line, "#{start}")
            } else {
                write!(line, "#{start}-#{end}")
            };
            if line.len() > CITATION_RESERVE / 2 {
                line.push_str(", ...");
                break;
            }
            index += 1;
        }
    }
    if steering > 0 {
        if !prompts.is_empty() {
            line.push(';');
        }
        let _ = write!(line, " {steering} steering message(s)");
    }
    line.push_str("]\n");
    line
}

/// The newest non-empty final assistant reply among the covered prompt runs,
/// read only up to the part's budget.
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
    // `substr` counts characters; `budget + 1` characters are always more
    // than `budget` bytes, so a fetched prefix is enough to cut from.
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

// Paths come out of the arguments in SQL, so a large `write_file` content or
// a malformed row is never copied out of SQLite. Only built-in file tools are
// read: MCP tools are always `mcp__`-prefixed, so these names are the
// built-ins. Shell-made changes are not listed.
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

/// Files modified and read through built-in tools, and failed tool calls,
/// from the newest `RECORD_SCAN_CALLS` calls of the scope.
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
            // The replaced turns' calls, as replay drops them.
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
        // Only `failed` calls are failures of the work: denied calls are a
        // user decision, interrupted ones a lifecycle event, and runtime
        // rejections ("not executed: ...") harness notices.
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
        // SQLite built this array from text values, so it always decodes;
        // a failure is a store invariant violation.
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
            // `- {path}\n`
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

/// A message's output text, read up to `max_bytes` (enough to cut from), and
/// its full length in bytes.
fn message_prefix(
    connection: &Connection,
    id: &str,
    max_bytes: usize,
) -> Result<(String, usize), SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "WITH message AS (
             SELECT m.output || COALESCE((SELECT group_concat(c.text, '') FROM (
                 SELECT text FROM message_chunks
                 WHERE message_id = m.id AND channel = 'output'
                 ORDER BY chunk_ordinal
             ) c), '') AS text
             FROM messages m WHERE m.id = ?1
         )
         SELECT substr(text, 1, ?2), length(CAST(text AS BLOB)) FROM message",
    )?;
    // `substr` counts characters; `max_bytes + 1` characters are always more
    // than `max_bytes` bytes.
    Ok(
        statement.query_row(params![id, max_bytes.saturating_add(1)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?,
    )
}
