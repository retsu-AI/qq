mod inputs;
mod record;

#[cfg(test)]
pub(super) use inputs::EXACT_INPUTS_HEADER;
pub(super) use inputs::with_exact_inputs;

pub(super) use record::{RecordScope, record_budget, render_compaction_record};

use super::*;

pub(super) fn narrative_of(mut summary: String) -> String {
    let at = [COMPACTION_RECORD_HEADER, inputs::EXACT_INPUTS_HEADER]
        .into_iter()
        .flat_map(|header| summary.match_indices(header).map(|(at, _)| at))
        .filter(|&at| at == 0 || summary[..at].ends_with('\n'))
        .min();
    if let Some(at) = at {
        summary.truncate(at);
        let kept = summary.trim_end().len();
        summary.truncate(kept);
    }
    summary
}

fn with_record(mut narrative: String, record: &str) -> String {
    if !record.is_empty() {
        narrative.reserve(record.len() + 2);
        narrative.push_str("\n\n");
        narrative.push_str(record);
    }
    narrative
}

pub(super) fn validate_compaction_summary(summary: &str) -> Result<(), String> {
    if summary.trim().is_empty() {
        return Err("compaction produced an empty summary".to_owned());
    }
    let replacement_bytes = COMPACTION_SUMMARY_PREAMBLE
        .len()
        .saturating_add(2)
        .saturating_add(summary.len())
        .saturating_add(2)
        .saturating_add(context::COMPACTION_RECORD_BYTES);
    if replacement_bytes > MAX_CONTEXT_BYTES {
        return Err("compaction summary exceeds the 4 MiB session context limit".to_owned());
    }
    let missing = COMPACTION_REQUIRED_SECTIONS
        .iter()
        .filter(|section| {
            !summary.lines().any(|line| {
                let line = line
                    .trim_start()
                    .trim_start_matches(|c: char| {
                        c.is_ascii_digit() || matches!(c, '.' | ')' | '#' | '*' | '-' | ' ')
                    })
                    .trim_start_matches(['*', '_']);
                line.get(..section.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(section))
                    && {
                        let rest = line[section.len()..]
                            .trim_matches(|c: char| matches!(c, '*' | '_' | '#' | ' ' | '\t'));
                        rest.is_empty() || rest.starts_with(':')
                    }
            })
        })
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "compaction summary is missing required sections: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

pub(super) fn start_auto_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    original: &ClaimedRun,
    audit: &PreparedRunAudit,
    cutoff_ordinal: Option<u64>,
) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
    let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let user_message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let assistant_message_id =
        MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let prompt_identity = serde_json::to_string(audit.prompt_identity.as_ref())?;
    let resolved_model = serde_json::to_string(audit.resolved_model.as_ref())?;
    let plan_identity = serde_json::to_string(&audit.plan_identity)?;
    let context_base_bytes = prepared_context_bytes(audit.weight)?;
    let now = now_ms();
    let transaction = store::begin_unit(connection)?;
    let attempted = transaction.execute(
        "UPDATE runs SET context_compaction_attempted = context_compaction_attempted + 1
             WHERE id = ?1 AND session_id = ?2 AND status = 'queued'
               AND outcome_json IS NULL AND cancel_requested = 0
               AND context_compaction_attempted < ?3",
        params![
            original.identity.run_id.to_string(),
            original.identity.session_id.to_string(),
            context::MAX_COMPACTION_STEPS,
        ],
    )?;
    if attempted != 1 {
        return Ok(None);
    }
    let reservation_valid: bool = transaction.query_row(
        "SELECT active_run_id IS NULL AND preparing_run_id = ?2
             FROM sessions WHERE id = ?1",
        params![
            original.identity.session_id.to_string(),
            original.identity.run_id.to_string()
        ],
        |row| row.get(0),
    )?;
    if !reservation_valid {
        return Ok(None);
    }
    transaction.execute(
        "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, kind, auto_compaction, auto_compaction_for_run_id,
                 prompt_identity_json,
                 resolved_model_json, context_base_bytes, context_increment_bytes,
                 created_at_ms, started_at_ms, plan_identity_json, plan_descriptor_json,
                 activity, origin_json
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, ?6, ?7,
                 ?8, ?9, 0, ?10, ?10, ?11, ?12, 'compacting', ?13
             )",
        params![
            run_id.to_string(),
            original.identity.session_id.to_string(),
            command_id.to_string(),
            user_message_id.to_string(),
            assistant_message_id.to_string(),
            original.identity.run_id.to_string(),
            prompt_identity,
            resolved_model,
            context_base_bytes,
            now,
            plan_identity,
            audit.plan_descriptor_json.as_ref(),
            serde_json::to_string(&qq_protocol::RunOrigin::AutoCompaction {
                owner_run_id: original.identity.run_id
            })?,
        ],
    )?;
    let session_started = transaction.execute(
        "UPDATE sessions SET active_run_id = ?2, status = 'running', updated_at_ms = ?3
             WHERE id = ?1 AND active_run_id IS NULL AND preparing_run_id = ?4",
        params![
            original.identity.session_id.to_string(),
            run_id.to_string(),
            now,
            original.identity.run_id.to_string(),
        ],
    )?;
    if session_started != 1 {
        return Ok(None);
    }
    goals::freeze_goal_claim(
        &transaction,
        original.identity,
        original.goal.as_deref(),
        original.goal_initial_notice.as_deref(),
    )?;
    if original.goal.is_some() || original.goal_spend_owner.is_some() {
        transaction.execute(
            "UPDATE runs SET usage_json=?2, estimated_cost_usd_nanos=0 WHERE id=?1",
            params![
                run_id.to_string(),
                serde_json::to_string(&TokenUsage::default())?
            ],
        )?;
    }
    let summary = load_session_summary(&transaction, original.identity.session_id)?;
    let started = append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            original.identity.workspace_id,
            original.identity.session_id,
            run_id,
            None,
            now,
        ),
        SessionEvent::RunStarted {
            origin: Some(qq_protocol::RunOrigin::AutoCompaction {
                owner_run_id: original.identity.run_id,
            }),
            session: Box::new(summary),
            run_id,
            plan: Some(Box::new(audit.plan_identity.clone())),
        },
    )?;
    append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            original.identity.workspace_id,
            original.identity.session_id,
            run_id,
            None,
            now,
        ),
        SessionEvent::RunActivityChanged {
            run_id,
            activity: RunActivity::Compacting,
        },
    )?;
    transaction.commit()?;
    Ok(Some((
        ClaimedRun {
            goal_spend_owner: original
                .goal_spend_owner
                .or_else(|| original.goal.as_ref().map(|goal| goal.goal_id)),
            goal: None,
            goal_initial_notice: None,
            goal_admission: None,
            check_deadline: None,
            origin: Some(qq_protocol::RunOrigin::AutoCompaction {
                owner_run_id: original.identity.run_id,
            }),
            identity: RunIdentity {
                workspace_id: original.identity.workspace_id,
                session_id: original.identity.session_id,
                run_id,
                command_id,
                kind: RunKind::Compaction,
                child: original.identity.child,
            },
            workspace: original.workspace.clone(),
            user_initiated: false,
            literal_slash: false,
            session_model: original.session_model.clone(),
            model: original.model.clone(),
            messages: Vec::new(),
            message_effects: Vec::new(),
            history_admission_failure: None,
            context_compaction_attempted: original.context_compaction_attempted.saturating_add(1),
            context_compaction_failed: false,
            context_compaction_remaining: false,
            compaction_cutoff_ordinal: cutoff_ordinal,
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
            purpose: original.purpose,
            cancel_requested: false,
            file_state: Vec::new(),
            pending_steering: Vec::new(),
            output: None,
        },
        started,
    )))
}

