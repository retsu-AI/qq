//! The streaming write path of a running turn: assistant message rows, text
//! chunks, per-turn commit, activity, reasoning, and truncation notices.

use super::*;

/// Creates the assistant message for one model turn and appends its first
/// text chunk in a single transaction. The message row is created lazily at
/// the turn's first delta — never at turn start — so call-only turns persist
/// no message row at all. The run row's `assistant_message_id` is repointed
/// here: crash recovery interrupts only the still-streaming current message.
pub(super) fn begin_assistant_message(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    message_id: MessageId,
    turn_ordinal: u32,
    channel: TextChannel,
    text: &str,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    if text.is_empty() {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let transaction = store::begin_unit(connection)?;
    reserve_context_capacity(&transaction, identity.run_id, text.len())?;
    let now = now_ms();
    let ordinal: u64 = transaction.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
        [identity.session_id.to_string()],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO messages(
                id, session_id, run_id, ordinal, turn_ordinal, role, state, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'assistant', 'streaming', ?6)",
        params![
            message_id.to_string(),
            identity.session_id.to_string(),
            identity.run_id.to_string(),
            ordinal,
            turn_ordinal,
            now,
        ],
    )?;
    transaction.execute(
        "UPDATE runs SET assistant_message_id = ?2 WHERE id = ?1",
        params![identity.run_id.to_string(), message_id.to_string()],
    )?;
    let message = load_message(&transaction, message_id)?;
    let started = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::AssistantMessageStarted { message },
    )?;
    insert_message_chunk(&transaction, message_id, channel, text)?;
    let appended = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::TextAppended {
            message_id,
            channel,
            text: text.to_owned(),
        },
    )?;
    transaction.commit()?;
    Ok(vec![started, appended])
}

