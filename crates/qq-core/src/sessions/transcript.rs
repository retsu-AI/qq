use super::*;

struct StoredAttachment {
    path: String,
    window: Option<(usize, usize, usize)>,
    content: Option<String>,
}

pub(super) fn store_message_attachments(
    transaction: &Connection,
    session_id: SessionId,
    message_id: &str,
    attachments: &[crate::input::ResolvedAttachment],
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let session = session_id.to_string();
    for (ordinal, attachment) in attachments.iter().enumerate() {
        let blob_key = match attachment.window {
            None => attachment.digest.clone(),
            Some((start, end, _)) => format!("{}:{start}-{end}", attachment.digest),
        };
        transaction.execute(
            "INSERT OR IGNORE INTO attachment_blobs(session_id, blob_key, content, content_bytes,
                                                   created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session,
                blob_key,
                attachment.content.as_bytes(),
                attachment.content.len() as u64,
                now,
            ],
        )?;
        let (window_start, window_end, window_total) = match attachment.window {
            None => (None, None, None),
            Some((start, end, total)) => (Some(start as u64), Some(end as u64), Some(total as u64)),
        };
        transaction.execute(
            "INSERT INTO message_attachments(message_id, ordinal, session_id, path, digest,
                                             blob_key, window_start, window_end, window_total)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                message_id,
                ordinal as u64,
                session,
                attachment.path,
                attachment.digest,
                blob_key,
                window_start,
                window_end,
                window_total,
            ],
        )?;
    }
    let held: u64 = transaction.query_row(
        "SELECT COALESCE(SUM(content_bytes), 0) FROM attachment_blobs
         WHERE session_id = ?1 AND content IS NOT NULL",
        [&session],
        |row| row.get(0),
    )?;
    if held > MAX_SESSION_ATTACHMENT_BYTES {
        let mut excess = held - MAX_SESSION_ATTACHMENT_BYTES;
        let mut statement = transaction.prepare(
            "SELECT b.blob_key, b.content_bytes FROM attachment_blobs b
             WHERE b.session_id = ?1 AND b.content IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM message_attachments a
                   JOIN messages m ON m.id = a.message_id
                   JOIN runs r ON r.id = m.run_id
                   WHERE a.session_id = b.session_id AND a.blob_key = b.blob_key
                     AND r.status IN ('running', 'preparing', 'queued'))
             ORDER BY b.created_at_ms ASC, b.blob_key ASC",
        )?;
        let candidates = statement
            .query_map([&session], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (key, bytes) in candidates {
            if excess == 0 {
                break;
            }
            transaction.execute(
                "UPDATE attachment_blobs SET content = NULL, evicted_at_ms = ?3
                 WHERE session_id = ?1 AND blob_key = ?2",
                params![session, key, now],
            )?;
            excess = excess.saturating_sub(bytes);
        }
    }
    Ok(())
}

fn load_retained_attachments(
    transaction: &Connection,
    session: &str,
    through_ordinal: u64,
    cutoff_ordinal: u64,
) -> Result<HashMap<String, Vec<StoredAttachment>>, SessionRuntimeError> {
    let mut statement = transaction.prepare_cached(
        "SELECT a.message_id, a.path, a.window_start, a.window_end, a.window_total, b.content
         FROM messages m
         JOIN messages s ON s.run_id = m.run_id AND s.role = 'user'
         JOIN message_attachments a ON a.message_id = s.id
         JOIN attachment_blobs b ON b.session_id = a.session_id AND b.blob_key = a.blob_key
         WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
           AND m.role = 'user' AND m.steering = 0
         ORDER BY a.message_id, a.ordinal",
    )?;
    let rows = statement.query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<u64>>(2)?,
            row.get::<_, Option<u64>>(3)?,
            row.get::<_, Option<u64>>(4)?,
            row.get::<_, Option<Vec<u8>>>(5)?,
        ))
    })?;
    let mut attachments: HashMap<String, Vec<StoredAttachment>> = HashMap::new();
    for row in rows {
        let (message_id, path, start, end, total, content) = row?;
        let window = match (start, end, total) {
            (Some(start), Some(end), Some(total)) => Some((
                usize::try_from(start).map_err(|_| SessionRuntimeError::CODEC)?,
                usize::try_from(end).map_err(|_| SessionRuntimeError::CODEC)?,
                usize::try_from(total).map_err(|_| SessionRuntimeError::CODEC)?,
            )),
            (None, None, None) => None,
            _ => return Err(SessionRuntimeError::CODEC),
        };
        let content = match content {
            None => None,
            Some(bytes) => Some(String::from_utf8(bytes).map_err(|_| SessionRuntimeError::CODEC)?),
        };
        attachments
            .entry(message_id)
            .or_default()
            .push(StoredAttachment {
                path,
                window,
                content,
            });
    }
    Ok(attachments)
}

pub(super) fn load_message(
    connection: &Connection,
    message_id: MessageId,
) -> Result<MessageSnapshot, SessionRuntimeError> {
    let (session, run, turn_ordinal, role, state, output, refusal, created, steering, truncated) =
        connection.query_row(
            "SELECT session_id, run_id, turn_ordinal, role, state, output, refusal,
                        created_at_ms, steering, truncated
                 FROM messages WHERE id = ?1",
            [message_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, u64>(7)?,
                    row.get::<_, bool>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            },
        )?;
    let (output, refusal) = load_message_text(connection, message_id, output, refusal)?;
    Ok(MessageSnapshot {
        id: message_id,
        session_id: parse_id(&session)?,
        run_id: parse_id(&run)?,
        turn_ordinal,
        role: parse_message_role(&role)?,
        state: parse_message_state(&state)?,
        steering,
        truncated,
        output,
        refusal,
        created_at_ms: created,
    })
}

