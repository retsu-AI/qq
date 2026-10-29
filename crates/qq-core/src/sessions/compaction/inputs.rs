use super::*;

pub(in crate::sessions) const EXACT_INPUTS_HEADER: &str =
    "[QQ original inputs: verbatim, in conversation order; later corrections take precedence]";

pub(in crate::sessions) fn with_exact_inputs(
    connection: &Connection,
    session: &str,
    scope: RecordScope,
    mut summary: String,
) -> Result<String, SessionRuntimeError> {
    let mut roots = connection.prepare_cached(
        "SELECT p.run_id, p.ordinal,
                COALESCE(json_extract(r.origin_json, '$.type'), '') IN ('goal', 'goal_check')
         FROM messages p JOIN runs r ON r.id = p.run_id
         WHERE p.session_id = ?1 AND p.role = 'user' AND p.steering = 0
           AND ((?2 IS NULL AND p.ordinal <= ?3
                 AND p.state IN ('complete', 'cancelled', 'failed', 'interrupted'))
                OR p.run_id = ?2)
         ORDER BY p.ordinal",
    )?;
    let (run, cutoff, turn_cutoff) = match scope {
        RecordScope::Session { cutoff_ordinal } => (None, cutoff_ordinal, None),
        RecordScope::Run {
            run_id,
            turn_cutoff,
        } => (Some(run_id.to_string()), 0, Some(turn_cutoff)),
    };
    let mut roots = roots.query(params![session, run, cutoff])?;
    let mut sources = connection.prepare_cached(
        "SELECT id, turn_ordinal, steering, 0 AS answer, ordinal AS position
         FROM messages WHERE run_id = ?1 AND role = 'user'
           AND ((steering = 0 AND ?2 = 0 AND ?3 IS NULL
                 AND state IN ('complete', 'cancelled', 'failed', 'interrupted'))
                OR (steering = 1 AND state = 'complete' AND (?3 IS NULL OR turn_ordinal <= ?4)))
         UNION ALL
         SELECT id, turn_ordinal, 0, 1, call_ordinal FROM tool_calls
         WHERE run_id = ?1 AND name = 'ask_user' AND state = 'completed'
           AND approval_resolution = 'answered' AND is_error = 0 AND result IS NOT NULL
           AND (?3 IS NULL OR turn_ordinal <= ?3)
         ORDER BY turn_ordinal, answer, position",
    )?;
    let mut message = connection.prepare_cached(
        "WITH message AS (
             SELECT m.output || COALESCE((SELECT group_concat(c.text, '') FROM (
                 SELECT text FROM message_chunks WHERE message_id = m.id AND channel = 'output'
                 ORDER BY chunk_ordinal
             ) c), '') AS text FROM messages m WHERE m.id = ?1
         ) SELECT substr(text, 1, ?2), length(CAST(text AS BLOB)) FROM message",
    )?;
    let mut answer = connection.prepare_cached(
        "SELECT substr(result, 1, ?2), length(CAST(result AS BLOB)) FROM tool_calls WHERE id = ?1",
    )?;
    let mut header_written = false;
    while let Some(root) = roots.next()? {
        let run: String = root.get(0)?;
        let ordinal: u64 = root.get(1)?;
        let generated: bool = root.get(2)?;
        let mut first_kept = None;
        if let Some(cutoff) = turn_cutoff {
            let mut turns = connection.prepare_cached(
                "SELECT turn_ordinal, assistant_content_json, truncated FROM model_turns
                 WHERE run_id = ?1 AND turn_ordinal > ?2 ORDER BY turn_ordinal",
            )?;
            let mut turns = turns.query(params![run, cutoff])?;
            while let Some(turn) = turns.next()? {
                let content: String = turn.get(1)?;
                if turn_is_replayed(&content, turn.get(2)?)? {
                    first_kept = Some(turn.get::<_, u32>(0)?);
                    break;
                }
            }
        }
        let mut rows = sources.query(params![
            run,
            generated,
            turn_cutoff,
            first_kept.or(turn_cutoff)
        ])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let turn: u32 = row.get(1)?;
            let steering: bool = row.get(2)?;
            let is_answer: bool = row.get(3)?;
            let label = if is_answer {
                format!("user answers to agent questions, user message #{ordinal} turn {turn}")
            } else if steering {
                format!("steering during user message #{ordinal} turn {turn}")
            } else {
                format!("user message #{ordinal}")
            };
            let heading = if header_written {
                ""
            } else {
                EXACT_INPUTS_HEADER
            };
            let framing = if header_written {
                format!("\n--- {label} ---\n")
            } else {
                format!("\n\n{heading}\n--- {label} ---\n")
            };
            let room = MAX_CONTEXT_BYTES
                .saturating_sub(
                    COMPACTION_SUMMARY_PREAMBLE
                        .len()
                        .max(IN_RUN_COMPACTION_PREAMBLE.len())
                        + 2,
                )
                .checked_sub(summary.len() + framing.len() + 1)
                .ok_or(SessionRuntimeError::ContextTooLarge)?;
            let statement = if is_answer { &mut answer } else { &mut message };
            let (text, bytes): (String, usize) =
                statement.query_row(params![id, room + 1], |row| Ok((row.get(0)?, row.get(1)?)))?;
            if bytes > room {
                return Err(SessionRuntimeError::ContextTooLarge);
            }
            summary.push_str(&framing);
            summary.push_str(&text);
            summary.push('\n');
            header_written = true;
        }
    }
    Ok(summary)
}
