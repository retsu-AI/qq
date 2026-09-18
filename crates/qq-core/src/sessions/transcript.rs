//! Model-context assembly from persisted rows: message loading, the pruned
//! assembly, legacy run replay, the runtime notice, and history search.

use super::*;

/// One stored `<attached-file>` block of a prompt, in prompt order. `content`
/// is `None` when the per-session cap reclaimed the bytes.
struct StoredAttachment {
    path: String,
    window: Option<(usize, usize, usize)>,
    content: Option<String>,
}

/// Persists the attachments a prompt embedded so later assembly reproduces
/// the request the model saw. Bytes are stored once per `(session, digest,
/// window)`; past the per-session cap, the oldest blobs of runs that are no
/// longer running lose their content, and their rows remain so reconstruction
/// renders an explicit evicted block.
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
        // `INSERT OR IGNORE` keeps the first copy (and its eviction state):
        // an evicted blob is not resurrected by a later prompt attaching the
        // same bytes, which would make the earlier prompt's rendering depend
        // on the later one.
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
        // A blob is reclaimable once no running run's prompt references it.
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

/// Every stored attachment of the retained prompts (those in the ordinal
/// window), keyed by message id and in prompt order, with the blob bytes
/// joined in. Sessions without attachments pay one indexed lookup that
/// returns nothing; archived prompts behind the cutoff are never read.
fn load_retained_attachments(
    transaction: &Connection,
    session: &str,
    through_ordinal: u64,
    cutoff_ordinal: u64,
) -> Result<HashMap<String, Vec<StoredAttachment>>, SessionRuntimeError> {
    let mut statement = transaction.prepare_cached(
        "SELECT a.message_id, a.path, a.window_start, a.window_end, a.window_total, b.content
         FROM messages m
         JOIN message_attachments a ON a.message_id = m.id
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

/// Assembles the provider messages for one session: the latest compaction
/// summary (when one exists), then the verbatim transcript after its cutoff,
/// with read-only tool results outside the recency window replaced by stubs.
/// The stored rows are never modified — pruning and summarization are
/// properties of assembly alone.
pub(super) fn load_model_context(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<Vec<Message>, SessionRuntimeError> {
    load_model_context_with_rewrite_status(transaction, session_id, through_ordinal)
        .map(|(context, _)| context)
}

/// Assembles model context with a fixed number of session-scoped queries:
/// the prompts (with their run's status and outcome and their assembled
/// text), every committed model turn for those runs, every recorded tool
/// result, every applied steering message, and — only for stores that
/// predate `model_turns` — the legacy assistant rows. Assembly then runs in
/// memory in prompt order, so the cost is proportional to the context and not
/// to the number of messages times the number of tables.
pub(super) fn load_model_context_with_rewrite_status(
    transaction: &Connection,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<(Vec<Message>, bool), SessionRuntimeError> {
    let compaction = latest_compaction(transaction, session_id)?;
    let cutoff_ordinal = compaction
        .as_ref()
        .map_or(0, |compaction| compaction.cutoff_ordinal);
    // SQLite integers are i64; `u64::MAX` means "everything".
    let through_ordinal = through_ordinal.min(u64::try_from(i64::MAX).unwrap_or(u64::MAX));
    let session = session_id.to_string();

    // Prompts in ordinal order, each with its run's status and outcome and its
    // full text: the base `output` plus every streamed chunk in chunk order.
    struct Prompt {
        message_id: String,
        run_id: String,
        text: String,
        input_json: Option<String>,
        status: String,
        outcome_json: Option<String>,
    }
    let mut statement = transaction.prepare_cached(
        "SELECT m.run_id, m.output, r.status, r.outcome_json,
                    (SELECT group_concat(c.text, '') FROM (
                         SELECT text FROM message_chunks
                         WHERE message_id = m.id AND channel = 'output'
                         ORDER BY chunk_ordinal
                     ) c),
                    m.id, m.input_json
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
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut attachments =
        load_retained_attachments(transaction, &session, through_ordinal, cutoff_ordinal)?;

    // Every committed turn of a retained run, grouped by run. The retained
    // runs are exactly those whose prompt the query above selected: the
    // same session, ordinal window, role, and state filter, expressed as a
    // join so the archive behind the compaction cutoff is never read. Old
    // history therefore costs nothing at assembly; recall goes through
    // `search_history`.
    let mut turns: HashMap<String, Vec<(u32, String, bool)>> = HashMap::new();
    let mut statement = transaction.prepare_cached(
        "SELECT t.run_id, t.turn_ordinal, t.assistant_content_json, t.truncated
             FROM messages m
             JOIN runs r ON r.id = m.run_id
             JOIN model_turns t ON t.run_id = r.id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
             ORDER BY t.run_id, t.turn_ordinal",
    )?;
    let rows = statement.query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, bool>(3)?,
        ))
    })?;
    for row in rows {
        let (run_id, ordinal, content, truncated) = row?;
        turns
            .entry(run_id)
            .or_default()
            .push((ordinal, content, truncated));
    }
    drop(statement);

    // Every recorded tool result, keyed by run, turn, and provider call id, with the
    // effect class the call was admitted under (absent for rows written
    // before schema 26).
    let mut results: HashMap<String, RecordedTurnResults> = HashMap::new();
    let mut statement = transaction.prepare_cached(
        "SELECT c.run_id, c.provider_call_id, c.result, c.is_error, c.effect, c.turn_ordinal
             FROM messages m
             JOIN runs r ON r.id = m.run_id
             JOIN tool_calls c ON c.run_id = r.id
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
        ))
    })?;
    for row in rows {
        let (run_id, call_id, content, is_error, effect, turn_ordinal) = row?;
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
                },
            );
    }
    drop(statement);

    // Applied steering, per run, in the order it was applied. Each carries
    // the ordinal of the turn whose request first included it.
    let mut steering: HashMap<String, std::collections::VecDeque<(u32, String)>> = HashMap::new();
    let mut statement = transaction.prepare_cached(
        "SELECT s.run_id, s.turn_ordinal, s.output
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
        ))
    })?;
    for row in rows {
        let (run_id, ordinal, text) = row?;
        steering
            .entry(run_id)
            .or_default()
            .push_back((ordinal, text));
    }
    drop(statement);

    let mut context = Vec::new();
    let mut effects = HashMap::new();
    if let Some(compaction) = compaction {
        context.push(Message::user(format!(
            "{COMPACTION_SUMMARY_PREAMBLE}\n\n{}",
            compaction.summary
        )));
    }
    for prompt in prompts {
        // A prompt whose files were persisted at run start is reassembled
        // from those bytes, exactly as its run first sent it. Prompts that
        // never started (or predate schema 29) keep their placeholder text.
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
        context.push(Message::user(text));
        // Reconstruct each run immediately after its prompt rather than
        // following message-row ordinals. Follow-up prompts can be queued
        // while the prior run is active, so its later committed output still
        // belongs before the follow-up in model context.
        if matches!(
            prompt.status.as_str(),
            "completed" | "cancelled" | "failed" | "interrupted" | "running"
        ) {
            match turns.remove(&prompt.run_id) {
                Some(run_turns) => append_run_turns(
                    run_turns,
                    results.remove(&prompt.run_id).unwrap_or_default(),
                    steering.remove(&prompt.run_id).unwrap_or_default(),
                    &mut context,
                    &mut effects,
                )?,
                None => append_legacy_run_messages(
                    transaction,
                    parse_id(&prompt.run_id)?,
                    &mut context,
                )?,
            }
        }
        if matches!(
            prompt.status.as_str(),
            "cancelled" | "failed" | "interrupted"
        ) {
            let outcome_json = prompt.outcome_json.ok_or(SessionRuntimeError::CODEC)?;
            let outcome: RunOutcome = serde_json::from_str(&outcome_json)?;
            if let Some(notice) = runtime_notice(&outcome) {
                context.push(Message::user(notice));
            }
        }
    }
    let context_rewritten = prune_stale_tool_results(&mut context, &effects);
    Ok((context, context_rewritten))
}

