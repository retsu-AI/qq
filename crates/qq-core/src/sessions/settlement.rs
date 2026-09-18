//! Terminal settlement: `settle_run` behind the `outcome_json IS NULL` guard,
//! the queued/reserved/prepared finishers, panic and recovery settlement, and
//! owned-child cancellation.

use super::*;

/// Records that an interrupting steer aborted `turn_ordinal`: every tool call
/// of the run still open settles as interrupted (the loop already yielded
/// their finished events, so this is the durable half), then the event.
pub(super) fn record_run_interrupted(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    turn_ordinal: u32,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let mut events = Vec::new();
    let mut statement = transaction.prepare(
        "SELECT id FROM tool_calls
             WHERE run_id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')
             ORDER BY turn_ordinal, call_ordinal",
    )?;
    let ids = statement
        .query_map([identity.run_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        let id = parse_id::<ToolCallId>(&id)?;
        transaction.execute(
            "UPDATE tool_calls
                 SET state = 'interrupted', result = ?2, is_error = 1, finished_at_ms = ?3
                 WHERE id = ?1",
            params![id.to_string(), INTERRUPTED_TOOL_RESULT, now],
        )?;
        let tool_call = load_tool_call(&transaction, id)?;
        events.push(append_event(
            &transaction,
            EventContext::for_run(store_id, identity, now),
            SessionEvent::ToolCallFinished { tool_call },
        )?);
    }
    events.push(append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::RunInterrupted {
            run_id: identity.run_id,
            turn_ordinal,
        },
    )?);
    transaction.commit()?;
    Ok(events)
}

pub(super) fn append_parent_session_update(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    child_session_id: SessionId,
    caused_by: CommandId,
    occurred_at_ms: u64,
    events: &mut Vec<SessionEventEnvelope>,
) -> Result<(), SessionRuntimeError> {
    let Some(parent_id) = session_parent(transaction, child_session_id)? else {
        return Ok(());
    };
    let session = load_session_summary(transaction, parent_id)?;
    events.push(append_event(
        transaction,
        EventContext::for_session(
            store_id,
            workspace_id,
            parent_id,
            Some(caused_by),
            occurred_at_ms,
        ),
        SessionEvent::SessionUpdated {
            session: Box::new(session),
        },
    )?);
    Ok(())
}

pub(super) fn complete_run(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    // Guard before any write: a replayed settlement must leave the superseded
    // steering rows exactly as the first settlement left them.
    if run_is_settled(&transaction, claimed.identity.run_id)? {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    supersede_pending_steering(
        &transaction,
        store_id,
        claimed.identity,
        now_ms(),
        &mut events,
    )?;
    events.push(expect_settled(settle_run(
        &transaction,
        store_id,
        claimed,
        outcome,
        accounting,
        SettlementCause::Executor,
    )?)?);
    append_parent_session_update(
        &transaction,
        store_id,
        claimed.identity.workspace_id,
        claimed.identity.session_id,
        claimed.identity.command_id,
        now_ms(),
        &mut events,
    )?;
    transaction.commit()?;
    Ok(events)
}

/// Who is settling a started run. The executor that owns the run knows the
/// command that queued it; recovery and panic sweeps settle rows they never
/// claimed and record no cause.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettlementCause {
    Executor,
    Recovery,
}

/// Result of a settlement attempt: the `RunFinished` envelope when this call
/// settled the run, `None` when the run already carried an outcome and
/// nothing was written. A run settles exactly once through any path.
pub(super) type RunSettled = Option<SessionEventEnvelope>;

/// For callers that checked `run_is_settled` earlier in the same transaction:
/// `None` there means the row changed under an open write transaction, which
/// SQLite forbids.
pub(super) fn expect_settled(
    settled: RunSettled,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    settled.ok_or(SessionRuntimeError::CONSTRAINT)
}

