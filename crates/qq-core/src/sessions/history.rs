use super::*;

pub(super) fn search_session_history(
    connection: &Connection,
    session_id: SessionId,
    query: &str,
    limit: usize,
    cursor: Option<HistoryCursor>,
) -> Result<HistorySearch, SessionRuntimeError> {
    let needle = query.to_lowercase();
    let mut cursor = cursor.unwrap_or_default();
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    for _ in 0..HISTORY_SCAN_STEPS {
        let (next, unit, bytes) = step(connection, session_id, cursor)?;
        if let Some((citation, text)) = unit
            && let Some(excerpt) = excerpt_around(&text, &needle)
        {
            matches.push(HistoryMatch { citation, excerpt });
        }
        scanned = scanned.saturating_add(bytes);
        let Some(next) = next else {
            return Ok(HistorySearch {
                matches,
                next_cursor: None,
            });
        };
        cursor = next;
        if matches.len() >= limit || scanned >= HISTORY_SCAN_BUDGET_BYTES {
            break;
        }
    }
    Ok(HistorySearch {
        matches,
        next_cursor: Some(cursor),
    })
}

type HistoryStep = (Option<HistoryCursor>, Option<(String, String)>, usize);

fn step(
    connection: &Connection,
    session_id: SessionId,
    cursor: HistoryCursor,
) -> Result<HistoryStep, SessionRuntimeError> {
    let session = session_id.to_string();
    let empty = |next| (Some(next), None, 0);
    match cursor {
        HistoryCursor::Prompt { before } => {
            let row = connection.prepare_cached(
                "SELECT ordinal, CASE WHEN role = 'user'
                     AND ((steering = 0 AND state IN ('complete', 'cancelled', 'failed', 'interrupted', 'streaming'))
                          OR (steering = 1 AND state = 'complete')) THEN output || COALESCE((SELECT group_concat(text, '') FROM (SELECT text FROM message_chunks WHERE message_id = messages.id AND channel = 'output' ORDER BY chunk_ordinal)), '') END, steering
                 FROM messages WHERE session_id = ?1 AND ordinal < ?2
                 ORDER BY ordinal DESC LIMIT 1",
            )?.query_row(params![session, before], |row| Ok((
                row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, bool>(2)?,
            ))).optional()?;
            let Some((prompt, text, steering)) = row else {
                return Ok((None, None, 0));
            };
            let Some(text) = text else {
                return Ok(empty(HistoryCursor::Prompt { before: prompt }));
            };
            let next = if steering {
                HistoryCursor::Prompt { before: prompt }
            } else {
                HistoryCursor::Turn {
                    prompt,
                    before: u32::MAX,
                }
            };
            let bytes = text.len();
            Ok((
                Some(next),
                Some((format!("user message #{prompt}"), text)),
                bytes,
            ))
        }
        HistoryCursor::Turn { prompt, before } => {
            let turn = connection.prepare_cached(
                "SELECT turn_ordinal FROM model_turns
                 WHERE run_id = (SELECT run_id FROM messages WHERE session_id = ?1 AND ordinal = ?2 AND role = 'user' AND steering = 0)
                   AND turn_ordinal < ?3 ORDER BY turn_ordinal DESC LIMIT 1",
            )?.query_row(params![session, prompt, before], |row| row.get::<_, u32>(0)).optional()?;
            Ok(empty(match turn {
                Some(turn) => HistoryCursor::Result {
                    prompt,
                    turn,
                    before: u16::MAX,
                },
                None => HistoryCursor::Prompt { before: prompt },
            }))
        }
        HistoryCursor::Result {
            prompt,
            turn,
            before,
        } => {
            let row = connection.prepare_cached(
                "SELECT name, call_ordinal, CASE WHEN name = 'search_history' THEN NULL ELSE result END FROM tool_calls
                 WHERE run_id = (SELECT run_id FROM messages WHERE session_id = ?1 AND ordinal = ?2 AND role = 'user' AND steering = 0)
                   AND turn_ordinal = ?3 AND call_ordinal < ?4
                 ORDER BY call_ordinal DESC LIMIT 1",
            )?.query_row(params![session, prompt, turn, before], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, u16>(1)?, row.get::<_, Option<String>>(2)?,
            ))).optional()?;
            let Some((name, call, text)) = row else {
                return Ok(empty(HistoryCursor::Assistant { prompt, turn }));
            };
            let next = HistoryCursor::Result {
                prompt,
                turn,
                before: call,
            };
            let Some(text) = text else {
                return Ok(empty(next));
            };
            let bytes = text.len();
            let unit = (!is_history_tool(&name)).then(|| {
                (
                    format!("{name} result, user message #{prompt} turn {turn} call {call}"),
                    text,
                )
            });
            Ok((Some(next), unit, bytes))
        }
        HistoryCursor::Assistant { prompt, turn } => {
            let raw: Option<String> = connection.prepare_cached(
                "SELECT assistant_content_json FROM model_turns
                 WHERE run_id = (SELECT run_id FROM messages WHERE session_id = ?1 AND ordinal = ?2 AND role = 'user' AND steering = 0)
                   AND turn_ordinal = ?3",
            )?.query_row(params![session, prompt, turn], |row| row.get(0)).optional()?;
            let next = HistoryCursor::Turn {
                prompt,
                before: turn,
            };
            let Some(raw) = raw else {
                return Ok(empty(next));
            };
            let bytes = raw.len();
            let (codec::PersistedTurn::Legacy(content)
            | codec::PersistedTurn::Replay { content, .. }) = serde_json::from_str(&raw)?;
            drop(raw);
            let mut text = String::new();
            for block in content {
                let piece = match block {
                    codec::PersistedContentBlock::Text { text } => text,
                    codec::PersistedContentBlock::ToolCall {
                        name, arguments, ..
                    } if !is_history_tool(&name) => format!("{name} {arguments}"),
                    codec::PersistedContentBlock::ToolCall { .. }
                    | codec::PersistedContentBlock::ToolResult { .. } => continue,
                };
                if text.is_empty() {
                    text = piece;
                } else {
                    text.push('\n');
                    text.push_str(&piece);
                }
            }
            Ok((
                Some(next),
                Some((
                    format!("assistant, user message #{prompt} turn {turn}"),
                    text,
                )),
                bytes,
            ))
        }
    }
}

fn is_history_tool(name: &str) -> bool {
    name == "search_history"
}