pub(super) fn append_text(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    message_id: MessageId,
    channel: TextChannel,
    text: String,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    if text.is_empty() {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let transaction = store::begin_unit(connection)?;
    let streaming = transaction
        .prepare_cached("SELECT 1 FROM messages WHERE id = ?1 AND state = 'streaming'")
        .and_then(|mut statement| {
            statement
                .query_row([message_id.to_string()], |_| Ok(()))
                .optional()
        })?;
    if streaming.is_none() {
        return Err(SessionRuntimeError::Unavailable);
    }
    reserve_context_capacity(&transaction, identity.run_id, text.len())?;
    insert_message_chunk(&transaction, message_id, channel, &text)?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        SessionEvent::TextAppended {
            message_id,
            channel,
            text,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

pub(super) fn insert_message_chunk(
    transaction: &Connection,
    message_id: MessageId,
    channel: TextChannel,
    text: &str,
) -> Result<(), SessionRuntimeError> {
    let channel = match channel {
        TextChannel::Output => "output",
        TextChannel::Refusal => "refusal",
    };
    transaction
        .prepare_cached(
            "INSERT INTO message_chunks(message_id, channel, chunk_ordinal, text)
             SELECT ?1, ?2, COALESCE(MAX(chunk_ordinal), 0) + 1, ?3
             FROM message_chunks WHERE message_id = ?1 AND channel = ?2",
        )
        .and_then(|mut statement| {
            statement.execute(params![message_id.to_string(), channel, text])
        })?;
    Ok(())
}

pub(super) fn persist_model_turn(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    turn: &ModelTurnCommit,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let ModelTurnCommit {
        turn_ordinal,
        message,
        calls,
        turn_message,
        context_tokens,
        occupancy_basis,
        usage,
        estimated_cost_usd_nanos,
        accounting,
        truncated,
    } = turn;
    if message.role() != Role::Assistant {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let content = message
        .content()
        .iter()
        .map(PersistedContentBlock::from)
        .collect::<Vec<_>>();
    let content_json = serde_json::to_string(&content)?;
    let model_json = serde_json::to_string(&claimed.model)?;
    let usage_json = usage
        .map(|usage| serde_json::to_string(&usage))
        .transpose()?;
    let turn_cost = estimated_cost_usd_nanos
        .map(i64::try_from)
        .transpose()
        .map_err(|_| SessionRuntimeError::CODEC)?;
    let now = now_ms();
    let transaction = store::begin_unit(connection)?;
    let persisted_calls = if claimed.identity.kind == RunKind::Prompt {
        calls.as_slice()
    } else {
        &[]
    };
    let full_message_bytes = crate::measure_message(message);
    let already_reserved_text_bytes = if turn_message.is_some() {
        message
            .content()
            .iter()
            .fold(0_u64, |total, block| match block {
                ContentBlock::Text { text } => {
                    total.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX))
                }
                ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. } => total,
            })
    } else {
        0
    };
    let non_text_bytes =
        usize::try_from(full_message_bytes.saturating_sub(already_reserved_text_bytes))
            .map_err(|_| SessionRuntimeError::OutputTooLarge)?;
    reserve_context_capacity(&transaction, claimed.identity.run_id, non_text_bytes)?;
    // Completing the turn's message in the same transaction as the turn row
    // keeps message state and turn persistence atomic: after a crash, a
    // streaming message always identifies exactly the turn that never
    // committed, and recovery interrupts only that message.
    if let Some(message_id) = turn_message {
        let updated = transaction.execute(
            "UPDATE messages SET state = 'complete', truncated = ?3
                 WHERE id = ?1 AND run_id = ?2 AND state = 'streaming'",
            params![
                message_id.to_string(),
                claimed.identity.run_id.to_string(),
                truncated
            ],
        )?;
        if updated != 1 {
            return Err(SessionRuntimeError::Unavailable);
        }
    }
    transaction.execute(
        "INSERT INTO model_turns(
                 run_id, turn_ordinal, assistant_content_json, model_json,
                 usage_json, estimated_cost_usd_nanos, completed_at_ms, truncated
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            claimed.identity.run_id.to_string(),
            turn_ordinal,
            content_json,
            model_json,
            usage_json,
            turn_cost,
            now,
            truncated,
        ],
    )?;
    let mut events = Vec::with_capacity(persisted_calls.len().saturating_add(3));
    events.push(append_event(
        &transaction,
        EventContext::for_run(store_id, claimed.identity, now),
        SessionEvent::ModelTurnCompleted {
            run_id: claimed.identity.run_id,
            turn_ordinal: *turn_ordinal,
            model: claimed.model.clone(),
            usage: *usage,
            estimated_cost_usd_nanos: *estimated_cost_usd_nanos,
        },
    )?);
    for call in persisted_calls {
        transaction.execute(
            "INSERT INTO tool_calls(
                     id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                     arguments_json, state, requested_at_ms, effect
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'requested', ?8, ?9)",
            params![
                call.id.to_string(),
                claimed.identity.run_id.to_string(),
                call.turn_ordinal,
                call.call_ordinal,
                call.provider_call_id,
                call.name,
                call.arguments,
                now,
                call.effect.as_str(),
            ],
        )?;
        let tool_call = load_tool_call(&transaction, call.id)?;
        events.push(append_event(
            &transaction,
            EventContext::for_run(store_id, claimed.identity, now),
            SessionEvent::ToolCallRequested { tool_call },
        )?);
    }
    let usage_json = accounting
        .as_ref()
        .and_then(|accounting| accounting.usage)
        .map(|usage| serde_json::to_string(&usage))
        .transpose()?;
    let estimated_cost_usd_nanos = accounting
        .as_ref()
        .and_then(|accounting| accounting.estimated_cost_usd_nanos)
        .and_then(|cost| i64::try_from(cost).ok());
    let occupancy_basis_json = occupancy_basis
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    // Persist-before-publish like every other event. A missing provider value
    // clears the previous turn's audit value instead of leaving stale data.
    transaction.execute(
        "UPDATE runs
             SET context_tokens = ?2, usage_json = ?3, estimated_cost_usd_nanos = ?4
             WHERE id = ?1",
        params![
            claimed.identity.run_id.to_string(),
            context_tokens,
            usage_json,
            estimated_cost_usd_nanos,
        ],
    )?;
    if let Some(context_tokens) = context_tokens {
        events.push(append_event(
            &transaction,
            EventContext::for_run(store_id, claimed.identity, now),
            SessionEvent::RunContextUpdated {
                run_id: claimed.identity.run_id,
                context_tokens: *context_tokens,
            },
        )?);
    }
    let session_context_updated = if claimed.identity.kind == RunKind::Prompt {
        transaction.execute(
            "UPDATE sessions
                 SET context_tokens = ?2, context_occupancy_json = ?4
                 WHERE id = ?1 AND model IS ?3
                       AND max_output_tokens IS ?5 AND organization IS ?6",
            params![
                claimed.identity.session_id.to_string(),
                context_tokens,
                &claimed.session_model.model,
                occupancy_basis_json,
                claimed.session_model.max_output_tokens,
                &claimed.session_model.organization,
            ],
        )? == 1
    } else {
        false
    };
    if session_context_updated {
        events.push(append_event(
            &transaction,
            EventContext::for_run(store_id, claimed.identity, now),
            SessionEvent::SessionContextUpdated {
                run_id: claimed.identity.run_id,
                context_tokens: *context_tokens,
            },
        )?);
    }
    transaction.commit()?;
    Ok(events)
}