pub(super) struct SummarizerInput {
    pub(super) messages: Vec<Message>,
    pub(super) cutoff_ordinal: Option<u64>,
    pub(super) oversized_unit_bytes: Option<u64>,
}

pub(super) fn load_summarizer_input(
    connection: &mut Connection,
    session_id: SessionId,
    message_byte_budget: Option<u64>,
) -> Result<SummarizerInput, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (mut messages, _, units) =
        load_model_context_with_units(&transaction, session_id, u64::MAX)?;
    let instruction = Message::user(COMPACTION_INSTRUCTION.to_owned());
    transaction.commit()?;
    let everything_ordinal = units.last().map(|unit| unit.prompt_ordinal);
    let (cutoff_ordinal, oversized_unit_bytes) = match (message_byte_budget, everything_ordinal) {
        (None, everything) | (Some(_), everything @ None) => (everything, None),
        (Some(budget), Some(everything)) => {
            let budget = budget.saturating_sub(crate::measure_message(&instruction));
            let mut spent = 0_u64;
            let mut taken = 0_usize;
            let mut cutoff = everything;
            let mut oversized = None;
            for (index, unit) in units.iter().enumerate() {
                let unit_bytes = crate::measure_messages(&messages[taken..unit.end]);
                let total = spent.saturating_add(unit_bytes);
                if total > budget && index > 0 {
                    break;
                }
                if total > budget {
                    oversized = Some(unit_bytes);
                }
                spent = total;
                taken = unit.end;
                cutoff = unit.prompt_ordinal;
            }
            messages.truncate(taken);
            (Some(cutoff), oversized)
        }
    };
    messages.push(instruction);
    Ok(SummarizerInput {
        messages,
        cutoff_ordinal,
        oversized_unit_bytes,
    })
}