pub(super) fn run_is_settled(
    transaction: &Connection,
    run_id: RunId,
) -> Result<bool, SessionRuntimeError> {
    let settled = transaction
        .query_row(
            "SELECT outcome_json IS NOT NULL FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    // A missing row has nothing left to settle either.
    Ok(settled.unwrap_or(true))
}

/// Settles a started run inside an open transaction: outcome, usage and cost
/// accounting, message states, tool-call interruption, session status, and
/// the `RunFinished` event. This is the only path that writes a started run's
/// `outcome_json`; the pre-read guard makes any replay a no-op.
pub(super) fn settle_run(
    transaction: &Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
    cause: SettlementCause,
) -> Result<RunSettled, SessionRuntimeError> {
    let already_settled = run_is_settled(transaction, claimed.identity.run_id)?;
    if already_settled {
        return Ok(None);
    }
    let now = now_ms();
    let caused_by = match cause {
        SettlementCause::Executor => Some(claimed.identity.command_id),
        SettlementCause::Recovery => None,
    };
    let outcome = cancellation_wins(transaction, claimed.identity.run_id, outcome)?;
    interrupt_active_tool_calls(
        transaction,
        store_id,
        claimed.identity,
        &outcome,
        caused_by,
        now,
    )?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json = serde_json::to_string(&outcome)?;
    let usage = accounting.as_ref().and_then(|accounting| accounting.usage);
    let usage_json = usage.as_ref().map(serde_json::to_string).transpose()?;
    let cost = accounting
        .as_ref()
        .and_then(|accounting| accounting.estimated_cost_usd_nanos)
        .and_then(|cost| i64::try_from(cost).ok());
    let reported_context_tokens = accounting
        .as_ref()
        .and_then(|accounting| accounting.context_tokens);
    let saw_turn = accounting
        .as_ref()
        .is_some_and(|accounting| accounting.saw_turn);
    // The verdict is meaningful only for a completed answer: a run that was
    // cancelled, failed, or ran out of budget never produced one to judge.
    let final_output = match &outcome {
        RunOutcome::Completed => accounting
            .as_ref()
            .and_then(|accounting| accounting.final_output.clone()),
        RunOutcome::Cancelled
        | RunOutcome::Interrupted
        | RunOutcome::Failed { .. }
        | RunOutcome::BudgetExhausted { .. } => None,
    };
    let final_output_json = final_output
        .as_deref()
        .map(serde_json::to_string)
        .transpose()?;
    let pending_context_overflow_basis = if claimed.identity.kind == RunKind::Prompt
        && matches!(
            &outcome,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderContextExceeded,
                    ..
                }
            }
        ) {
        accounting
            .as_ref()
            .map(|accounting| serde_json::to_string(&accounting.request_basis))
            .transpose()?
    } else {
        None
    };
    let (current_cost, current_cost_known) = transaction.query_row(
        "SELECT estimated_cost_usd_nanos, cost_known FROM sessions WHERE id = ?1",
        [claimed.identity.session_id.to_string()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
    )?;
    let (next_cost, next_cost_known) = if saw_turn {
        match cost.and_then(|cost| current_cost.checked_add(cost)) {
            Some(cost) if current_cost_known => (cost, true),
            _ => (current_cost, false),
        }
    } else {
        (current_cost, current_cost_known)
    };
    // Terminal accounting owns the final per-turn figure only when a model
    // turn completed. No completed turn preserves an earlier committed value;
    // an unmeasured completed turn explicitly clears it. Recovery never held
    // the accumulator, so the row keeps whatever its last committed turn
    // recorded.
    let preserve_run_accounting = cause == SettlementCause::Recovery;
    transaction.execute(
        "UPDATE runs
             SET status = ?2, outcome_json = ?3, finished_at_ms = ?4,
                 usage_json = CASE WHEN ?9 THEN usage_json ELSE ?5 END,
                 estimated_cost_usd_nanos = CASE
                     WHEN ?9 THEN estimated_cost_usd_nanos ELSE ?6
                 END,
                 context_tokens = CASE WHEN ?8 THEN ?7 ELSE context_tokens END,
                 final_output_json = ?10
             WHERE id = ?1 AND outcome_json IS NULL",
        params![
            claimed.identity.run_id.to_string(),
            run_status,
            outcome_json,
            now,
            usage_json,
            cost,
            reported_context_tokens,
            saw_turn,
            preserve_run_accounting,
            final_output_json,
        ],
    )?;
    let context_tokens = run_context_tokens(transaction, claimed.identity.run_id)?;
    transaction.execute(
        "UPDATE messages SET state = ?2
             WHERE run_id = ?1 AND role = 'assistant' AND state = 'streaming'",
        params![claimed.identity.run_id.to_string(), message_state],
    )?;
    transaction.execute(
        "UPDATE sessions
             SET active_run_id = NULL,
                  status = CASE WHEN queued_prompts > 0 THEN 'queued' ELSE 'idle' END,
                  estimated_cost_usd_nanos = ?4,
                  cost_known = ?5,
                  context_tokens = CASE
                      WHEN ?6 AND ?7 AND model IS ?9
                      THEN ?8
                      ELSE context_tokens
                  END,
                  pending_context_overflow_basis_json = COALESCE(
                      ?10, pending_context_overflow_basis_json
                  ),
                  updated_at_ms = ?2
             WHERE id = ?1 AND active_run_id = ?3",
        params![
            claimed.identity.session_id.to_string(),
            now,
            claimed.identity.run_id.to_string(),
            next_cost,
            next_cost_known,
            claimed.identity.kind == RunKind::Prompt,
            saw_turn,
            reported_context_tokens,
            &claimed.model.model,
            pending_context_overflow_basis,
        ],
    )?;
    let summary = load_session_summary(transaction, claimed.identity.session_id)?;
    let context = EventContext::for_run(store_id, claimed.identity, now);
    let context = match cause {
        SettlementCause::Executor => context,
        SettlementCause::Recovery => context.uncaused(),
    };
    append_event(
        transaction,
        context,
        SessionEvent::RunFinished {
            session: Box::new(summary),
            run_id: claimed.identity.run_id,
            outcome,
            usage,
            context_tokens,
            final_output,
        },
    )
    .map(Some)
}