/// Persists the audit record on the run row and publishes
/// `run_audit_completed` in one transaction. A later revision's audit
/// replaces the record; the latest is what `RunSnapshot.audit` shows.
pub(super) fn record_run_audit(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    record: AuditRecord,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let audit_json = serde_json::to_string(&record)?;
    let updated = transaction.execute(
        "UPDATE runs SET audit_json = ?2 WHERE id = ?1 AND status = 'running'",
        params![identity.run_id.to_string(), audit_json],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::CODEC);
    }
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::RunAuditCompleted {
            run_id: identity.run_id,
            audit: record,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

pub(super) fn record_checkpoint_started(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    correlation: String,
    phase: qq_protocol::CheckpointPhase,
    tool_call_id: Option<qq_protocol::ToolCallId>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    // A crash after dispatch must not leave a known total that omits it.
    // A completed receipt restores totals from the live accumulator atomically.
    transaction.execute(
        "UPDATE runs SET usage_json = NULL, estimated_cost_usd_nanos = NULL WHERE id = ?1",
        [identity.run_id.to_string()],
    )?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        SessionEvent::CheckpointStarted {
            run_id: identity.run_id,
            correlation,
            phase,
            tool_call_id,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

pub(super) struct CheckpointRecord {
    pub(super) correlation: String,
    pub(super) phase: qq_protocol::CheckpointPhase,
    pub(super) tool_call_id: Option<qq_protocol::ToolCallId>,
    pub(super) outcome: qq_protocol::CheckpointOutcome,
    pub(super) confidence_basis_points: Option<u16>,
    pub(super) feedback: String,
    pub(super) spend: Option<qq_protocol::CheckpointSpend>,
}

pub(super) fn record_checkpoint(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    review: CheckpointRecord,
    accounting: Option<RunAccounting>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let CheckpointRecord {
        correlation,
        phase,
        tool_call_id,
        outcome,
        confidence_basis_points,
        feedback,
        spend,
    } = review;
    let transaction = store::begin_unit(connection)?;
    if let Some(accounting) = accounting {
        let usage_json = accounting
            .usage
            .map(|usage| serde_json::to_string(&usage))
            .transpose()?;
        transaction.execute(
            "UPDATE runs SET usage_json = ?2, estimated_cost_usd_nanos = ?3 WHERE id = ?1",
            params![
                identity.run_id.to_string(),
                usage_json,
                accounting
                    .estimated_cost_usd_nanos
                    .and_then(|cost| i64::try_from(cost).ok())
            ],
        )?;
    }
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        SessionEvent::CheckpointReviewed {
            run_id: identity.run_id,
            correlation,
            phase,
            tool_call_id,
            outcome,
            confidence_basis_points,
            feedback,
            spend,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

/// Persists the continuation counter and publishes `run_output_truncated`
/// in one transaction. The truncated turn itself was committed by the
/// preceding `persist_model_turn`; the counter is the run's authoritative
/// record of how many continuations it spent.
pub(super) fn record_run_output_truncated(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    turn_ordinal: u32,
    continuation: u16,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let updated = transaction.execute(
        "UPDATE runs SET output_continuations = ?2 WHERE id = ?1 AND status = 'running'",
        params![identity.run_id.to_string(), continuation],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::RunOutputTruncated {
            run_id: identity.run_id,
            turn_ordinal,
            continuation,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

/// retained in the event log for reconnect/replay, but does not alter model
/// context or transcript rows.
pub(super) fn append_run_activity(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    activity: RunActivity,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    // The column and the event commit together, so the summary query reads
    // the same value a replaying client would reconstruct.
    let running = transaction
        .prepare_cached(
            "UPDATE runs SET activity = ?3
             WHERE id = ?1 AND session_id = ?2 AND status = 'running'",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                identity.run_id.to_string(),
                identity.session_id.to_string(),
                run_activity_column(activity),
            ])
        })?;
    if running != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        SessionEvent::RunActivityChanged {
            run_id: identity.run_id,
            activity,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

pub(super) fn append_reasoning(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    reasoning: ReasoningEvent,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let running = transaction
        .query_row(
            "SELECT 1 FROM runs WHERE id = ?1 AND session_id = ?2 AND status = 'running'",
            params![identity.run_id.to_string(), identity.session_id.to_string()],
            |_| Ok(()),
        )
        .optional()?;
    if running.is_none() {
        return Err(SessionRuntimeError::Unavailable);
    }
    let event = match reasoning {
        ReasoningEvent::Started { kind } => SessionEvent::ReasoningStarted {
            run_id: identity.run_id,
            kind,
        },
        ReasoningEvent::Delta { kind, text } => SessionEvent::ReasoningDelta {
            run_id: identity.run_id,
            kind,
            text,
        },
        ReasoningEvent::Completed { kind } => SessionEvent::ReasoningCompleted {
            run_id: identity.run_id,
            kind,
        },
    };
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        event,
    )?;
    transaction.commit()?;
    Ok(event)
}