pub(super) fn complete_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    summary: String,
    accounting: Option<RunAccounting>,
    record_budget: usize,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let summary = narrative_of(summary);
    let transaction = store::begin_unit(connection)?;
    if run_is_settled(&transaction, claimed.identity.run_id)? {
        return Ok(Vec::new());
    }
    let mut outcome =
        cancellation_wins(&transaction, claimed.identity.run_id, RunOutcome::Completed)?;
    if matches!(outcome, RunOutcome::Completed)
        && let Err(reason) = validate_compaction_summary(&summary)
    {
        outcome = RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Policy,
                message: reason,
            },
        };
    }
    let mut events = Vec::with_capacity(2);
    if matches!(outcome, RunOutcome::Completed) {
        let now = now_ms();
        let before_bytes = assembled_context_bytes(&transaction, claimed.identity.session_id)?;
        let cutoff_ordinal: u64 = match claimed.compaction_cutoff_ordinal {
            Some(cutoff) => cutoff,
            None => transaction.query_row(
                "SELECT COALESCE(MAX(ordinal), 0) FROM messages
                     WHERE session_id = ?1 AND state IN ('complete', 'interrupted')",
                [claimed.identity.session_id.to_string()],
                |row| row.get(0),
            )?,
        };
        let record = render_compaction_record(
            &transaction,
            claimed.identity.session_id,
            RecordScope::Session { cutoff_ordinal },
            record_budget,
        )?;
        let summary = with_record(summary, &record);
        transaction.execute(
            "INSERT INTO session_compactions(
                     session_id, run_id, summary, cutoff_ordinal,
                     before_bytes, after_bytes, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![
                claimed.identity.session_id.to_string(),
                claimed.identity.run_id.to_string(),
                summary,
                cutoff_ordinal,
                u64::try_from(before_bytes).unwrap_or(u64::MAX),
                now,
            ],
        )?;
        let candidate_bytes =
            match assembled_context_bytes(&transaction, claimed.identity.session_id) {
                Ok(bytes) => Some(bytes),
                Err(SessionRuntimeError::ContextTooLarge) => None,
                Err(error) => return Err(error),
            };
        let shrinkage_required = before_bytes > COMPACTION_SHRINKAGE_FLOOR_BYTES;
        if candidate_bytes.is_none()
            || (shrinkage_required && candidate_bytes.is_some_and(|bytes| bytes >= before_bytes))
        {
            transaction.execute(
                "DELETE FROM session_compactions WHERE session_id = ?1 AND run_id = ?2",
                params![
                    claimed.identity.session_id.to_string(),
                    claimed.identity.run_id.to_string()
                ],
            )?;
            let failed = RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    message: match candidate_bytes {
                        Some(bytes) => format!(
                            "compaction summary did not shrink the assembled context \
                             ({bytes} bytes after, {before_bytes} before); the prior \
                             compaction remains in effect"
                        ),
                        None => "exact inputs and summary exceed the 4 MiB session context limit; \
                                 the prior compaction remains in effect"
                            .to_owned(),
                    },
                },
            };
            events.extend(expect_settled(settle_run(
                &transaction,
                store_id,
                claimed,
                failed,
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
            return Ok(events);
        }
        let after_bytes = candidate_bytes.expect("the rejected candidate returned above");
        transaction.execute(
            "UPDATE session_compactions SET after_bytes = ?3
                 WHERE session_id = ?1 AND run_id = ?2",
            params![
                claimed.identity.session_id.to_string(),
                claimed.identity.run_id.to_string(),
                u64::try_from(after_bytes).unwrap_or(u64::MAX),
            ],
        )?;
        prune_compaction_history(&transaction, claimed.identity.session_id)?;
        transaction.execute(
            "UPDATE sessions
                 SET context_tokens = NULL,
                     context_occupancy_json = NULL,
                     pending_context_overflow_basis_json = NULL
                 WHERE id = ?1",
            [claimed.identity.session_id.to_string()],
        )?;
        events.extend(expect_settled(settle_run(
            &transaction,
            store_id,
            claimed,
            outcome,
            accounting,
            SettlementCause::Executor,
        )?)?);
        let session = load_session_summary(&transaction, claimed.identity.session_id)?;
        events.push(append_event(
            &transaction,
            EventContext::for_run(store_id, claimed.identity, now),
            SessionEvent::SessionCompacted {
                session: Box::new(session),
                summary: Some(truncate_utf8(summary, MAX_EVENT_SUMMARY_BYTES)),
                before_bytes: u64::try_from(before_bytes).unwrap_or(u64::MAX),
                after_bytes: u64::try_from(after_bytes).unwrap_or(u64::MAX),
            },
        )?);
    } else {
        events.extend(expect_settled(settle_run(
            &transaction,
            store_id,
            claimed,
            outcome,
            accounting,
            SettlementCause::Executor,
        )?)?);
    }
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

pub(super) struct CompactionRow {
    pub(super) summary: String,
    pub(super) cutoff_ordinal: u64,
}

pub(super) fn prune_compaction_history(
    connection: &Connection,
    session_id: SessionId,
) -> Result<(), SessionRuntimeError> {
    connection.execute(
        "WITH recent AS MATERIALIZED (
             SELECT rowid AS id FROM session_compactions WHERE session_id = ?1
             ORDER BY rowid DESC LIMIT ?2
         ), baseline AS MATERIALIZED (
             SELECT rowid AS id, cutoff_ordinal FROM session_compactions
             WHERE session_id = ?1 AND scope_run_id IS NULL
               AND rowid < (SELECT MIN(id) FROM recent)
             ORDER BY rowid DESC LIMIT 1
         ), retained AS MATERIALIZED (
             SELECT id FROM recent
             UNION ALL SELECT id FROM baseline
             UNION ALL
             SELECT MAX(c.rowid) FROM session_compactions c
             JOIN runs r ON r.id = c.scope_run_id
             JOIN messages m ON m.id = r.user_message_id
             WHERE c.session_id = ?1 AND r.session_id = ?1
               AND c.rowid < (SELECT MIN(id) FROM recent)
               AND m.ordinal > COALESCE((SELECT cutoff_ordinal FROM baseline), 0)
             GROUP BY c.scope_run_id
         )
         DELETE FROM session_compactions
         WHERE session_id = ?1 AND rowid NOT IN (SELECT id FROM retained)",
        params![session_id.to_string(), COMPACTION_HISTORY_ROWS],
    )?;
    Ok(())
}