/// Cancels a run the caller has just read as `queued` in this transaction.
pub(super) fn finish_queued_run(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    now: u64,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    expect_settled(finish_queued_run_with_outcome(
        transaction,
        store_id,
        workspace_id,
        session_id,
        run_id,
        RunOutcome::Cancelled,
        now,
    )?)
}

/// Settles a run that never started. Only a `queued` row without an outcome
/// is written; a started or already-settled row is left untouched.
pub(super) fn finish_queued_run_with_outcome(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    outcome: RunOutcome,
    now: u64,
) -> Result<RunSettled, SessionRuntimeError> {
    let outcome = cancellation_wins(transaction, run_id, outcome)?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json = serde_json::to_string(&outcome)?;
    let settled = transaction.execute(
        "UPDATE runs
             SET status = ?2, outcome_json = ?3, finished_at_ms = ?4
             WHERE id = ?1 AND status = 'queued' AND outcome_json IS NULL",
        params![run_id.to_string(), run_status, outcome_json, now],
    )?;
    if settled != 1 {
        return Ok(None);
    }
    transaction.execute(
        "UPDATE messages SET state = ?2 WHERE run_id = ?1 AND state = 'queued'",
        params![run_id.to_string(), message_state],
    )?;
    transaction.execute(
        "UPDATE sessions
             SET queued_prompts = queued_prompts - 1,
                 preparing_run_id = CASE
                     WHEN preparing_run_id = ?3 THEN NULL
                     ELSE preparing_run_id
                 END,
                 status = CASE
                     WHEN active_run_id IS NOT NULL THEN 'running'
                     WHEN queued_prompts > 1 THEN 'queued'
                     ELSE 'idle'
                 END,
                 updated_at_ms = ?2
             WHERE id = ?1",
        params![session_id.to_string(), now, run_id.to_string()],
    )?;
    let summary = load_session_summary(transaction, session_id)?;
    append_event(
        transaction,
        EventContext::for_run_ids(store_id, workspace_id, session_id, run_id, None, now),
        SessionEvent::RunFinished {
            session: Box::new(summary),
            run_id,
            outcome,
            usage: None,
            // A queued run never reached the model; no context to report.
            context_tokens: None,
            final_output: None,
        },
    )
    .map(Some)
}