type RecordedTurnResults = HashMap<u32, HashMap<String, RecordedResult>>;

/// One stored tool result as context assembly reads it.
pub(super) struct RecordedResult {
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) effect: Option<EffectClass>,
}

/// Assistant rows from stores that predate `model_turns`: one query per such
/// run, which only legacy sessions ever pay.
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
        RunOutcome::Failed { failure } => format!("The previous run failed: {}", failure.message),
    };
    Some(format!(
        "{RUNTIME_NOTICE_PREAMBLE}\n{status}\n{RUNTIME_NOTICE_GUIDANCE}"
    ))
}

/// Replaces read-only tool results older than the recency window with
/// one-line stubs. A result is prunable when the call was admitted with the
/// `read_only` effect class (`effects` maps assembled message/block positions
/// to their stored effects) — its output is re-derivable on demand; mutating, shell, and
/// external outputs are not. Rows recorded before the effect was stored fall
/// back to the built-in read-only names. The window keeps the last
/// [`CONTEXT_PRUNE_KEEP_TURNS`] model turns (assistant messages) verbatim.
/// `is_error` is preserved so an error result stays an error stub.
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
    // A provider ID is local to one assistant turn, not the session. Retain
    // only that turn's call metadata while visiting its results.
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

/// The stub replacing a prunable read-only result, or `None` when the result
/// must stay verbatim (unknown call, non-read-only tool, or already smaller
/// than the stub would be).
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
    // A result that follows the header convention keeps its header: the
    // counts, hash, and cursor it carries let the model continue without
    // re-running the call.
    let stub = match crate::tools::header_line(name, content) {
        Some(header) => format!(
            "{header}\n[pruned: {name} {arguments} returned {} bytes; call it again if needed]",
            content.len()
        ),
        None => format!(
            "[pruned: {name} {arguments} returned {} bytes; call it again if needed]",
            content.len()
        ),
    };
    (content.len() > stub.len()).then_some(stub)
}