pub(super) fn load_message_text(
    connection: &Connection,
    message_id: MessageId,
    mut output: String,
    mut refusal: String,
) -> Result<(String, String), SessionRuntimeError> {
    let mut statement = connection.prepare(
        "SELECT channel, text FROM message_chunks
             WHERE message_id = ?1 ORDER BY channel, chunk_ordinal",
    )?;
    let chunks = statement
        .query_map([message_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let (output_bytes, refusal_bytes) = chunks.iter().fold(
        (0_usize, 0_usize),
        |(output_bytes, refusal_bytes), (channel, text)| match channel.as_str() {
            "output" => (output_bytes.saturating_add(text.len()), refusal_bytes),
            "refusal" => (output_bytes, refusal_bytes.saturating_add(text.len())),
            _ => (usize::MAX, usize::MAX),
        },
    );
    if output_bytes == usize::MAX || refusal_bytes == usize::MAX {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    output.reserve(output_bytes);
    refusal.reserve(refusal_bytes);
    for (channel, text) in chunks {
        match channel.as_str() {
            "output" => output.push_str(&text),
            "refusal" => refusal.push_str(&text),
            _ => return Err(SessionRuntimeError::CONSTRAINT),
        }
    }
    Ok((output, refusal))
}

pub(super) fn load_model_context(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<Vec<Message>, SessionRuntimeError> {
    load_model_context_with_rewrite_status(transaction, session_id, through_ordinal)
        .map(|(context, _)| context)
}

pub(super) fn load_model_context_with_rewrite_status(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<(Vec<Message>, bool), SessionRuntimeError> {
    load_model_context_with_units(transaction, session_id, through_ordinal)
        .map(|(context, rewritten, _)| (context, rewritten))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContextUnit {
    pub(super) prompt_ordinal: u64,
    pub(super) end: usize,
}

pub(super) fn load_model_context_with_units(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<(Vec<Message>, bool, Vec<ContextUnit>), SessionRuntimeError> {
    assemble_model_context(transaction, session_id, through_ordinal, None)
        .map(|assembled| (assembled.context, assembled.rewritten, assembled.units))
}

pub(super) type ResultEffects = Vec<Option<EffectClass>>;

pub(super) fn load_model_context_with_effects(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<(Vec<Message>, ResultEffects), SessionRuntimeError> {
    let Assembled {
        context, effects, ..
    } = assemble_model_context(transaction, session_id, through_ordinal, None)?;
    let mut ordered = Vec::new();
    for (message_index, message) in context.iter().enumerate() {
        for (block_index, block) in message.content().iter().enumerate() {
            if matches!(block, ContentBlock::ToolResult { .. }) {
                ordered.push(effects.get(&(message_index, block_index)).copied());
            }
        }
    }
    Ok((context, ordered))
}

struct Assembled {
    context: Vec<Message>,
    rewritten: bool,
    units: Vec<ContextUnit>,
    effects: HashMap<(usize, usize), EffectClass>,
}

fn assemble_model_context(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
    window_start: Option<u64>,
) -> Result<Assembled, SessionRuntimeError> {
    let compaction = if window_start.is_some() {
        None
    } else {
        latest_compaction(transaction, session_id)?
    };
    let cutoff_ordinal = window_start.unwrap_or_else(|| {
        compaction
            .as_ref()
            .map_or(0, |compaction| compaction.cutoff_ordinal)
    });
    let watermark: Option<(u64, u32)> = transaction
        .query_row(
            "SELECT prune_through_ordinal, prune_through_turn FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| {
                Ok(row
                    .get::<_, Option<u64>>(0)?
                    .zip(row.get::<_, Option<u32>>(1)?))
            },
        )
        .optional()?
        .flatten();
    let through_ordinal = through_ordinal.min(u64::try_from(i64::MAX).unwrap_or(u64::MAX));
    let session = session_id.to_string();

    struct Prompt {
        message_id: String,
        run_id: String,
        text: String,
        input_json: Option<String>,
        status: String,
        outcome_json: Option<String>,
        ordinal: u64,
        goal_initial_notice: Option<String>,
    }
    let mut remaining_turn_bytes = 64 * 1024 * 1024_usize;
    let mut statement = transaction.prepare_cached(
        "SELECT m.run_id, m.output, r.status, r.outcome_json,
                    (SELECT group_concat(c.text, '') FROM (
                         SELECT text FROM message_chunks
                         WHERE message_id = m.id AND channel = 'output'
                         ORDER BY chunk_ordinal
                     ) c),
                    m.id, m.input_json, m.ordinal, r.goal_initial_notice
             FROM messages m JOIN runs r ON r.id = m.run_id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
             ORDER BY m.ordinal",
    )?;
    let prompts = statement
        .query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
            let mut text = row.get::<_, String>(1)?;
            if let Some(chunks) = row.get::<_, Option<String>>(4)? {
                text.push_str(&chunks);
            }
            Ok(Prompt {
                message_id: row.get(5)?,
                run_id: row.get(0)?,
                text,
                input_json: row.get(6)?,
                status: row.get(2)?,
                outcome_json: row.get(3)?,
                ordinal: row.get(7)?,
                goal_initial_notice: goals::read_notice_with_budget(
                    row,
                    8,
                    &mut remaining_turn_bytes,
                )?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut attachments =
        load_retained_attachments(transaction, &session, through_ordinal, cutoff_ordinal)?;
    let mut in_run = in_run_compactions(
        transaction,
        &session,
        through_ordinal,
        cutoff_ordinal,
        &mut remaining_turn_bytes,
    )?;
    let mut delivered =
        deliveries::retained_deliveries(transaction, &session, through_ordinal, cutoff_ordinal)?;

    let mut turns: HashMap<String, Vec<StoredTurn>> = HashMap::new();
    let mut statement = transaction.prepare_cached(
        "SELECT t.run_id, t.turn_ordinal, t.assistant_content_json, t.truncated, t.notice,
                    CASE WHEN json_valid(r.plan_descriptor_json) AND json_valid(r.plan_identity_json)
                          AND json_extract(r.plan_descriptor_json,'$.version') BETWEEN 10 AND 14
                          AND json_extract(r.plan_descriptor_json,'$.prompt_version') BETWEEN 1 AND 19
                          AND json_extract(r.plan_identity_json,'$.descriptor_version') = json_extract(r.plan_descriptor_json,'$.version')
                         THEN 1 ELSE 0 END,
                    CASE WHEN json_valid(r.plan_descriptor_json) AND json_valid(r.plan_identity_json)
                          AND json_extract(r.plan_descriptor_json,'$.version') IN (15,16)
                          AND json_extract(r.plan_identity_json,'$.descriptor_version')=json_extract(r.plan_descriptor_json,'$.version')
                          AND json_extract(r.plan_descriptor_json,'$.tool_output_projection')='strict_reserved_v1'
                         THEN 1 ELSE 0 END,
                    CASE WHEN t.tool_results_projection_json IS NULL THEN NULL
                         WHEN length(CAST(t.tool_results_projection_json AS BLOB))<=2097152 THEN t.tool_results_projection_json END,
                    length(CAST(t.tool_results_projection_json AS BLOB)),
                    -- Legacy review consequences are durable model state, not
                    -- delivery events that a reconnect journal may retire.
                    CASE WHEN t.tool_results_projection_json IS NOT NULL THEN 0
                         WHEN CASE WHEN json_valid(r.plan_descriptor_json)
                                   THEN json_type(r.plan_descriptor_json,'$.checkpoint') IS NULL
                                        OR json_type(r.plan_descriptor_json,'$.checkpoint')='null'
                                   ELSE 0 END THEN 0
                         ELSE t.legacy_checkpoint_required END, t.goal_notice
             FROM messages m
             JOIN runs r ON r.id = m.run_id
             JOIN model_turns t ON t.run_id = r.id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
             ORDER BY t.run_id, t.turn_ordinal",
    )?;
    let mut rows = statement.query(params![session, through_ordinal, cutoff_ordinal])?;
    while let Some(row) = rows.next()? {
        let raw = row
            .get_ref(2)?
            .as_str()
            .map_err(|_| SessionRuntimeError::CONSTRAINT)?;
        if raw.len() > remaining_turn_bytes {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        remaining_turn_bytes -= raw.len();
        let projection_bytes = row.get::<_, Option<usize>>(8)?.unwrap_or(0);
        if projection_bytes > super::projection::MAX_PROJECTION_JSON_BYTES
            || projection_bytes > remaining_turn_bytes
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        remaining_turn_bytes -= projection_bytes;
        let turn = StoredTurn {
            strict_projection: row.get(6)?,
            checkpoint_projection_required: row.get(9)?,
            projection_json: row.get(7)?,
            run_id: row.get(0)?,
            legacy_projection: row.get(5)?,
            ordinal: row.get::<_, u32>(1)?,
            content_json: raw.to_owned(),
            truncated: row.get::<_, bool>(3)?,
            notice: match row.get_ref(4)?.as_str_or_null() {
                Ok(None) => None,
                Ok(Some(stored)) => Some(
                    crate::runtime::TurnNotice::from_stored(stored)
                        .ok_or(SessionRuntimeError::CODEC)?,
                ),
                Err(_) => return Err(SessionRuntimeError::CONSTRAINT),
            },
            goal_notices: goals::read_turn_notices(row, 10, &mut remaining_turn_bytes)?,
        };
        turns
            .entry(row.get::<_, String>(0)?)
            .or_default()
            .push(turn);
    }
    drop(rows);
    drop(statement);

    let mut results: HashMap<String, RecordedTurnResults> = HashMap::new();
    let mut statement = transaction.prepare_cached(
        "SELECT c.run_id, c.provider_call_id, c.result, c.is_error, c.effect, c.turn_ordinal,
                c.id, s.digest
             FROM messages m
             JOIN runs r ON r.id = m.run_id
             JOIN tool_calls c ON c.run_id = r.id
             LEFT JOIN tool_spills s ON s.tool_call_id = c.id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
               AND c.result IS NOT NULL",
    )?;
    let rows = statement.query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, bool>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, u32>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    for row in rows {
        let (run_id, call_id, content, is_error, effect, turn_ordinal, id, spill_digest) = row?;
        let effect = effect.as_deref().and_then(EffectClass::from_stored);
        results
            .entry(run_id)
            .or_default()
            .entry(turn_ordinal)
            .or_default()
            .insert(
                call_id,
                RecordedResult {
                    content,
                    is_error,
                    effect,
                    id: parse_id(&id)?,
                    spill_digest,
                },
            );
    }
    drop(statement);

    let mut steering: HashMap<String, std::collections::VecDeque<(u32, String)>> = HashMap::new();
    let mut statement = transaction.prepare_cached(
        "SELECT s.run_id, s.turn_ordinal, s.output, s.id, s.input_json
             FROM messages m
             JOIN messages s ON s.run_id = m.run_id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
               AND s.steering = 1 AND s.state = 'complete'
             ORDER BY s.run_id, s.turn_ordinal, s.ordinal",
    )?;
    let rows = statement.query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in rows {
        let (run_id, ordinal, text, message_id, input_json) = row?;
        let text = match attachments.remove(&message_id) {
            Some(stored) => {
                let parts = parse_input_parts(input_json.as_deref())?;
                crate::input::render_resolved_prompt(
                    &crate::input::render_text_parts(&parts),
                    stored.iter().map(|attachment| {
                        (
                            attachment.path.as_str(),
                            attachment.window,
                            attachment.content.as_deref(),
                        )
                    }),
                )
            }
            None => text,
        };
        steering
            .entry(run_id)
            .or_default()
            .push_back((ordinal, text));
    }
    drop(statement);

    let mut context = Vec::new();
    let mut effects = HashMap::new();
    let mut units = Vec::with_capacity(prompts.len());
    let mut prune_limit = 0_usize;
    if let Some(compaction) = compaction {
        context.push(Message::user(format!(
            "{COMPACTION_SUMMARY_PREAMBLE}\n\n{}",
            with_exact_inputs(
                transaction,
                &session_id.to_string(),
                RecordScope::Session {
                    cutoff_ordinal: compaction.cutoff_ordinal,
                },
                compaction.summary
            )?
        )));
    }
    for prompt in prompts {
        let prompt_ordinal = prompt.ordinal;
        let text = match attachments.remove(&prompt.message_id) {
            Some(stored) => {
                let parts = parse_input_parts(prompt.input_json.as_deref())?;
                crate::input::render_resolved_prompt(
                    &crate::input::render_text_parts(&parts),
                    stored.iter().map(|attachment| {
                        (
                            attachment.path.as_str(),
                            attachment.window,
                            attachment.content.as_deref(),
                        )
                    }),
                )
            }
            None => prompt.text,
        };
        if let Some(notice) = prompt.goal_initial_notice {
            context.push(Message::user(notice));
        }
        context.push(Message::user(text));
        if matches!(
            prompt.status.as_str(),
            "completed"
                | "cancelled"
                | "failed"
                | "interrupted"
                | "budget_exhausted"
                | "paused"
                | "running"
        ) {
            match turns.remove(&prompt.run_id) {
                Some(run_turns) => {
                    let ends = append_run_turns(
                        run_turns,
                        results.remove(&prompt.run_id).unwrap_or_default(),
                        steering.remove(&prompt.run_id).unwrap_or_default(),
                        delivered.remove(&prompt.run_id).unwrap_or_default(),
                        in_run.remove(&prompt.run_id),
                        &mut context,
                        &mut effects,
                    )?;
                    if let Some((_, through)) =
                        watermark.filter(|(ordinal, _)| *ordinal == prompt_ordinal)
                    {
                        prune_limit = ends
                            .iter()
                            .rev()
                            .find(|(turn, _)| *turn <= through)
                            .map_or(prune_limit, |(_, end)| *end);
                    }
                }
                None => {
                    append_legacy_run_messages(
                        transaction,
                        parse_id(&prompt.run_id)?,
                        &mut context,
                    )?;
                    for (_, text) in delivered.remove(&prompt.run_id).unwrap_or_default() {
                        context.push(Message::user(text));
                    }
                }
            }
        }
        if matches!(
            prompt.status.as_str(),
            "cancelled" | "failed" | "interrupted" | "budget_exhausted" | "paused"
        ) {
            let outcome_json = prompt.outcome_json.ok_or(SessionRuntimeError::CODEC)?;
            let outcome: RunOutcome = serde_json::from_str(&outcome_json)?;
            if let Some(notice) = runtime_notice(&outcome) {
                context.push(Message::user(notice));
            }
        }
        if watermark.is_some_and(|(ordinal, _)| prompt_ordinal < ordinal) {
            prune_limit = context.len();
        }
        units.push(ContextUnit {
            prompt_ordinal,
            end: context.len(),
        });
    }
    let rewritten = prune_stale_tool_results(&mut context[..prune_limit], &effects);
    Ok(Assembled {
        context,
        rewritten,
        units,
        effects,
    })
}

pub(super) fn capture_side_context(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<Message>, SessionRuntimeError> {
    const LIMIT: usize = 32 * 1024;
    const SCAN_BYTES: u64 = 128 * 1024;
    const NOTICE: &str = "[Captured context omits older or oversized complete units. Live inspection reads current files, not a filesystem snapshot.]";
    let summary: Option<(u64, Option<String>)> = connection
        .query_row(
            "SELECT cutoff_ordinal, CASE WHEN length(CAST(summary AS BLOB)) <= ?2
             THEN summary ELSE NULL END FROM session_compactions
         WHERE session_id = ?1 AND scope_run_id IS NULL ORDER BY rowid DESC LIMIT 1",
            params![session_id.to_string(), LIMIT.saturating_sub(NOTICE.len())],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let cutoff = summary.as_ref().map_or(0, |(cutoff, _)| *cutoff);
    let mut statement = connection.prepare_cached(
        "SELECT m.ordinal, m.run_id FROM messages m JOIN runs r ON r.id = m.run_id
         WHERE m.session_id = ?1 AND m.ordinal > ?2 AND m.role = 'user' AND m.steering = 0
           AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
           AND r.status IN ('completed', 'cancelled', 'failed', 'interrupted', 'budget_exhausted', 'paused')
         ORDER BY m.ordinal DESC LIMIT 65",
    )?;
    let candidates = statement
        .query_map(params![session_id.to_string(), cutoff], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut remaining = LIMIT.saturating_sub(NOTICE.len());
    let mut scanned = 0_u64;
    let mut newest = Vec::new();
    let unfinished: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1 AND status IN ('running', 'preparing', 'queued'))",
        [session_id.to_string()], |row| row.get(0),
    )?;
    let mut omitted = candidates.len() > 64 || unfinished;
    for (ordinal, run) in candidates.into_iter().take(64) {
        let (bytes, rows): (u64, u64) = connection.query_row(
            "SELECT COALESCE(SUM(bytes), 0), COUNT(*) FROM (
              SELECT length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
                     + COALESCE(length(CAST(input_json AS BLOB)), 0) AS bytes
                FROM messages WHERE run_id = ?1
              UNION ALL SELECT length(CAST(c.text AS BLOB)) FROM message_chunks c
                JOIN messages m ON m.id = c.message_id WHERE m.run_id = ?1
              UNION ALL SELECT length(CAST(assistant_content_json AS BLOB))
                     + COALESCE(length(CAST(goal_notice AS BLOB)), 0)
                FROM model_turns WHERE run_id = ?1
              UNION ALL SELECT COALESCE(length(CAST(result AS BLOB)), 0)
                FROM tool_calls WHERE run_id = ?1
              UNION ALL SELECT length(CAST(a.path AS BLOB)) + COALESCE(length(b.content), 0) + 128
                FROM message_attachments a JOIN messages m ON m.id = a.message_id
                JOIN attachment_blobs b ON b.session_id = a.session_id AND b.blob_key = a.blob_key
                WHERE m.run_id = ?1
              UNION ALL SELECT length(CAST(summary AS BLOB))
                     + COALESCE(length(CAST(goal_notice AS BLOB)), 0) FROM session_compactions
                WHERE scope_run_id = ?1
              UNION ALL SELECT COALESCE(length(CAST(text AS BLOB)), 0) FROM child_deliveries
                WHERE parent_run_id = ?1
              UNION ALL SELECT COALESCE(length(CAST(text AS BLOB)), 0) FROM child_reports
                WHERE parent_run_id = ?1
              UNION ALL SELECT COALESCE(length(CAST(outcome_json AS BLOB)), 0)
                     + COALESCE(length(CAST(goal_initial_notice AS BLOB)), 0)
                FROM runs WHERE id = ?1
              LIMIT 1025
            )",
            [&run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if rows > 1024 || bytes > SCAN_BYTES.saturating_sub(scanned) {
            omitted = true;
            break;
        }
        scanned += bytes;
        let unit = assemble_model_context(
            connection,
            session_id,
            ordinal,
            Some(ordinal.saturating_sub(1)),
        )?
        .context;
        let bytes = context_bytes(&unit);
        if bytes > remaining {
            omitted = true;
            break;
        }
        remaining -= bytes;
        newest.push(unit);
    }
    let mut active_context = Vec::new();
    let active_prompt: Option<(Option<String>, Option<String>)> = connection.query_row(
        "SELECT CASE WHEN length(CAST(m.output AS BLOB))
                + COALESCE(length(CAST(r.goal_initial_notice AS BLOB)),0) <= ?2 THEN m.output ELSE NULL END,
                CASE WHEN length(CAST(m.output AS BLOB))
                + COALESCE(length(CAST(r.goal_initial_notice AS BLOB)),0) <= ?2 THEN r.goal_initial_notice ELSE NULL END
         FROM messages m JOIN sessions s ON s.active_run_id = m.run_id JOIN runs r ON r.id=m.run_id
         WHERE m.session_id = ?1 AND m.role = 'user' AND m.steering = 0 ORDER BY m.ordinal DESC LIMIT 1",
        params![session_id.to_string(), remaining.saturating_sub(128)], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    if let Some((Some(prompt), goal_notice)) = active_prompt {
        if let Some(notice) = goal_notice {
            remaining = remaining.saturating_sub(notice.len());
            active_context.push(Message::user(notice));
        }
        let prompt =
            format!("[Current main task; unfinished output and tool exchanges omitted]\n{prompt}");
        remaining = remaining.saturating_sub(prompt.len());
        active_context.push(Message::user(prompt));
        let mut statement = connection.prepare_cached(
            "SELECT m.id, length(CAST(m.output AS BLOB)) + COALESCE((
                    SELECT SUM(length(CAST(c.text AS BLOB))) FROM message_chunks c
                    WHERE c.message_id = m.id), 0)
             FROM messages m JOIN sessions s ON s.active_run_id = m.run_id
             WHERE m.session_id = ?1 AND m.role = 'assistant' AND m.state = 'complete'
             ORDER BY m.ordinal DESC LIMIT 16",
        )?;
        let candidates = statement
            .query_map([session_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let bound = u64::try_from(remaining.saturating_sub(128).min(8192)).unwrap_or(u64::MAX);
        let mut texts = Vec::with_capacity(candidates.len());
        for (id, bytes) in candidates {
            if bytes > bound {
                continue;
            }
            let message_id: MessageId = parse_id(&id)?;
            let output: String = connection.query_row(
                "SELECT output FROM messages WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )?;
            let (output, _) = load_message_text(connection, message_id, output, String::new())?;
            texts.push(output);
        }
        for text in texts.into_iter().rev().filter(|text| !text.is_empty()) {
            let text = format!("[Committed main assistant text; tool exchanges omitted]\n{text}");
            if text.len() > remaining {
                break;
            }
            remaining -= text.len();
            active_context.push(Message::user(text));
        }
    }
    let mut context = Vec::new();
    if let Some((_, summary)) = summary {
        match summary {
            Some(summary) if COMPACTION_SUMMARY_PREAMBLE.len() + 2 + summary.len() <= remaining => {
                context.push(Message::user(format!(
                    "{COMPACTION_SUMMARY_PREAMBLE}\n\n{summary}"
                )));
            }
            Some(_) | None => omitted = true,
        }
    }
    if omitted {
        context.insert(0, Message::user(NOTICE));
    }
    for unit in newest.into_iter().rev() {
        context.extend(unit);
    }
    context.extend(active_context);
    Ok(context)
}

type RecordedTurnResults = HashMap<u32, HashMap<String, RecordedResult>>;

pub(super) fn advance_prune_watermark(
    connection: &mut Connection,
    identity: RunIdentity,
    through_turn: u32,
) -> Result<(), SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let anchored: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs r JOIN messages m ON m.id = r.user_message_id
                       WHERE r.id = ?1 AND r.session_id = ?2 AND r.kind = 'prompt')",
        params![identity.run_id.to_string(), identity.session_id.to_string()],
        |row| row.get(0),
    )?;
    if !anchored {
        return Err(SessionRuntimeError::Unavailable);
    }
    transaction.execute(
        "UPDATE sessions
             SET prune_through_ordinal = p.ordinal, prune_through_turn = ?3
             FROM (SELECT m.ordinal FROM runs r JOIN messages m ON m.id = r.user_message_id
                   WHERE r.id = ?2) AS p
             WHERE sessions.id = ?1
               AND (sessions.prune_through_ordinal IS NULL
                    OR sessions.prune_through_ordinal < p.ordinal
                    OR (sessions.prune_through_ordinal = p.ordinal
                        AND sessions.prune_through_turn < ?3))",
        params![
            identity.session_id.to_string(),
            identity.run_id.to_string(),
            through_turn
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn advance_prune_watermark_before_prompt(
    connection: &mut Connection,
    identity: RunIdentity,
) -> Result<bool, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let moved = transaction.execute(
        "UPDATE sessions
             SET prune_through_ordinal = latest.ordinal, prune_through_turn = latest.turn_ordinal
             FROM (SELECT m.ordinal, t.turn_ordinal
                   FROM messages m JOIN model_turns t ON t.run_id = m.run_id
                   WHERE m.session_id = ?1 AND m.role = 'user' AND m.steering = 0
                     AND m.ordinal < (SELECT p.ordinal FROM runs r
                                      JOIN messages p ON p.id = r.user_message_id
                                      WHERE r.id = ?2)
                   ORDER BY m.ordinal DESC, t.turn_ordinal DESC
                   LIMIT 1) AS latest
             WHERE sessions.id = ?1
               AND (sessions.prune_through_ordinal IS NULL
                    OR sessions.prune_through_ordinal < latest.ordinal
                    OR (sessions.prune_through_ordinal = latest.ordinal
                        AND sessions.prune_through_turn < latest.turn_ordinal))",
        params![identity.session_id.to_string(), identity.run_id.to_string()],
    )?;
    transaction.commit()?;
    Ok(moved == 1)
}

pub(super) struct RecordedResult {
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) effect: Option<EffectClass>,
    pub(super) id: ToolCallId,
    pub(super) spill_digest: Option<String>,
}

pub(super) fn append_legacy_run_messages(
    connection: &Connection,
    run_id: RunId,
    context: &mut Vec<Message>,
) -> Result<(), SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT m.output, m.refusal,
                    (SELECT group_concat(c.text, '') FROM (
                         SELECT text FROM message_chunks
                         WHERE message_id = m.id AND channel = 'output'
                         ORDER BY chunk_ordinal
                     ) c),
                    (SELECT group_concat(c.text, '') FROM (
                         SELECT text FROM message_chunks
                         WHERE message_id = m.id AND channel = 'refusal'
                         ORDER BY chunk_ordinal
                     ) c)
             FROM messages m
             WHERE m.run_id = ?1 AND m.role = 'assistant' AND m.state = 'complete'
             ORDER BY m.turn_ordinal, m.ordinal",
    )?;
    let rows = statement.query_map([run_id.to_string()], |row| {
        let mut output = row.get::<_, String>(0)?;
        let mut refusal = row.get::<_, String>(1)?;
        if let Some(chunks) = row.get::<_, Option<String>>(2)? {
            output.push_str(&chunks);
        }
        if let Some(chunks) = row.get::<_, Option<String>>(3)? {
            refusal.push_str(&chunks);
        }
        Ok((output, refusal))
    })?;
    for row in rows {
        let (output, refusal) = row?;
        let content = if output.is_empty() { refusal } else { output };
        if !content.trim().is_empty() {
            context.push(Message::assistant(content));
        }
    }
    Ok(())
}

pub(super) fn runtime_notice(outcome: &RunOutcome) -> Option<String> {
    let status = match outcome {
        RunOutcome::Completed => return None,
        RunOutcome::Cancelled => "The previous run was cancelled.".to_owned(),
        RunOutcome::Interrupted => "The previous run was interrupted before completion.".to_owned(),
        RunOutcome::BudgetExhausted { exhaustion } => format!(
            "The previous run stopped when its budget ran out: {}",
            exhaustion.message
        ),
        RunOutcome::Paused { pause } => match pause.as_ref() {
            qq_protocol::RunPause::ProviderRetry {
                attempts,
                turn_ordinal,
                message,
                ..
            } => format!(
                "The previous run paused after {attempts} retries of turn {turn_ordinal} on a provider fault ({message}); its completed turns stand. Continue from where it stopped."
            ),
            qq_protocol::RunPause::NoProgress { .. } => format!(
                "The previous run {pause}; its completed turns stand. Continue from where it stopped."
            ),
        },
        RunOutcome::Failed { failure } => format!("The previous run failed: {}", failure.message),
    };
    Some(format!(
        "{RUNTIME_NOTICE_PREAMBLE}\n{status}\n{RUNTIME_NOTICE_GUIDANCE}"
    ))
}

pub(crate) fn prune_stale_tool_results(
    context: &mut [Message],
    effects: &HashMap<(usize, usize), EffectClass>,
) -> bool {
    let assistant_positions = context
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role() == Role::Assistant)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let Some(&window_start) = assistant_positions
        .len()
        .checked_sub(CONTEXT_PRUNE_KEEP_TURNS)
        .and_then(|index| assistant_positions.get(index))
    else {
        return false;
    };
    let mut calls = HashMap::new();
    let mut rewritten = false;
    for (message_index, message) in context[..window_start].iter_mut().enumerate() {
        if message.role() == Role::Assistant {
            calls.clear();
            for block in message.content() {
                if let ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } = block
                {
                    calls.insert(id.clone(), (name.clone(), arguments.to_string()));
                }
            }
            continue;
        }
        if !message
            .content()
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        {
            continue;
        }
        let mut replacement: Option<Vec<ContentBlock>> = None;
        for (block_index, block) in message.content().iter().enumerate() {
            let stub = match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => prunable_stub(
                    &calls,
                    effects.get(&(message_index, block_index)).copied(),
                    call_id,
                    content,
                )
                .map(|stub| ContentBlock::ToolResult {
                    call_id: call_id.clone(),
                    content: stub,
                    is_error: *is_error,
                }),
                _ => None,
            };
            match stub {
                Some(stub) => {
                    let replacement = replacement.get_or_insert_with(|| {
                        let mut blocks = Vec::with_capacity(message.content().len());
                        blocks.extend_from_slice(&message.content()[..block_index]);
                        blocks
                    });
                    replacement.push(stub);
                }
                None => {
                    if let Some(replacement) = &mut replacement {
                        replacement.push(block.clone());
                    }
                }
            }
        }
        if let Some(content) = replacement {
            *message = Message::new(message.role(), content);
            rewritten = true;
        }
    }
    rewritten
}

pub(super) fn prunable_stub(
    calls: &HashMap<String, (String, String)>,
    effect: Option<EffectClass>,
    call_id: &str,
    content: &str,
) -> Option<String> {
    let (name, arguments) = calls.get(call_id)?;
    if !match effect {
        Some(effect) => effect == EffectClass::ReadOnly,
        None => PRUNABLE_READ_ONLY_TOOLS.contains(&name.as_str()),
    } {
        return None;
    }
    let mut arguments = arguments.clone();
    if arguments.len() > CONTEXT_PRUNE_STUB_ARGUMENT_BYTES {
        arguments = truncate_utf8(arguments, CONTEXT_PRUNE_STUB_ARGUMENT_BYTES);
        arguments.push_str("...");
    }
    let hint = if name == "read_file" {
        PRUNED_READ_REREAD
    } else {
        "call it again if needed"
    };
    let header_tool = if name == "read_file" { "read" } else { name };
    let last = match content.split_once('\n') {
        Some((_, rest)) if rest.contains('\n') => None,
        Some((_, rest)) => crate::tools::header_line(header_tool, content).map(|_| rest),
        None => Some(content),
    };
    if last
        .and_then(|line| line.strip_prefix("[pruned: "))
        .and_then(|line| line.strip_prefix(name.as_str()))
        .and_then(|line| line.strip_prefix(' '))
        .and_then(|line| line.strip_prefix(arguments.as_str()))
        .and_then(|line| line.strip_prefix(" returned "))
        .and_then(|line| line.strip_suffix(']'))
        .and_then(|line| line.strip_suffix(hint))
        .and_then(|line| line.strip_suffix(" bytes; "))
        .is_some_and(|size| {
            size.bytes().all(|byte| byte.is_ascii_digit()) && size.parse::<u64>().is_ok()
        })
    {
        return None;
    }
    let size = content.len();
    let header = if name == "read_file" {
        crate::tools::header_line("read", content).map(|header| {
            match header.rfind(" h:").map(|at| {
                let end = header[at + 1..]
                    .find(' ')
                    .map_or(header.len(), |len| at + 1 + len);
                (at, end)
            }) {
                Some((at, end)) if is_read_hash_token(&header[at + 1..end]) => {
                    format!("{}{}", &header[..at], &header[end..])
                }
                Some(_) | None => header.to_owned(),
            }
        })
    } else {
        crate::tools::header_line(name, content).map(str::to_owned)
    };
    let stub = match header {
        Some(header) => {
            format!("{header}\n[pruned: {name} {arguments} returned {size} bytes; {hint}]")
        }
        None => format!("[pruned: {name} {arguments} returned {size} bytes; {hint}]"),
    };
    (content.len() > stub.len()).then_some(stub)
}

const PRUNED_READ_REREAD: &str = "the text is no longer in context: read_file the lines you \
need again, without if_changed_since";

fn is_read_hash_token(token: &str) -> bool {
    token.strip_prefix("h:").is_some_and(|hash| {
        hash == "-" || (hash.len() == 12 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    })
}

pub(super) fn context_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(Message::content)
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => id.len() + name.len() + arguments.to_string().len(),
            ContentBlock::ToolResult {
                call_id, content, ..
            } => call_id.len() + content.len(),
        })
        .fold(
            messages
                .iter()
                .map(|message| message.replay().map_or(0, str::len))
                .fold(0usize, usize::saturating_add),
            usize::saturating_add,
        )
}

pub(super) fn assembled_context_bytes(
    transaction: &Connection,
    session_id: SessionId,
) -> Result<usize, SessionRuntimeError> {
    let context = load_model_context(transaction, session_id, u64::MAX)?;
    let streaming_bytes: u64 = transaction.query_row(
        "SELECT
                 (SELECT COALESCE(SUM(
                      length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
                  ), 0) FROM messages WHERE session_id = ?1 AND state = 'streaming')
                 +
                 (SELECT COALESCE(SUM(length(CAST(c.text AS BLOB))), 0)
                  FROM message_chunks c
                  JOIN messages m ON m.id = c.message_id
                  WHERE m.session_id = ?1 AND m.state = 'streaming')",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(context_bytes(&context)
        .saturating_add(usize::try_from(streaming_bytes).unwrap_or(usize::MAX)))
}

pub(super) fn turn_is_replayed(
    content_json: &str,
    truncated: bool,
) -> Result<bool, SessionRuntimeError> {
    if !truncated {
        return Ok(true);
    }
    let content = match serde_json::from_str::<codec::PersistedTurn>(content_json)? {
        codec::PersistedTurn::Legacy(content) | codec::PersistedTurn::Replay { content, .. } => {
            content
        }
    };
    Ok(Message::new(
        Role::Assistant,
        content.into_iter().map(ContentBlock::from).collect(),
    )
    .has_content())
}

pub(super) struct StoredTurn {
    pub(super) strict_projection: bool,
    pub(super) checkpoint_projection_required: bool,
    pub(super) projection_json: Option<String>,
    pub(super) run_id: String,
    pub(super) legacy_projection: bool,
    pub(super) ordinal: u32,
    pub(super) content_json: String,
    pub(super) truncated: bool,
    pub(super) notice: Option<crate::runtime::TurnNotice>,
    pub(super) goal_notices: Vec<crate::runtime::OrderedTurnNotice>,
}

pub(super) fn append_run_turns(
    turns: Vec<StoredTurn>,
    mut recorded: RecordedTurnResults,
    mut steering: std::collections::VecDeque<(u32, String)>,
    mut delivered: std::collections::VecDeque<(Option<u32>, String)>,
    compaction: Option<InRunCompaction>,
    context: &mut Vec<Message>,
    effects: &mut HashMap<(usize, usize), EffectClass>,
) -> Result<Vec<(u32, usize)>, SessionRuntimeError> {
    let mut ends = Vec::with_capacity(turns.len() + 1);
    let mut turns = turns;
    if let Some(marker) = compaction {
        let replaced = turns
            .iter()
            .take_while(|turn| turn.ordinal <= marker.turn_cutoff)
            .count();
        if replaced > 0 {
            turns.drain(..replaced);
            for ordinal in 1..=marker.turn_cutoff {
                recorded.remove(&ordinal);
            }
            let mut first_kept_index = None;
            for (index, turn) in turns.iter().enumerate() {
                if turn_is_replayed(&turn.content_json, turn.truncated)? {
                    first_kept_index = Some(index);
                    break;
                }
            }
            let first_kept =
                first_kept_index.map_or(marker.turn_cutoff, |index| turns[index].ordinal);
            if let Some(index) = first_kept_index {
                for turn in &mut turns[..=index] {
                    turn.notice = None;
                    turn.goal_notices.clear();
                }
            }
            while steering
                .front()
                .is_some_and(|(applied_before, _)| *applied_before <= first_kept)
            {
                steering.pop_front();
            }
            while delivered
                .front()
                .is_some_and(|(before, _)| before.is_some_and(|before| before <= first_kept))
            {
                delivered.pop_front();
            }
            context.push(Message::user(format!(
                "{IN_RUN_COMPACTION_PREAMBLE}\n\n{}",
                marker.summary
            )));
            if let Some(notice) = marker.goal_notice {
                context.push(Message::user(notice));
            }
            ends.push((marker.turn_cutoff, context.len()));
        }
    }
    for StoredTurn {
        strict_projection,
        checkpoint_projection_required,
        projection_json,
        run_id,
        legacy_projection,
        ordinal: turn_ordinal,
        content_json,
        truncated,
        notice,
        goal_notices,
    } in turns
    {
        let mut recorded_turn = recorded.remove(&turn_ordinal).unwrap_or_default();
        while steering
            .front()
            .is_some_and(|(applied_before, _)| *applied_before <= turn_ordinal)
        {
            let (_, text) = steering.pop_front().expect("front was just checked");
            context.push(Message::user(text));
        }
        let positioned_notice =
            notice.is_some_and(|notice| goal_notices.iter().any(|atom| atom.text == notice.text()));
        let mut preceding_deliveries = 0;
        for atom in goal_notices {
            if atom.preceding_deliveries < preceding_deliveries {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            while preceding_deliveries < atom.preceding_deliveries {
                if !delivered
                    .front()
                    .is_some_and(|(before, _)| before.is_some_and(|before| before <= turn_ordinal))
                {
                    return Err(SessionRuntimeError::CONSTRAINT);
                }
                let (_, text) = delivered.pop_front().expect("front was just checked");
                context.push(Message::user(text));
                preceding_deliveries += 1;
            }
            context.push(Message::user(atom.text));
        }
        while delivered
            .front()
            .is_some_and(|(before, _)| before.is_some_and(|before| before <= turn_ordinal))
        {
            let (_, text) = delivered.pop_front().expect("front was just checked");
            context.push(Message::user(text));
        }
        if let Some(notice) = notice.filter(|_| !positioned_notice) {
            context.push(Message::user(notice.text()));
        }
        let (content, replay) =
            match serde_json::from_str::<super::codec::PersistedTurn>(&content_json)? {
                super::codec::PersistedTurn::Legacy(content) => (content, None),
                super::codec::PersistedTurn::Replay { content, replay } => (content, Some(replay)),
            };
        let content: Vec<ContentBlock> = content.into_iter().map(ContentBlock::from).collect();
        let mut saved_projection = projection_json
            .as_deref()
            .map(super::projection::SavedProjection::decode)
            .transpose()?;
        let call_ids = content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } => Some(id),
            _ => None,
        });
        if let Some(saved) = &saved_projection {
            if !saved.matches_calls(call_ids.clone()) {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
        } else if (checkpoint_projection_required || (!legacy_projection && !strict_projection))
            && call_ids.clone().next().is_some()
        {
            return Err(SessionRuntimeError::ToolProjectionUnavailable { run: run_id });
        }
        let result_message_index = context.len() + 1;
        let mut result_index = 0;
        let mut legacy_output =
            legacy_projection.then(crate::tools::output::LegacyTurnOutputBudget::new);
        let mut turn_output =
            crate::tools::TurnOutputBudget::for_results(content.iter().filter_map(|block| {
                match block {
                    ContentBlock::ToolCall { id, name, .. } => Some((
                        name.as_str(),
                        recorded_turn
                            .get(id)
                            .map_or(INTERRUPTED_TOOL_RESULT, |result| result.content.as_str()),
                    )),
                    _ => None,
                }
            }));
        let mut projected_replies = saved_projection
            .as_mut()
            .map(|saved| std::mem::take(&mut saved.results).into_iter());
        let results = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, .. } => {
                    let block_index = result_index;
                    result_index += 1;
                    if let Some(projected) = &mut projected_replies {
                        let reply = projected
                            .next()
                            .expect("ordered projected IDs were validated");
                        let stored = recorded_turn
                            .remove(id)
                            .ok_or(SessionRuntimeError::CONSTRAINT);
                        match stored {
                            Ok(stored) => {
                                if let Some(effect) = stored.effect {
                                    effects.insert((result_message_index, block_index), effect);
                                }
                            }
                            Err(_) => return Some(Err(SessionRuntimeError::CONSTRAINT)),
                        }
                        return Some(Ok(ContentBlock::ToolResult {
                            call_id: reply.call_id,
                            content: reply.content,
                            is_error: reply.is_error,
                        }));
                    }
                    Some(Ok(match recorded_turn.remove(id) {
                        Some(RecordedResult {
                            mut content,
                            is_error,
                            effect,
                            id: tool_call_id,
                            spill_digest,
                        }) => {
                            if let Some(effect) = effect {
                                effects.insert((result_message_index, block_index), effect);
                            }
                            let spill_handle = spill_digest.as_deref().map(|digest| {
                                crate::tools::output::result_handle(name, tool_call_id, digest)
                            });
                            let recall = match &spill_handle {
                                Some(handle) => crate::tools::ResultRecall::Spill(handle),
                                None => crate::tools::ResultRecall::StoredResult {
                                    tool: name,
                                    call: tool_call_id,
                                },
                            };
                            if let Some(legacy) = &mut legacy_output {
                                legacy.admit(&mut content, recall);
                            } else {
                                turn_output.admit(name, &mut content, recall);
                            }
                            ContentBlock::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error,
                            }
                        }
                        None => {
                            let mut content = INTERRUPTED_TOOL_RESULT.to_owned();
                            if let Some(legacy) = &mut legacy_output {
                                legacy.admit(&mut content, crate::tools::ResultRecall::None);
                            } else {
                                turn_output.admit(
                                    name,
                                    &mut content,
                                    crate::tools::ResultRecall::None,
                                );
                            }
                            ContentBlock::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error: true,
                            }
                        }
                    }))
                }
                ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
        let mut message = Message::new(Role::Assistant, content);
        if let Some(replay) = replay {
            message = message.with_replay(replay.into());
        }
        if truncated && !message.has_content() && results.is_empty() {
            continue;
        }
        context.push(message);
        if !results.is_empty() {
            context.push(Message::tool_results(results));
        }
        if let Some(notice) = saved_projection.and_then(|saved| saved.correction_notice) {
            context.push(Message::user(notice));
        }
        if truncated {
            context.push(Message::user(crate::OUTPUT_TRUNCATED_CONTINUE_NOTICE));
        }
        ends.push((turn_ordinal, context.len()));
    }
    for (_, text) in steering {
        context.push(Message::user(text));
    }
    for (_, text) in delivered {
        context.push(Message::user(text));
    }
    Ok(ends)
}

#[cfg(test)]
mod action_bias_tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    #[test]
    fn ab2_pruning_preserves_long_search_cursor_and_is_idempotent() {
        let path = format!("{}/file.txt", "a".repeat(4_000));
        let cursor = URL_SAFE_NO_PAD.encode(format!("{path}\0{}", 0));
        let header = format!("search . mode=names next={cursor}");
        let page = format!("{header}\n{}\n", "row\n".repeat(2_000));
        let calls = HashMap::from([(
            "search-call".to_owned(),
            ("search".to_owned(), "{}".to_owned()),
        )]);
        let stub =
            prunable_stub(&calls, Some(EffectClass::ReadOnly), "search-call", &page).unwrap();
        assert_eq!(stub.lines().next(), Some(header.as_str()));
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(stub.split("next=").nth(1).unwrap().lines().next().unwrap())
                .unwrap(),
            format!("{path}\0{}", 0).as_bytes()
        );
        assert!(prunable_stub(&calls, Some(EffectClass::ReadOnly), "search-call", &stub).is_none());
    }

    #[test]
    fn ab2_replay_matches_live_projection_for_sixty_four_mixed_replies() {
        let long_name = "x".repeat(128);
        let page = format!(
            "search . mode=names next=YWJjADA\n{}",
            "match\n".repeat(2_030)
        );
        let mut blocks = Vec::new();
        let mut recorded = HashMap::new();
        let mut rows = Vec::new();
        for index in 0..64_u8 {
            let name = if [1, 12, 23, 34, 63].contains(&index) {
                "search"
            } else {
                long_name.as_str()
            };
            let text = if name == "search" {
                page.clone()
            } else {
                "ordinary\n".repeat(4_000)
            };
            let id = format!("call-{index}");
            let call = ToolCallId::from_bytes([index + 1; 16]);
            blocks.push(super::super::codec::PersistedContentBlock::ToolCall {
                id: id.clone(),
                name: name.to_owned(),
                arguments: serde_json::json!({}),
            });
            recorded.insert(
                id.clone(),
                RecordedResult {
                    content: text.clone(),
                    is_error: false,
                    effect: None,
                    id: call,
                    spill_digest: None,
                },
            );
            rows.push((id, name.to_owned(), text, call));
        }
        let mut budget = crate::tools::TurnOutputBudget::for_results(
            rows.iter()
                .map(|(_, name, text, _)| (name.as_str(), text.as_str())),
        );
        let expected = rows
            .iter()
            .map(|(id, name, text, call)| {
                let mut content = text.clone();
                budget.admit(
                    name,
                    &mut content,
                    crate::tools::ResultRecall::StoredResult {
                        tool: name,
                        call: *call,
                    },
                );
                ContentBlock::ToolResult {
                    call_id: id.clone(),
                    content,
                    is_error: false,
                }
            })
            .collect::<Vec<_>>();
        let mut context = Vec::new();
        append_run_turns(
            vec![StoredTurn {
                strict_projection: true,
                checkpoint_projection_required: false,
                projection_json: None,
                run_id: "fixture".to_owned(),
                legacy_projection: false,
                ordinal: 1,
                content_json: serde_json::to_string(&blocks).unwrap(),
                truncated: false,
                notice: None,
                goal_notices: Vec::new(),
            }],
            HashMap::from([(1, recorded)]),
            Default::default(),
            Default::default(),
            None,
            &mut context,
            &mut HashMap::new(),
        )
        .unwrap();
        assert_eq!(context[1].content(), expected);
        let total = expected
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { content, .. } => content.len(),
                _ => unreachable!(),
            })
            .sum::<usize>();
        assert!(total <= crate::tools::output::MAX_TURN_TOOL_OUTPUT_BYTES);
        for index in [1, 12, 23, 34, 63] {
            let ContentBlock::ToolResult { content, .. } = &expected[index] else {
                unreachable!()
            };
            assert_eq!(content, &page);
        }
    }

    #[test]
    fn ab2_replay_preserves_complete_late_search_pages() {
        let path = format!("{}/file.txt", "a".repeat(2_000));
        let cursor = URL_SAFE_NO_PAD.encode(format!("{path}\0{}", 0));
        let page = format!(
            "search . mode=names next={cursor}\n{path}\n{}\n",
            "b".repeat(4_000)
        );
        let mut blocks = Vec::new();
        let mut recorded = HashMap::new();
        for index in 0..4_u8 {
            let id = format!("call-{index}");
            let name = if index == 3 { "search" } else { "read_file" };
            blocks.push(super::super::codec::PersistedContentBlock::ToolCall {
                id: id.clone(),
                name: name.to_owned(),
                arguments: serde_json::json!({}),
            });
            recorded.insert(
                id,
                RecordedResult {
                    content: if index == 3 {
                        page.clone()
                    } else {
                        "x".repeat(32 * 1024)
                    },
                    is_error: false,
                    effect: None,
                    id: ToolCallId::from_bytes([index + 1; 16]),
                    spill_digest: None,
                },
            );
        }
        let turn = StoredTurn {
            strict_projection: true,
            checkpoint_projection_required: false,
            projection_json: None,
            run_id: "fixture".to_owned(),
            legacy_projection: false,
            ordinal: 1,
            content_json: serde_json::to_string(&blocks).unwrap(),
            truncated: false,
            notice: None,
            goal_notices: Vec::new(),
        };
        let mut context = Vec::new();
        append_run_turns(
            vec![turn],
            HashMap::from([(1, recorded)]),
            Default::default(),
            Default::default(),
            None,
            &mut context,
            &mut HashMap::new(),
        )
        .unwrap();
        let ContentBlock::ToolResult { content, .. } = &context[1].content()[3] else {
            panic!("replayed search");
        };
        assert_eq!(content, &page);
        assert!(!content.contains("turn budget reached"));
    }
}