pub(super) fn finish_reserved_run(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    outcome: RunOutcome,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let state = transaction
        .query_row(
            "SELECT status, outcome_json FROM runs WHERE id = ?1 AND session_id = ?2",
            params![identity.run_id.to_string(), identity.session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((status, stored_outcome)) = state else {
        return Ok(Vec::new());
    };
    if stored_outcome.is_some() {
        return Ok(Vec::new());
    }
    if status != "queued" {
        return Err(SessionRuntimeError::Unavailable);
    }
    let mut events = vec![expect_settled(finish_queued_run_with_outcome(
        &transaction,
        store_id,
        identity.workspace_id,
        identity.session_id,
        identity.run_id,
        outcome,
        now_ms(),
    )?)?];
    append_parent_session_update(
        &transaction,
        store_id,
        identity.workspace_id,
        identity.session_id,
        identity.command_id,
        now_ms(),
        &mut events,
    )?;
    transaction.commit()?;
    Ok(events)
}

pub(super) fn finish_prepared_run(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    audit: &PreparedRunAudit,
    outcome: RunOutcome,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let prompt_identity = serde_json::to_string(audit.prompt_identity.as_ref())?;
    let resolved_model = serde_json::to_string(audit.resolved_model.as_ref())?;
    let context_base_bytes = prepared_context_bytes(audit.weight)?;
    let transaction = store::begin_unit(connection)?;
    let state = transaction
        .query_row(
            "SELECT status, outcome_json FROM runs WHERE id = ?1 AND session_id = ?2",
            params![identity.run_id.to_string(), identity.session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((status, stored_outcome)) = state else {
        return Ok(Vec::new());
    };
    if stored_outcome.is_some() {
        return Ok(Vec::new());
    }
    if status != "queued" {
        return Err(SessionRuntimeError::Unavailable);
    }
    let recorded = transaction.execute(
        "UPDATE runs
             SET prompt_identity_json = ?3, resolved_model_json = ?4,
                 context_base_bytes = ?5, context_increment_bytes = 0
             WHERE id = ?1 AND session_id = ?2 AND status = 'queued'
               AND outcome_json IS NULL",
        params![
            identity.run_id.to_string(),
            identity.session_id.to_string(),
            prompt_identity,
            resolved_model,
            context_base_bytes,
        ],
    )?;
    if recorded != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let mut events = vec![expect_settled(finish_queued_run_with_outcome(
        &transaction,
        store_id,
        identity.workspace_id,
        identity.session_id,
        identity.run_id,
        outcome,
        now_ms(),
    )?)?];
    append_parent_session_update(
        &transaction,
        store_id,
        identity.workspace_id,
        identity.session_id,
        identity.command_id,
        now_ms(),
        &mut events,
    )?;
    transaction.commit()?;
    Ok(events)
}

pub(super) struct PanickedExecutionSettlement {
    pub(super) events: Vec<SessionEventEnvelope>,
    pub(super) run_ids: Vec<RunId>,
}

/// Settles whichever durable state an execution task owned when it panicked.
/// The task may still be preparing its original queued prompt, may have
/// started that prompt, or may have atomically handed the session to a
/// distinct auto-compaction while retaining the prompt reservation.
pub(super) fn settle_panicked_execution(
    connection: &mut Connection,
    store_id: StoreId,
    original: &ClaimedRun,
    outcome: RunOutcome,
) -> Result<PanickedExecutionSettlement, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let session_state = transaction
        .query_row(
            "SELECT active_run_id, preparing_run_id FROM sessions WHERE id = ?1",
            [original.identity.session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?;
    let Some((active_run, _preparing_run)) = session_state else {
        return Ok(PanickedExecutionSettlement {
            events: Vec::new(),
            run_ids: Vec::new(),
        });
    };
    let original_id = original.identity.run_id.to_string();
    let original_state = transaction
        .query_row(
            "SELECT status, outcome_json
             FROM runs WHERE id = ?1 AND session_id = ?2",
            params![original_id, original.identity.session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let mut events = Vec::with_capacity(3);
    // Cleanup ownership is independent of whether this transaction emits a
    // new terminal event: a concurrent cancel may already have settled the
    // original while its task still owns the in-memory registration.
    let mut run_ids = vec![original.identity.run_id];
    if let Some(active_run) = active_run {
        let active = transaction
            .query_row(
                "SELECT command_id, kind, auto_compaction_for_run_id, status, outcome_json
                 FROM runs WHERE id = ?1 AND session_id = ?2",
                params![active_run, original.identity.session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((command_id, kind, auto_compaction_for_run_id, status, stored_outcome)) = active
            && (active_run == original_id
                || auto_compaction_for_run_id.as_deref() == Some(original_id.as_str()))
            && status == "running"
            && stored_outcome.is_none()
        {
            let active_run_id: RunId = parse_id(&active_run)?;
            let active_claim = ClaimedRun {
                identity: RunIdentity {
                    workspace_id: original.identity.workspace_id,
                    session_id: original.identity.session_id,
                    run_id: active_run_id,
                    command_id: parse_id(&command_id)?,
                    kind: parse_run_kind(&kind)?,
                    child: original.identity.child,
                },
                workspace: String::new(),
                user_initiated: false,
                literal_slash: false,
                session_model: original.session_model.clone(),
                model: original.model.clone(),
                messages: Vec::new(),
                context_compaction_attempted: original.context_compaction_attempted,
                context_compaction_failed: false,
                context_compaction_remaining: false,
                compaction_cutoff_ordinal: None,
                context_compaction_oversized_unit_bytes: None,
                context_overflow_basis: None,
                context_occupancy: None,
                limits: RunLimits::default(),
                input: Vec::new(),
                resolved_input: None,
                profile: original.profile.clone(),
                approval_mode: original.approval_mode,
                depth: original.depth,
                root_run_id: original.root_run_id,
                cancel_requested: false,
                file_state: Vec::new(),
                pending_steering: Vec::new(),
                output: None,
                purpose: original.purpose,
            };
            events.push(expect_settled(settle_run(
                &transaction,
                store_id,
                &active_claim,
                outcome.clone(),
                None,
                SettlementCause::Recovery,
            )?)?);
            if !run_ids.contains(&active_run_id) {
                run_ids.push(active_run_id);
            }
        }
    }
    if let Some((status, stored_outcome)) = original_state
        && status == "queued"
        && stored_outcome.is_none()
    {
        events.push(expect_settled(finish_queued_run_with_outcome(
            &transaction,
            store_id,
            original.identity.workspace_id,
            original.identity.session_id,
            original.identity.run_id,
            outcome,
            now_ms(),
        )?)?);
    }
    if !events.is_empty() {
        append_parent_session_update(
            &transaction,
            store_id,
            original.identity.workspace_id,
            original.identity.session_id,
            original.identity.command_id,
            now_ms(),
            &mut events,
        )?;
    }
    transaction.commit()?;
    Ok(PanickedExecutionSettlement { events, run_ids })
}

pub(super) struct OwnedChildCancellations {
    pub(super) committed_through: Option<EventCursor>,
    pub(super) running: Vec<RunId>,
    pub(super) settled_queued: bool,
}

/// Returns running child work whose durable cancellation still needs its
/// in-memory signal. This is also used when an idempotent parent cancellation
/// is replayed after its first caller disappeared before signalling children.
pub(super) fn owned_running_run_ids(
    connection: &Connection,
    owner_run_id: RunId,
) -> Result<Vec<RunId>, SessionRuntimeError> {
    let mut statement = connection.prepare(
        "WITH RECURSIVE owned(session_id, level) AS (
                 SELECT id, 1 FROM sessions WHERE owner_run_id = ?1
                 UNION ALL
                 SELECT child.id, owned.level + 1
                 FROM sessions child
                 JOIN runs parent_run ON parent_run.id = child.owner_run_id
                 JOIN owned ON owned.session_id = parent_run.session_id
                 WHERE owned.level < ?2
             )
             SELECT r.id
             FROM owned JOIN sessions child ON child.id = owned.session_id
             JOIN runs r ON r.session_id = child.id
             WHERE r.cancel_requested = 1
               AND (
                   r.status = 'running'
                   OR (r.status = 'queued' AND child.preparing_run_id = r.id)
                   OR r.status = 'cancelled'
               )
             ORDER BY r.created_at_ms, r.rowid",
    )?;
    statement
        .query_map(params![owner_run_id.to_string(), MAX_CHILD_DEPTH], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| {
            let run = row?;
            parse_id(&run)
        })
        .collect()
}

pub(super) fn cancellation_signal_run_ids(
    connection: &Connection,
    cancelled_run_id: RunId,
) -> Result<Vec<RunId>, SessionRuntimeError> {
    let mut run_ids = owned_running_run_ids(connection, cancelled_run_id)?;
    let auto_compaction = connection
        .query_row(
            "SELECT active.id
             FROM runs cancelled
             JOIN sessions session ON session.id = cancelled.session_id
             JOIN runs active ON active.id = session.active_run_id
             WHERE cancelled.id = ?1
               AND active.kind = 'compaction' AND active.auto_compaction = 1
               AND active.status = 'running' AND active.cancel_requested = 1",
            [cancelled_run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(auto_compaction) = auto_compaction {
        let run_id = parse_id(&auto_compaction)?;
        if !run_ids.contains(&run_id) {
            run_ids.push(run_id);
        }
    }
    Ok(run_ids)
}

/// Cancels every unfinished run in a session spawned by `owner_run_id`.
/// Running children are returned for their in-memory signal; queued children
/// are settled in this transaction, so either ordering between parent
/// cancellation and atomic child creation has a durable outcome.
pub(super) fn cancel_owned_child_runs(
    transaction: &Connection,
    store_id: StoreId,
    owner_run_id: RunId,
    command_id: CommandId,
    now: u64,
) -> Result<OwnedChildCancellations, SessionRuntimeError> {
    let mut statement = transaction.prepare(
        // The whole subtree: sessions this run owns, sessions their runs
        // own, and so on to the depth ceiling. Cancelling a parent must
        // settle every descendant, not only the first generation.
        "WITH RECURSIVE owned(session_id, level) AS (
                 SELECT id, 1 FROM sessions WHERE owner_run_id = ?1
                 UNION ALL
                 SELECT child.id, owned.level + 1
                 FROM sessions child
                 JOIN runs parent_run ON parent_run.id = child.owner_run_id
                 JOIN owned ON owned.session_id = parent_run.session_id
                 WHERE owned.level < ?2
             )
             SELECT r.id, child.id, child.workspace_id, r.status,
                    COALESCE(child.preparing_run_id = r.id, 0)
             FROM owned JOIN sessions child ON child.id = owned.session_id
             JOIN runs r ON r.session_id = child.id
             WHERE r.status IN ('queued', 'running')
             ORDER BY owned.level, r.created_at_ms, r.rowid",
    )?;
    let owned = statement
        .query_map(params![owner_run_id.to_string(), MAX_CHILD_DEPTH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut committed_through = None;
    let mut running = Vec::new();
    let mut settled_queued = false;
    for (run, session, workspace, status, preparing) in owned {
        let run_id: RunId = parse_id(&run)?;
        let session_id: SessionId = parse_id(&session)?;
        let workspace_id: WorkspaceId = parse_id(&workspace)?;
        transaction.execute(
            "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
            [run_id.to_string()],
        )?;
        let summary = load_session_summary(transaction, session_id)?;
        let requested = append_event(
            transaction,
            EventContext::for_run_ids(
                store_id,
                workspace_id,
                session_id,
                run_id,
                Some(command_id),
                now,
            ),
            SessionEvent::CancellationRequested {
                session: Box::new(summary),
                run_id,
            },
        )?;
        if status == "queued" {
            committed_through = Some(
                finish_queued_run(transaction, store_id, workspace_id, session_id, run_id, now)?
                    .cursor,
            );
            settled_queued = true;
            if preparing {
                // The durable row is terminal, but its loader/runtime
                // preparation may still be holding a permit. Signal that
                // task so parent cancellation and shutdown can quiesce.
                running.push(run_id);
            }
        } else {
            committed_through = Some(requested.cursor);
            running.push(run_id);
        }
    }
    Ok(OwnedChildCancellations {
        committed_through,
        running,
        settled_queued,
    })
}

/// Requests cancellation of the session's active auto-compaction when no
/// queued prompt remains to run after it. Returns the compaction's run id
/// (for the in-memory cancellation signal) and the appended
/// `CancellationRequested` event, or `None` when there is nothing to cascade
/// to: prompts still queued, a manual compaction, an ordinary prompt run, or
/// a cancellation already underway.
pub(super) fn cascade_auto_compaction_cancel(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    command_id: CommandId,
    now: u64,
) -> Result<Option<(RunId, SessionEventEnvelope)>, SessionRuntimeError> {
    let compaction_run = transaction
        .query_row(
            "SELECT r.id FROM sessions s JOIN runs r ON r.id = s.active_run_id
             WHERE s.id = ?1 AND s.queued_prompts = 0
               AND r.kind = 'compaction' AND r.auto_compaction = 1
               AND r.outcome_json IS NULL AND r.cancel_requested = 0",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(compaction_run) = compaction_run else {
        return Ok(None);
    };
    let compaction_run: RunId = parse_id(&compaction_run)?;
    transaction.execute(
        "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
        [compaction_run.to_string()],
    )?;
    let summary = load_session_summary(transaction, session_id)?;
    let event = append_event(
        transaction,
        EventContext::for_run_ids(
            store_id,
            workspace_id,
            session_id,
            compaction_run,
            Some(command_id),
            now,
        ),
        SessionEvent::CancellationRequested {
            session: Box::new(summary),
            run_id: compaction_run,
        },
    )?;
    Ok(Some((compaction_run, event)))
}

pub(super) fn recover_interrupted_runs(
    connection: &mut Connection,
    store_id: StoreId,
) -> Result<Vec<EventCursor>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    // Reservation is process-local work backed by a queued run. A crash may
    // leave the pointer behind before RunStarted; clearing it makes that same
    // queued row eligible again. The per-prompt compaction-attempt marker is
    // deliberately retained: once a compaction start committed, recovery
    // must not spend a second attempt.
    transaction.execute(
        "UPDATE sessions SET preparing_run_id = NULL
             WHERE preparing_run_id IS NOT NULL",
        [],
    )?;
    // A spawned child can be durably queued while its owner was running when
    // the process stopped. Settle it before recovering running rows: once the
    // owner is marked interrupted its session no longer advertises an active
    // run, but the explicit owner id still proves this child has no waiter.
    let mut statement = transaction.prepare(
        "SELECT child_run.id, child.id, child.workspace_id
             FROM runs child_run
             JOIN sessions child ON child.id = child_run.session_id
             JOIN runs owner ON owner.id = child.owner_run_id
             WHERE child_run.status = 'queued'
               AND owner.status IN ('running', 'completed', 'cancelled', 'failed', 'interrupted')
             ORDER BY child_run.created_at_ms, child_run.rowid",
    )?;
    let abandoned_children = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut cursors = Vec::with_capacity(abandoned_children.len());
    let recovery_started_at = now_ms();
    for (run, session, workspace) in abandoned_children {
        let event = finish_queued_run(
            &transaction,
            store_id,
            parse_id(&workspace)?,
            parse_id(&session)?,
            parse_id(&run)?,
            recovery_started_at,
        )?;
        cursors.push(event.cursor);
    }
    let mut statement = transaction.prepare(
        "SELECT r.id, r.session_id, s.workspace_id
             FROM runs r JOIN sessions s ON s.id = r.session_id
             WHERE r.status = 'running'",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    cursors.reserve(rows.len());
    for (run, session, workspace) in rows {
        let run_id = parse_id(&run)?;
        let session_id = parse_id(&session)?;
        let workspace_id = parse_id(&workspace)?;
        let claimed = ClaimedRun {
            identity: RunIdentity {
                workspace_id,
                session_id,
                run_id,
                command_id: CommandId::from_bytes([0; 16]),
                kind: RunKind::Prompt,
                child: false,
            },
            workspace: String::new(),
            user_initiated: false,
            literal_slash: false,
            session_model: ModelSelection::default(),
            model: ModelSelection::default(),
            messages: Vec::new(),
            context_compaction_attempted: 0,
            context_compaction_failed: false,
            context_compaction_remaining: false,
            compaction_cutoff_ordinal: None,
            context_compaction_oversized_unit_bytes: None,
            context_overflow_basis: None,
            context_occupancy: None,
            limits: RunLimits::default(),
            input: Vec::new(),
            resolved_input: None,
            profile: AgentProfileId::default(),
            approval_mode: ApprovalMode::default(),
            depth: 0,
            root_run_id: run_id,
            cancel_requested: false,
            file_state: Vec::new(),
            pending_steering: Vec::new(),
            output: None,
            purpose: SessionPurpose::Task,
        };
        let event = expect_settled(settle_run(
            &transaction,
            store_id,
            &claimed,
            RunOutcome::Interrupted,
            None,
            SettlementCause::Recovery,
        )?)?;
        cursors.push(event.cursor);
    }
    transaction.commit()?;
    Ok(cursors)
}

pub(super) fn cancellation_wins(
    transaction: &Connection,
    run_id: RunId,
    outcome: RunOutcome,
) -> Result<RunOutcome, SessionRuntimeError> {
    if matches!(outcome, RunOutcome::Cancelled) {
        return Ok(outcome);
    }
    let requested = transaction.query_row(
        "SELECT cancel_requested FROM runs WHERE id = ?1",
        [run_id.to_string()],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(if requested {
        RunOutcome::Cancelled
    } else {
        outcome
    })
}

pub(super) fn outcome_states(outcome: &RunOutcome) -> (&'static str, &'static str) {
    match outcome {
        RunOutcome::Completed => ("completed", "complete"),
        RunOutcome::Cancelled => ("cancelled", "cancelled"),
        RunOutcome::Interrupted => ("interrupted", "interrupted"),
        RunOutcome::BudgetExhausted { .. } => ("budget_exhausted", "interrupted"),
        RunOutcome::Failed { .. } => ("failed", "failed"),
    }
}