/// The byte weight the assembled context contributes to the session budget:
/// message text, tool-call names and arguments, and (pruned) tool results.
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
        .fold(0_usize, usize::saturating_add)
}

/// Searches the session's complete durable transcript for a case-insensitive
/// literal: each user prompt, then that run's assistant turns and tool
/// results. Compaction markers and assembly-time pruning are deliberately
/// ignored — this is the recall path that makes aggressive compaction safe.
/// Each match yields one bounded excerpt with a citation naming its durable
/// coordinates; at most `limit` matches are returned, in transcript order.
/// The calling run is excluded: its own `search_history` arguments would
/// otherwise match every query.
///
/// The walk runs newest prompt first and stops once it has examined
/// [`HISTORY_SCAN_BUDGET_BYTES`] of transcript, so a rare or absent term
/// costs a bounded amount of store-worker time however long the session is;
/// the result says when the oldest history went unexamined. Every statement
/// is prepared once per search, not once per run.
pub(super) fn search_session_history(
    transaction: &Connection,
    session_id: SessionId,
    calling_run: RunId,
    query: &str,
    limit: usize,
) -> Result<HistorySearch, SessionRuntimeError> {
    let needle = query.to_lowercase();
    // Newest first, so a budget-truncated search has covered the recent
    // spans a follow-up most often needs. Matches are reversed at the end
    // to keep transcript order for the reader.
    let mut statement = transaction.prepare_cached(
        "SELECT id, ordinal, run_id FROM messages
             WHERE session_id = ?1 AND role = 'user' AND run_id != ?2 AND steering = 0
               AND state IN ('complete', 'cancelled', 'failed', 'interrupted')
             ORDER BY ordinal DESC",
    )?;
    let prompts = statement
        .query_map(
            params![session_id.to_string(), calling_run.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut turns_statement = transaction.prepare_cached(
        "SELECT turn_ordinal, assistant_content_json FROM model_turns
             WHERE run_id = ?1 ORDER BY turn_ordinal DESC",
    )?;
    let mut results_statement = transaction.prepare_cached(
        "SELECT name, call_ordinal, result FROM tool_calls
             WHERE run_id = ?1 AND turn_ordinal = ?2 AND result IS NOT NULL
             ORDER BY call_ordinal DESC",
    )?;

    // Per run, hits are collected newest-turn-first and reversed before they
    // join the overall list, so each run's span reads in transcript order
    // once the whole list is reversed.
    let mut newest_first: Vec<HistoryMatch> = Vec::new();
    let mut scanned: usize = 0;
    let mut truncated = false;
    'prompts: for (message_id, ordinal, run_id) in prompts {
        if newest_first.len() >= limit {
            break;
        }
        if scanned >= HISTORY_SCAN_BUDGET_BYTES {
            truncated = true;
            break;
        }
        let mut run_hits: Vec<HistoryMatch> = Vec::new();
        let record = |scanned: &mut usize,
                      run_hits: &mut Vec<HistoryMatch>,
                      citation: String,
                      text: &str| {
            *scanned = scanned.saturating_add(text.len());
            let lowered = text.to_lowercase();
            if let Some(excerpt) = excerpt_around(text, &lowered, &needle) {
                run_hits.push(HistoryMatch { citation, excerpt });
            }
        };
        let turns = turns_statement
            .query_map([&run_id], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (turn_ordinal, content_json) in turns {
            let results = results_statement
                .query_map(params![&run_id, turn_ordinal], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u16>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (name, call_ordinal, result) in results {
                record(
                    &mut scanned,
                    &mut run_hits,
                    format!(
                        "{name} result, user message #{ordinal} turn {turn_ordinal} call {call_ordinal}"
                    ),
                    &result,
                );
            }
            let content = serde_json::from_str::<Vec<PersistedContentBlock>>(&content_json)?;
            for block in content.into_iter().rev() {
                match ContentBlock::from(block) {
                    ContentBlock::Text { text } => record(
                        &mut scanned,
                        &mut run_hits,
                        format!("assistant, user message #{ordinal} turn {turn_ordinal}"),
                        &text,
                    ),
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => record(
                        &mut scanned,
                        &mut run_hits,
                        format!("tool call {name}, user message #{ordinal} turn {turn_ordinal}"),
                        &arguments.to_string(),
                    ),
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            if scanned >= HISTORY_SCAN_BUDGET_BYTES {
                // Finish this prompt's own text so its citation set is
                // whole, then stop.
                let prompt = load_message(transaction, parse_id(&message_id)?)?;
                record(
                    &mut scanned,
                    &mut run_hits,
                    format!("user message #{ordinal}"),
                    &prompt.output,
                );
                run_hits.reverse();
                newest_first.extend(run_hits);
                truncated = true;
                break 'prompts;
            }
        }
        let prompt = load_message(transaction, parse_id(&message_id)?)?;
        record(
            &mut scanned,
            &mut run_hits,
            format!("user message #{ordinal}"),
            &prompt.output,
        );
        run_hits.reverse();
        newest_first.extend(run_hits);
    }
    if newest_first.len() > limit {
        // Keep the newest `limit` hits: the walk collected whole runs past
        // the cap so a run's span is never split mid-way.
        newest_first.truncate(limit);
    }
    newest_first.reverse();
    Ok(HistorySearch {
        matches: newest_first,
        truncated,
    })
}

/// Measures the session's context as the next run would assemble it —
/// summary plus post-cutoff transcript with pruning applied — plus any text
/// still streaming into the current turn's message, which has not joined a
/// committed turn yet but will.
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

/// Replays one run's persisted model turns (assistant content and tool
/// results) into `context`, in turn order.
/// Replays one run's committed turns into `context`: each assistant turn,
/// then exactly one result per `ToolCall` block in block order, with applied
/// steering placed immediately before the turn whose request first carried it
/// and the continuation notice after a truncated turn.
pub(super) fn append_run_turns(
    turns: Vec<(u32, String, bool)>,
    mut recorded: RecordedTurnResults,
    mut steering: std::collections::VecDeque<(u32, String)>,
    context: &mut Vec<Message>,
    effects: &mut HashMap<(usize, usize), EffectClass>,
) -> Result<(), SessionRuntimeError> {
    for (turn_ordinal, content_json, truncated) in turns {
        let mut recorded_turn = recorded.remove(&turn_ordinal).unwrap_or_default();
        while steering
            .front()
            .is_some_and(|(applied_before, _)| *applied_before <= turn_ordinal)
        {
            let (_, text) = steering.pop_front().expect("front was just checked");
            context.push(Message::user(text));
        }
        let content: Vec<ContentBlock> =
            serde_json::from_str::<Vec<PersistedContentBlock>>(&content_json)?
                .into_iter()
                .map(ContentBlock::from)
                .collect();
        // A block without a recorded result (a crash between the turn commit
        // and its tool_calls rows in an older store) gets an explicit
        // interrupted result so replayed context stays provider-valid
        // instead of poisoning the session.
        let result_message_index = context.len() + 1;
        let mut result_index = 0;
        let results = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => {
                    let block_index = result_index;
                    result_index += 1;
                    Some(match recorded_turn.remove(id) {
                        Some(RecordedResult {
                            content,
                            is_error,
                            effect,
                        }) => {
                            if let Some(effect) = effect {
                                effects.insert((result_message_index, block_index), effect);
                            }
                            ContentBlock::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error,
                            }
                        }
                        None => ContentBlock::ToolResult {
                            call_id: id.clone(),
                            content: INTERRUPTED_TOOL_RESULT.to_owned(),
                            is_error: true,
                        },
                    })
                }
                ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>();
        context.push(Message::new(Role::Assistant, content));
        if !results.is_empty() {
            context.push(Message::tool_results(results));
        }
        // A truncated turn was followed in the live run by the continuation
        // notice; replaying it keeps the assembled context identical to the
        // request the model actually saw (and preserves role alternation).
        if truncated {
            context.push(Message::user(crate::OUTPUT_TRUNCATED_CONTINUE_NOTICE));
        }
    }
    // Steering applied for a turn that never committed (the run settled
    // first) still reached the model's request; keep it so the transcript
    // the user saw is the transcript the next run continues from.
    for (_, text) in steering {
        context.push(Message::user(text));
    }
    Ok(())
}