pub(super) fn latest_compaction(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Option<CompactionRow>, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT summary, cutoff_ordinal FROM session_compactions
             WHERE session_id = ?1 AND scope_run_id IS NULL
             ORDER BY rowid DESC LIMIT 1",
            [session_id.to_string()],
            |row| {
                Ok(CompactionRow {
                    summary: row.get(0)?,
                    cutoff_ordinal: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::CODEC)
}

pub(super) struct InRunCompaction {
    pub(super) summary: String,
    pub(super) turn_cutoff: u32,
    pub(super) goal_notice: Option<String>,
}

pub(super) fn in_run_compactions(
    connection: &Connection,
    session: &str,
    through_ordinal: u64,
    cutoff_ordinal: u64,
    remaining_notice_bytes: &mut usize,
) -> Result<HashMap<String, InRunCompaction>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT c.scope_run_id, c.summary, c.turn_cutoff, c.goal_notice
         FROM messages m
         JOIN session_compactions c ON c.scope_run_id = m.run_id
         WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
           AND m.role = 'user' AND m.steering = 0
           AND c.rowid = (SELECT MAX(rowid) FROM session_compactions
                          WHERE scope_run_id = m.run_id)",
    )?;
    let mut rows = statement.query(params![session, through_ordinal, cutoff_ordinal])?;
    let mut markers = HashMap::new();
    while let Some(row) = rows.next()? {
        let run_id: String = row.get(0)?;
        let summary: String = row.get(1)?;
        let turn_cutoff: u32 = row.get(2)?;
        let goal_notice = goals::read_notice_with_budget(row, 3, remaining_notice_bytes)?;
        let summary = with_exact_inputs(
            connection,
            session,
            RecordScope::Run {
                run_id: run_id.parse().map_err(|_| SessionRuntimeError::CODEC)?,
                turn_cutoff,
            },
            summary,
        )?;
        *remaining_notice_bytes = remaining_notice_bytes
            .checked_sub(summary.len())
            .ok_or(SessionRuntimeError::ContextTooLarge)?;
        markers.insert(
            run_id,
            InRunCompaction {
                summary,
                turn_cutoff,
                goal_notice,
            },
        );
    }
    Ok(markers)
}

