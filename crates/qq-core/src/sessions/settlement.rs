use super::*;

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
    if run_is_settled(&transaction, claimed.identity.run_id)? {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    settle_abandoned_owned_summary(&transaction, store_id, claimed, &mut events)?;
    supersede_pending_steering(
        &transaction,
        store_id,
        claimed.identity,
        now_ms(),
        &mut events,
    )?;
    events.extend(expect_settled(settle_run(
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

fn settle_abandoned_owned_summary(
    transaction: &Connection,
    store_id: StoreId,
    prompt: &ClaimedRun,
    events: &mut Vec<SessionEventEnvelope>,
) -> Result<Option<RunId>, SessionRuntimeError> {
    if prompt.identity.kind != RunKind::Prompt {
        return Ok(None);
    }
    let mut statement = transaction.prepare_cached(
        "SELECT step.id, step.command_id FROM runs step JOIN runs owner
            ON owner.id = step.auto_compaction_for_run_id
         WHERE owner.id = ?1 AND owner.session_id = ?2 AND owner.kind = 'prompt'
            AND owner.status = 'running' AND owner.outcome_json IS NULL
            AND step.session_id = owner.session_id AND step.kind = 'compaction'
            AND step.status = 'running' AND step.outcome_json IS NULL
         LIMIT 2",
    )?;
    let rows = statement
        .query_map(
            params![
                prompt.identity.run_id.to_string(),
                prompt.identity.session_id.to_string()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if rows.len() > 1 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let Some((run_id, command_id)) = rows.first() else {
        return Ok(None);
    };
    let mut summary = prompt.clone();
    summary.identity.run_id = parse_id(run_id)?;
    summary.identity.command_id = parse_id(command_id)?;
    summary.identity.kind = RunKind::Compaction;
    summary.goal = None;
    events.extend(expect_settled(settle_run(
        transaction,
        store_id,
        &summary,
        RunOutcome::Cancelled,
        None,
        SettlementCause::Recovery,
    )?)?);
    Ok(Some(summary.identity.run_id))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettlementCause {
    Executor,
    Recovery,
}

pub(super) type RunSettled = Option<Vec<SessionEventEnvelope>>;

pub(super) fn expect_settled<T>(settled: Option<T>) -> Result<T, SessionRuntimeError> {
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
    Ok(settled.unwrap_or(true))
}

pub(super) fn settle_run(
    transaction: &Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
    cause: SettlementCause,
) -> Result<Option<Vec<SessionEventEnvelope>>, SessionRuntimeError> {
    let state = transaction
        .query_row(
            "SELECT outcome_json IS NOT NULL, origin_json, goal_request_pending,
                goal_helper_admitted OR goal_request_pending OR EXISTS(
                    SELECT 1 FROM model_turns WHERE run_id=runs.id AND work_disposition='uncertain')
             FROM runs WHERE id = ?1",
            [claimed.identity.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((false, origin_json, goal_request_pending, goal_spend_admitted)) = state else {
        return Ok(None);
    };
    let origin = origin_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
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
    let pending_goal_request = goal_request_pending
        && !accounting
            .as_ref()
            .is_some_and(|accounting| accounting.goal_request_rejected);
    let usage = if pending_goal_request {
        None
    } else {
        accounting.as_ref().and_then(|accounting| accounting.usage)
    };
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
    let final_output = match &outcome {
        RunOutcome::Completed => accounting
            .as_ref()
            .and_then(|accounting| accounting.final_output.clone()),
        RunOutcome::Cancelled
        | RunOutcome::Interrupted
        | RunOutcome::Paused { .. }
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
    let (session_cost, session_saw_turn) = if cause == SettlementCause::Recovery {
        transaction
            .query_row(
                "SELECT estimated_cost_usd_nanos,
                    CASE WHEN goal_id IS NOT NULL OR goal_paid_owner_run_id IS NOT NULL THEN
                        goal_request_pending OR goal_helper_admitted OR EXISTS(
                            SELECT 1 FROM model_turns WHERE run_id=runs.id
                                AND work_disposition IN ('committed','uncertain'))
                    ELSE usage_json IS NULL OR EXISTS(SELECT 1 FROM model_turns WHERE run_id=runs.id)
                    END
                 FROM runs WHERE id=?1 AND (goal_id IS NOT NULL OR goal_paid_owner_run_id IS NOT NULL OR
                    (kind='compaction' AND auto_compaction_for_run_id IS NOT NULL))",
                [claimed.identity.run_id.to_string()],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?
            .unwrap_or((cost, saw_turn || goal_spend_admitted))
    } else {
        if pending_goal_request {
            (None, true)
        } else {
            (cost, saw_turn || goal_spend_admitted)
        }
    };
    let (next_cost, next_cost_known) = if session_saw_turn {
        match session_cost.and_then(|cost| current_cost.checked_add(cost)) {
            Some(cost) if current_cost_known => (cost, true),
            _ => (current_cost, false),
        }
    } else {
        (current_cost, current_cost_known)
    };
    transaction.execute(
        "UPDATE sessions SET estimated_cost_usd_nanos = ?2, cost_known = ?3 WHERE id = ?1",
        params![
            claimed.identity.session_id.to_string(),
            next_cost,
            next_cost_known
        ],
    )?;
    let preserve_run_accounting = cause == SettlementCause::Recovery || pending_goal_request;
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
    if accounting
        .as_ref()
        .is_some_and(|accounting| accounting.goal_request_rejected)
    {
        transaction.execute(
            "UPDATE runs SET goal_request_pending=0 WHERE id=?1 AND (goal_id IS NOT NULL OR goal_paid_owner_run_id IS NOT NULL OR goal_helper_admitted=1)",
            [claimed.identity.run_id.to_string()],
        )?;
    }
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
                  context_tokens = CASE
                      WHEN ?4 AND ?5 AND model IS ?7
                      THEN ?6
                      ELSE context_tokens
                  END,
                  pending_context_overflow_basis_json = COALESCE(
                      ?8, pending_context_overflow_basis_json
                  ),
                  updated_at_ms = ?2
             WHERE id = ?1 AND active_run_id = ?3",
        params![
            claimed.identity.session_id.to_string(),
            now,
            claimed.identity.run_id.to_string(),
            claimed.identity.kind == RunKind::Prompt,
            saw_turn,
            reported_context_tokens,
            &claimed.model.model,
            pending_context_overflow_basis,
        ],
    )?;
    deliveries::deliver_settled_children(
        transaction,
        claimed.identity.run_id,
        None,
        usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
        now,
    )?;
    deliveries::deliver_to_settled_parent(transaction, claimed.identity.run_id, now)?;
    let summary = load_session_summary(transaction, claimed.identity.session_id)?;
    let context = EventContext::for_run(store_id, claimed.identity, now);
    let context = match cause {
        SettlementCause::Executor => context,
        SettlementCause::Recovery => context.uncaused(),
    };
    let mut events = Vec::new();
    for goal in goals::finalize_spend(
        transaction,
        claimed.identity.session_id,
        claimed.identity.run_id,
    )? {
        let goal_context = if goal.session_id == claimed.identity.session_id {
            context
        } else {
            EventContext::for_session(
                store_id,
                claimed.identity.workspace_id,
                goal.session_id,
                caused_by,
                now,
            )
        };
        events.push(append_event(
            transaction,
            goal_context,
            SessionEvent::GoalUpdated {
                goal: Box::new(goal),
            },
        )?);
    }
    events.push(append_event(
        transaction,
        context,
        SessionEvent::RunFinished {
            origin,
            session: Box::new(summary),
            run_id: claimed.identity.run_id,
            outcome,
            usage,
            context_tokens,
            final_output,
        },
    )?);
    Ok(Some(events))
}

pub(super) fn finish_queued_run(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    now: u64,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
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
    deliveries::deliver_to_settled_parent(transaction, run_id, now)?;
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
    let (usage_json, origin_json): (Option<String>, Option<String>) = transaction.query_row(
        "SELECT usage_json, origin_json FROM runs WHERE id = ?1",
        [run_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let origin = origin_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let usage = usage_json
        .map(|encoded| serde_json::from_str(&encoded))
        .transpose()?;
    let summary = load_session_summary(transaction, session_id)?;
    let context = EventContext::for_run_ids(store_id, workspace_id, session_id, run_id, None, now);
    let mut events = Vec::new();
    if let Some(qq_protocol::RunOrigin::GoalCheck { goal_id, .. }) = &origin
        && let Some(mut goal) = goals::load_live(transaction, session_id)?
        && goal.goal_id == *goal_id
        && goal.pending_run_id == Some(run_id)
    {
        goal.pending_run_id = None;
        goals::save(transaction, &goal)?;
        events.push(append_event(
            transaction,
            context,
            SessionEvent::GoalUpdated {
                goal: Box::new(goal),
            },
        )?);
    }
    for goal in goals::finalize_spend(transaction, session_id, run_id)? {
        let goal_context = if goal.session_id == session_id {
            context
        } else {
            EventContext::for_session(store_id, workspace_id, goal.session_id, None, now)
        };
        events.push(append_event(
            transaction,
            goal_context,
            SessionEvent::GoalUpdated {
                goal: Box::new(goal),
            },
        )?);
    }
    events.push(append_event(
        transaction,
        context,
        SessionEvent::RunFinished {
            origin,
            session: Box::new(summary),
            run_id,
            outcome,
            usage,
            context_tokens: None,
            final_output: None,
        },
    )?);
    Ok(Some(events))
}

pub(super) fn finish_reserved_run(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    outcome: RunOutcome,
    goal: Option<&qq_protocol::GoalSnapshot>,
    goal_initial_notice: Option<&str>,
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
    if goal.is_some() {
        goals::freeze_goal_claim(&transaction, identity, goal, goal_initial_notice)?;
    }
    let mut events = expect_settled(finish_queued_run_with_outcome(
        &transaction,
        store_id,
        identity.workspace_id,
        identity.session_id,
        identity.run_id,
        outcome,
        now_ms(),
    )?)?;
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
    let mut events = expect_settled(finish_queued_run_with_outcome(
        &transaction,
        store_id,
        identity.workspace_id,
        identity.session_id,
        identity.run_id,
        outcome,
        now_ms(),
    )?)?;
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
    let mut run_ids = vec![original.identity.run_id];
    if let Some(summary) =
        settle_abandoned_owned_summary(&transaction, store_id, original, &mut events)?
    {
        run_ids.push(summary);
    }
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
                goal_spend_owner: None,
                goal: None,
                goal_initial_notice: None,
                goal_admission: None,
                check_deadline: None,
                origin: None,
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
                message_effects: Vec::new(),
                history_admission_failure: None,
                context_compaction_attempted: original.context_compaction_attempted,
                context_compaction_failed: false,
                context_compaction_remaining: false,
                compaction_cutoff_ordinal: None,
                context_compaction_oversized_unit_bytes: None,
                in_run_turn_cutoff: None,
                context_overflow_basis: None,
                context_occupancy: None,
                limits: RunLimits::default(),
                input: Vec::new(),
                resolved_input: None,
                profile: original.profile.clone(),
                reasoning_effort: original.reasoning_effort,
                approval_mode: original.approval_mode,
                depth: original.depth,
                root_run_id: original.root_run_id,
                cancel_requested: false,
                file_state: Vec::new(),
                pending_steering: Vec::new(),
                output: None,
                purpose: original.purpose,
            };
            events.extend(expect_settled(settle_run(
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
        events.extend(expect_settled(finish_queued_run_with_outcome(
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

pub(super) fn cancel_owned_child_runs(
    transaction: &Connection,
    store_id: StoreId,
    owner_run_id: RunId,
    command_id: CommandId,
    now: u64,
) -> Result<OwnedChildCancellations, SessionRuntimeError> {
    let mut statement = transaction.prepare(
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
                    .last()
                    .ok_or(SessionRuntimeError::CONSTRAINT)?
                    .cursor,
            );
            settled_queued = true;
            if preparing {
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

pub(super) fn cascade_in_run_compaction_cancel(
    transaction: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    prompt_run: RunId,
    command_id: CommandId,
    now: u64,
) -> Result<Option<(RunId, SessionEventEnvelope)>, SessionRuntimeError> {
    let compaction_run = transaction
        .query_row(
            "SELECT id FROM runs
             WHERE auto_compaction_for_run_id = ?1 AND kind = 'compaction'
               AND status = 'running' AND outcome_json IS NULL AND cancel_requested = 0
             ORDER BY rowid DESC LIMIT 1",
            [prompt_run.to_string()],
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

    let mut statement = transaction.prepare(
        "SELECT r.id, r.session_id, s.workspace_id FROM runs r
         JOIN sessions s ON s.id = r.session_id
         WHERE r.status = 'queued' AND r.outcome_json IS NULL AND r.routing_json IS NOT NULL",
    )?;
    let routed = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut cursors = super::goal_check::recover(&transaction, store_id)?;
    cursors.reserve(routed.len());
    for (run, session, workspace) in routed {
        if let Some(event) = finish_queued_run_with_outcome(
            &transaction,
            store_id,
            parse_id(&workspace)?,
            parse_id(&session)?,
            parse_id(&run)?,
            RunOutcome::Interrupted,
            now_ms(),
        )? {
            cursors.extend(event.into_iter().map(|event| event.cursor));
        }
    }
    transaction.execute(
        "UPDATE sessions SET preparing_run_id = NULL
             WHERE preparing_run_id IS NOT NULL",
        [],
    )?;
    let mut statement = transaction.prepare(
        "SELECT child_run.id, child.id, child.workspace_id
             FROM runs child_run
             JOIN sessions child ON child.id = child_run.session_id
             JOIN runs owner ON owner.id = child.owner_run_id
             WHERE child_run.status = 'queued'
               AND owner.status IN ('running', 'completed', 'cancelled', 'failed', 'interrupted',
                                    'budget_exhausted', 'paused')
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
    cursors.reserve(abandoned_children.len());
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
        cursors.extend(event.into_iter().map(|event| event.cursor));
    }
    let mut statement = transaction.prepare(
        "SELECT r.id, r.session_id, s.workspace_id, r.kind
             FROM runs r JOIN sessions s ON s.id = r.session_id
             WHERE r.status = 'running'",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    cursors.reserve(rows.len());
    for (run, session, workspace, kind) in rows {
        let run_id = parse_id(&run)?;
        let session_id = parse_id(&session)?;
        let workspace_id = parse_id(&workspace)?;
        let events = recover_running_run(
            &transaction,
            store_id,
            RunIdentity {
                workspace_id,
                session_id,
                run_id,
                command_id: CommandId::from_bytes([0; 16]),
                kind: if kind == "goal_check" {
                    RunKind::GoalCheck
                } else {
                    RunKind::Prompt
                },
                child: false,
            },
        )?;
        cursors.extend(events.into_iter().map(|event| event.cursor));
    }
    deliveries::deliver_orphaned_answers(&transaction, now_ms())?;
    transaction.commit()?;
    Ok(cursors)
}

pub(super) fn recovery_claim(identity: RunIdentity) -> ClaimedRun {
    ClaimedRun {
        goal_spend_owner: None,
        goal: None,
        goal_initial_notice: None,
        goal_admission: None,
        check_deadline: None,
        origin: None,
        identity,
        workspace: String::new(),
        user_initiated: false,
        literal_slash: false,
        session_model: ModelSelection::default(),
        model: ModelSelection::default(),
        messages: Vec::new(),
        message_effects: Vec::new(),
        history_admission_failure: None,
        context_compaction_attempted: 0,
        context_compaction_failed: false,
        context_compaction_remaining: false,
        compaction_cutoff_ordinal: None,
        context_compaction_oversized_unit_bytes: None,
        in_run_turn_cutoff: None,
        context_overflow_basis: None,
        context_occupancy: None,
        limits: RunLimits::default(),
        input: Vec::new(),
        resolved_input: None,
        profile: AgentProfileId::default(),
        reasoning_effort: None,
        approval_mode: ApprovalMode::default(),
        depth: 0,
        root_run_id: identity.run_id,
        cancel_requested: false,
        file_state: Vec::new(),
        pending_steering: Vec::new(),
        output: None,
        purpose: SessionPurpose::Task,
    }
}

pub(super) fn recover_running_run(
    transaction: &Connection,
    store_id: StoreId,
    identity: RunIdentity,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let claimed = recovery_claim(identity);
    if run_is_settled(transaction, identity.run_id)? {
        return Ok(Vec::new());
    }
    let mut events = if identity.kind == RunKind::GoalCheck {
        super::goal_check::interrupt(transaction, store_id, identity.run_id)?
    } else {
        Vec::new()
    };
    supersede_pending_steering(transaction, store_id, identity, now_ms(), &mut events)?;
    events.extend(
        settle_run(
            transaction,
            store_id,
            &claimed,
            RunOutcome::Interrupted,
            None,
            SettlementCause::Recovery,
        )?
        .unwrap_or_default(),
    );
    Ok(events)
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
        RunOutcome::Paused { .. } => ("paused", "interrupted"),
        RunOutcome::Failed { .. } => ("failed", "failed"),
    }
}