pub(super) fn start_in_run_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    prompt_run: &ClaimedRun,
    resolved_model: &ResolvedModel,
    turn_cutoff: u32,
    inherited_prefix: bool,
) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
    let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let user_message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let assistant_message_id =
        MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let resolved_model_json = serde_json::to_string(resolved_model)?;
    let now = now_ms();
    let transaction = store::begin_unit(connection)?;
    let running: bool = transaction
        .query_row(
            "SELECT status = 'running' AND cancel_requested = 0 FROM runs
                 WHERE id = ?1 AND session_id = ?2",
            params![
                prompt_run.identity.run_id.to_string(),
                prompt_run.identity.session_id.to_string()
            ],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if !running {
        return Ok(None);
    }
    transaction.execute(
        "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, kind, auto_compaction, auto_compaction_for_run_id,
                 resolved_model_json, context_base_bytes, context_increment_bytes,
                 created_at_ms, started_at_ms, activity, usage_json, estimated_cost_usd_nanos, origin_json
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, ?6, ?7, 0, 0, ?8, ?8,
                 'compacting', ?9, 0, ?10
             )",
        params![
            run_id.to_string(),
            prompt_run.identity.session_id.to_string(),
            command_id.to_string(),
            user_message_id.to_string(),
            assistant_message_id.to_string(),
            prompt_run.identity.run_id.to_string(),
            resolved_model_json,
            now,
            serde_json::to_string(&TokenUsage::default())?,
            serde_json::to_string(&qq_protocol::RunOrigin::InRunCompaction { owner_run_id: prompt_run.identity.run_id })?,
        ],
    )?;
    let summary = load_session_summary(&transaction, prompt_run.identity.session_id)?;
    let started = append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            prompt_run.identity.workspace_id,
            prompt_run.identity.session_id,
            run_id,
            None,
            now,
        ),
        SessionEvent::RunStarted {
            origin: Some(qq_protocol::RunOrigin::InRunCompaction {
                owner_run_id: prompt_run.identity.run_id,
            }),
            session: Box::new(summary),
            run_id,
            plan: None,
        },
    )?;
    append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            prompt_run.identity.workspace_id,
            prompt_run.identity.session_id,
            run_id,
            None,
            now,
        ),
        SessionEvent::RunActivityChanged {
            run_id,
            activity: RunActivity::Compacting,
        },
    )?;
    let inherited_cutoff = if inherited_prefix {
        Some(transaction.query_row(
            "SELECT m.ordinal - 1 FROM messages m JOIN runs r ON r.user_message_id=m.id WHERE r.id=?1 AND m.session_id=?2",
            params![prompt_run.identity.run_id.to_string(), prompt_run.identity.session_id.to_string()],
            |row| row.get::<_, u64>(0),
        )?)
    } else {
        None
    };
    let inherited_messages = match inherited_cutoff {
        Some(cutoff) => load_model_context(&transaction, prompt_run.identity.session_id, cutoff)?,
        None => Vec::new(),
    };
    transaction.commit()?;
    let mut claimed = prompt_run.panic_settlement_claim();
    claimed.origin = Some(qq_protocol::RunOrigin::InRunCompaction {
        owner_run_id: prompt_run.identity.run_id,
    });
    claimed.identity.run_id = run_id;
    claimed.identity.command_id = command_id;
    claimed.identity.kind = RunKind::Compaction;
    claimed.goal = None;
    claimed.goal_spend_owner = prompt_run
        .goal_spend_owner
        .or_else(|| prompt_run.goal.as_ref().map(|goal| goal.goal_id));
    claimed.compaction_cutoff_ordinal = inherited_cutoff;
    claimed.messages = inherited_messages;
    claimed.in_run_turn_cutoff = Some((prompt_run.identity.run_id, turn_cutoff));
    Ok(Some((claimed, started)))
}

pub(super) struct InRunSummaryCommit<'a> {
    pub(super) summary: String,
    pub(super) accounting: Option<RunAccounting>,
    pub(super) record_budget: usize,
    pub(super) retained_bytes: u64,
    pub(super) goal_notice: Option<&'a str>,
}

pub(super) fn complete_in_run_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    commit: InRunSummaryCommit<'_>,
) -> Result<(Vec<SessionEventEnvelope>, Option<String>), SessionRuntimeError> {
    let InRunSummaryCommit {
        summary,
        accounting,
        record_budget,
        retained_bytes,
        goal_notice,
    } = commit;
    let Some((scope_run_id, turn_cutoff)) = claimed.in_run_turn_cutoff else {
        return Err(SessionRuntimeError::CONSTRAINT);
    };
    let summary = narrative_of(summary);
    if goal_notice.is_some_and(|text| {
        text.len() > crate::tools::goal::MAX_GOAL_RENDER_BYTES
            || !text.starts_with("[QQ runtime notice; not a user instruction]\n")
    }) {
        return Err(SessionRuntimeError::InvalidGoal(
            goals::GoalValidationError::Notice,
        ));
    }
    let transaction = store::begin_unit(connection)?;
    if run_is_settled(&transaction, claimed.identity.run_id)? {
        return Ok((Vec::new(), None));
    }
    let mut outcome =
        cancellation_wins(&transaction, claimed.identity.run_id, RunOutcome::Completed)?;
    if matches!(outcome, RunOutcome::Completed)
        && let Err(reason) = validate_compaction_summary(&summary)
    {
        outcome = RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Policy,
                message: reason,
            },
        };
    }
    let mut events = Vec::with_capacity(3);
    let mut committed = None;
    if matches!(outcome, RunOutcome::Completed) {
        let now = now_ms();
        let session = claimed.identity.session_id;
        let before_bytes = assembled_context_bytes(&transaction, session)?;
        let scope = match claimed.compaction_cutoff_ordinal {
            Some(cutoff_ordinal) => RecordScope::Session { cutoff_ordinal },
            None => RecordScope::Run {
                run_id: scope_run_id,
                turn_cutoff,
            },
        };
        let record = render_compaction_record(&transaction, session, scope, record_budget)?;
        let summary = with_record(summary, &record);
        let visible_summary =
            match with_exact_inputs(&transaction, &session.to_string(), scope, summary.clone()) {
                Ok(summary) => summary,
                Err(SessionRuntimeError::ContextTooLarge) => {
                    events.extend(expect_settled(settle_run(
                    &transaction,
                    store_id,
                    claimed,
                    RunOutcome::Failed {
                        failure: RunFailure {
                            kind: RunFailureKind::Policy,
                            message:
                                "exact inputs and summary exceed the 4 MiB session context limit; \
                                      history remains intact"
                                    .to_owned(),
                        },
                    },
                    accounting,
                    SettlementCause::Executor,
                )?)?);
                    transaction.commit()?;
                    return Ok((events, None));
                }
                Err(error) => return Err(error),
            };
        transaction.execute(
            "INSERT INTO session_compactions(
                     session_id, run_id, summary, cutoff_ordinal,
                     before_bytes, after_bytes, created_at_ms, scope_run_id, turn_cutoff, goal_notice
                 ) VALUES (?1, ?2, ?3, ?9, ?4, 0, ?5, ?6, ?7, ?8)",
            params![
                session.to_string(),
                claimed.identity.run_id.to_string(),
                summary,
                u64::try_from(before_bytes).unwrap_or(u64::MAX),
                now,
                claimed.compaction_cutoff_ordinal.is_none().then(|| scope_run_id.to_string()),
                claimed.compaction_cutoff_ordinal.is_none().then_some(turn_cutoff),
                goal_notice,
                claimed.compaction_cutoff_ordinal.unwrap_or(0),
            ],
        )?;
        let after_bytes = assembled_context_bytes(&transaction, session)?;
        let shrinkage_required = before_bytes > COMPACTION_SHRINKAGE_FLOOR_BYTES;
        if shrinkage_required && after_bytes >= before_bytes {
            transaction.execute(
                "DELETE FROM session_compactions WHERE session_id = ?1 AND run_id = ?2",
                params![session.to_string(), claimed.identity.run_id.to_string()],
            )?;
            outcome = RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    message: format!(
                        "in-run compaction summary did not shrink the assembled context \
                         ({after_bytes} bytes after, {before_bytes} before)"
                    ),
                },
            };
        } else {
            transaction.execute(
                "UPDATE session_compactions SET after_bytes = ?3
                     WHERE session_id = ?1 AND run_id = ?2",
                params![
                    session.to_string(),
                    claimed.identity.run_id.to_string(),
                    u64::try_from(after_bytes).unwrap_or(u64::MAX),
                ],
            )?;
            prune_compaction_history(&transaction, session)?;
            transaction.execute(
                "UPDATE sessions SET context_tokens = NULL, context_occupancy_json = NULL
                     WHERE id = ?1",
                [session.to_string()],
            )?;
            let framed_summary_bytes = crate::measure_message(&Message::user(format!(
                "{}\n\n{visible_summary}",
                if claimed.compaction_cutoff_ordinal.is_some() {
                    COMPACTION_SUMMARY_PREAMBLE
                } else {
                    IN_RUN_COMPACTION_PREAMBLE
                }
            )));
            let context_base_bytes =
                i64::try_from(retained_bytes.saturating_add(framed_summary_bytes))
                    .map_err(|_| SessionRuntimeError::OutputTooLarge)?;
            transaction.execute(
                "UPDATE runs SET context_base_bytes = ?2, context_increment_bytes = 0
                     WHERE id = ?1 AND status = 'running'",
                params![scope_run_id.to_string(), context_base_bytes],
            )?;
            events.extend(expect_settled(settle_run(
                &transaction,
                store_id,
                claimed,
                RunOutcome::Completed,
                accounting.clone(),
                SettlementCause::Executor,
            )?)?);
            let session_summary = load_session_summary(&transaction, session)?;
            events.push(append_event(
                &transaction,
                EventContext::for_run(store_id, claimed.identity, now),
                SessionEvent::SessionCompacted {
                    session: Box::new(session_summary),
                    summary: Some({
                        let mut end = summary.len().min(MAX_EVENT_SUMMARY_BYTES);
                        while !summary.is_char_boundary(end) {
                            end -= 1;
                        }
                        summary[..end].to_owned()
                    }),
                    before_bytes: u64::try_from(before_bytes).unwrap_or(u64::MAX),
                    after_bytes: u64::try_from(after_bytes).unwrap_or(u64::MAX),
                },
            )?);
            committed = Some(visible_summary);
        }
    }
    if committed.is_none() {
        events.extend(expect_settled(settle_run(
            &transaction,
            store_id,
            claimed,
            outcome,
            accounting,
            SettlementCause::Executor,
        )?)?);
    }
    transaction.commit()?;
    Ok((events, committed))
}
