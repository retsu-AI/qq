//! Compaction runs: summary validation, auto-compaction start and reload, the
//! transaction that swaps a summary in for the transcript span, and the
//! summarizer instruction.

use super::*;

/// Structural validation of a summarizer reply. Shrinkage is measured
/// separately against the real assembly inside the compaction transaction.
pub(super) fn validate_compaction_summary(summary: &str) -> Result<(), String> {
    if summary.trim().is_empty() {
        return Err("compaction produced an empty summary".to_owned());
    }
    let replacement_bytes = COMPACTION_SUMMARY_PREAMBLE
        .len()
        .saturating_add(2)
        .saturating_add(summary.len());
    if replacement_bytes > MAX_CONTEXT_BYTES {
        return Err("compaction summary exceeds the 4 MiB session context limit".to_owned());
    }
    // A heading is a line that starts with the section name (optionally
    // numbered or marked up) and then either a colon or nothing else: both
    // `1. Intent: ...` and a markdown `## 1. Intent` line with the body
    // below it count. Matching is per line so body text mentioning
    // "errors:" cannot satisfy the requirement.
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
    // One step per call; the column counts summarizer requests spent on this
    // prompt so a restart resumes the fold where it stopped.
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
                 created_at_ms, started_at_ms, plan_identity_json, plan_descriptor_json
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, ?6, ?7,
                 ?8, ?9, 0, ?10, ?10, ?11, ?12
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
            session: Box::new(summary),
            run_id,
            plan: Some(Box::new(audit.plan_identity.clone())),
        },
    )?;
    transaction.commit()?;
    Ok(Some((
        ClaimedRun {
            checkpoint: None,
            routing: None,
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
            jev_mode: original.jev_mode,
            approval_mode: original.approval_mode,
            depth: original.depth,
            root_run_id: original.root_run_id,
            purpose: original.purpose,
            // A compaction run starts inside this transaction: nothing can have
            // cancelled it yet, and it steers and edits no files.
            cancel_requested: false,
            file_state: Vec::new(),
            pending_steering: Vec::new(),
            output: None,
        },
        started,
    )))
}

/// What one summarizer request reads: the assembled context up to a unit
/// boundary plus the instruction, and the ordinal its summary will cover.
/// Whether that is everything the session holds is re-read from the store
/// after the step commits, so a fold resumes correctly after a restart.
pub(super) struct SummarizerInput {
    pub(super) messages: Vec<Message>,
    /// The prompt ordinal the summary will cover; `None` when nothing new
    /// follows the current marker, so the commit keeps its span.
    pub(super) cutoff_ordinal: Option<u64>,
    /// Bytes of the first unit when even it alone exceeds the budget. The
    /// request is still sent — the estimate is conservative and the provider
    /// adjudicates — but a rejection then names an irreducible unit, not an
    /// exhausted retry.
    pub(super) oversized_unit_bytes: Option<u64>,
}

/// Assembles the summarizer's request from the session's context after its
/// current cutoff. With a byte budget, the longest prefix of whole
/// prompt/run units that fits is taken — never fewer than one — so the
/// request fits the model window by construction and the summary it
/// produces replaces exactly that span. Without a budget (no declared
/// window) the whole context is read, as before.
pub(super) fn load_summarizer_input(
    connection: &mut Connection,
    session_id: SessionId,
    message_byte_budget: Option<u64>,
) -> Result<SummarizerInput, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (mut messages, _, units) =
        load_model_context_with_units(&transaction, session_id, u64::MAX)?;
    let instruction = Message::user(compaction_instruction(&transaction, session_id)?);
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
                // The leading summary, if any, is charged to the first unit.
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

/// Commits a compaction: the summary row and cutoff marker persist in the
/// same transaction that settles the internal run, so a crash anywhere
/// before the commit leaves no marker and the command can simply be retried.
/// Events (RunFinished, then SessionCompacted) are appended before commit —
/// persist-before-publish like every other event.
pub(super) fn complete_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    summary: String,
    accounting: Option<RunAccounting>,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    // A settled compaction already committed (or rejected) its marker; a
    // replay must not insert a second one.
    if run_is_settled(&transaction, claimed.identity.run_id)? {
        return Ok(Vec::new());
    }
    // A cancel that raced the summarizer's completion wins: the run settles
    // cancelled and no marker is committed.
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
        // The cutoff covers exactly the span the summary replaced: the
        // messages assembly showed the summarizer. A bounded step carries
        // its unit boundary; an unbounded one read everything settled. A
        // prompt still queued behind an auto-compaction has an ordinal but
        // was not summarized — it must stay after the marker so its run
        // still sends it.
        let cutoff_ordinal: u64 = match claimed.compaction_cutoff_ordinal {
            Some(cutoff) => cutoff,
            None => transaction.query_row(
                "SELECT COALESCE(MAX(ordinal), 0) FROM messages
                     WHERE session_id = ?1 AND state IN ('complete', 'interrupted')",
                [claimed.identity.session_id.to_string()],
                |row| row.get(0),
            )?,
        };
        // Insert the candidate marker, then measure the assembly it
        // produces. A summary that does not shrink the assembly is rejected
        // and the row removed within this transaction, so the prior usable
        // compaction stays authoritative.
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
        let after_bytes = assembled_context_bytes(&transaction, claimed.identity.session_id)?;
        // Shrinkage is the point of compaction. A short transcript is the one
        // exception: the structured summary's fixed framing can exceed it,
        // yet compacting it is still correct when the provider reported
        // overflow. Above that floor a summary that fails to shrink the
        // assembly is rejected outright.
        let shrinkage_required = before_bytes > COMPACTION_SHRINKAGE_FLOOR_BYTES;
        if shrinkage_required && after_bytes >= before_bytes {
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
                    message: format!(
                        "compaction summary did not shrink the assembled context \
                         ({after_bytes} bytes after, {before_bytes} before); the prior \
                         compaction, if any, remains in effect"
                    ),
                },
            };
            events.push(expect_settled(settle_run(
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
        transaction.execute(
            "UPDATE session_compactions SET after_bytes = ?3
                 WHERE session_id = ?1 AND run_id = ?2",
            params![
                claimed.identity.session_id.to_string(),
                claimed.identity.run_id.to_string(),
                u64::try_from(after_bytes).unwrap_or(u64::MAX),
            ],
        )?;
        // Bounded history, newest rows kept; no eager deletion beyond it so
        // a future rollback command can restore the previous compaction.
        transaction.execute(
            "DELETE FROM session_compactions
                 WHERE session_id = ?1 AND rowid NOT IN (
                     SELECT rowid FROM session_compactions WHERE session_id = ?1
                     ORDER BY rowid DESC LIMIT ?2
                 )",
            params![
                claimed.identity.session_id.to_string(),
                COMPACTION_HISTORY_ROWS
            ],
        )?;
        // The compaction request measured the context that was just
        // replaced, not the summary now occupying the session. Keep the
        // session unknown until its next prompt turn reports exact usage.
        transaction.execute(
            "UPDATE sessions
                 SET context_tokens = NULL,
                     context_occupancy_json = NULL,
                     pending_context_overflow_basis_json = NULL
                 WHERE id = ?1",
            [claimed.identity.session_id.to_string()],
        )?;
        events.push(expect_settled(settle_run(
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
        events.push(expect_settled(settle_run(
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

/// One persisted between-run compaction: the summary that replaces every
/// prompt at or before `cutoff_ordinal`, and their runs, in assembly.
pub(super) struct CompactionRow {
    pub(super) summary: String,
    pub(super) cutoff_ordinal: u64,
}

/// The newest between-run marker. In-run markers (`scope_run_id` set) are
/// not candidates: they replace turns inside one run and are read per run
/// by `in_run_compactions`.
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

/// One persisted in-run compaction: within the run it names, the summary
/// replaces every model turn with ordinal `<= turn_cutoff` (and their tool
/// results and applied steering). Turns after it stay verbatim.
pub(super) struct InRunCompaction {
    pub(super) summary: String,
    pub(super) turn_cutoff: u32,
}

/// The newest in-run marker of each run whose prompt is retained. Keyed by
/// run id. Runs with no marker are absent; the query is bounded by the
/// retained prompt window like every other assembly query.
pub(super) fn in_run_compactions(
    connection: &Connection,
    session: &str,
    through_ordinal: u64,
    cutoff_ordinal: u64,
) -> Result<HashMap<String, InRunCompaction>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT c.scope_run_id, c.summary, c.turn_cutoff
         FROM messages m
         JOIN session_compactions c ON c.scope_run_id = m.run_id
         WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
           AND m.role = 'user' AND m.steering = 0
           AND c.rowid = (SELECT MAX(rowid) FROM session_compactions
                          WHERE scope_run_id = m.run_id)",
    )?;
    let rows = statement.query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
        ))
    })?;
    let mut markers = HashMap::new();
    for row in rows {
        let (run_id, summary, turn_cutoff) = row?;
        markers.insert(
            run_id,
            InRunCompaction {
                summary,
                turn_cutoff,
            },
        );
    }
    Ok(markers)
}

/// The final user message of a compaction run: the fixed structured-schema
/// instruction plus the file list seeded mechanically from the session's
/// file-state table.
pub(super) fn compaction_instruction(
    connection: &Connection,
    session_id: SessionId,
) -> Result<String, SessionRuntimeError> {
    let mut statement =
        connection.prepare("SELECT path FROM session_files WHERE session_id = ?1 ORDER BY path")?;
    let paths = statement
        .query_map([session_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut instruction = String::from(COMPACTION_INSTRUCTION);
    instruction.push_str("\n\nFiles touched (seeded from the session file-state table):\n");
    if paths.is_empty() {
        instruction.push_str("(none recorded)\n");
    } else {
        let path_count = paths.len();
        for (index, path) in paths.into_iter().enumerate() {
            let required = 2_usize.saturating_add(path.len()).saturating_add(1);
            if instruction.len().saturating_add(required) > context::COMPACTION_INSTRUCTION_BYTES {
                let omitted = path_count.saturating_sub(index);
                let notice = format!("- ... {omitted} additional paths omitted\n");
                if instruction.len().saturating_add(notice.len())
                    <= context::COMPACTION_INSTRUCTION_BYTES
                {
                    instruction.push_str(&notice);
                }
                break;
            }
            instruction.push_str("- ");
            instruction.push_str(&path);
            instruction.push('\n');
        }
    }
    Ok(instruction)
}

/// Starts an in-run compaction for a prompt run that is `running`. Unlike a
/// between-run step it never takes the session's active-run slot — the
/// prompt run holds it — so `settle_run`'s slot release is a no-op for it.
/// The row carries the prompt run as its owner and the cutoff it will cover.
/// `None` when the prompt run is no longer running (cancelled or settled
/// while the loop was at the boundary).
pub(super) fn start_in_run_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    prompt_run: &ClaimedRun,
    resolved_model: &ResolvedModel,
    turn_cutoff: u32,
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
    // Every in-run step is a summarizer request charged to this prompt.
    let admitted = transaction.execute(
        "UPDATE runs SET context_compaction_attempted = context_compaction_attempted + 1
             WHERE id = ?1 AND context_compaction_attempted < ?2",
        params![
            prompt_run.identity.run_id.to_string(),
            context::MAX_COMPACTION_STEPS
        ],
    )?;
    if admitted != 1 {
        return Ok(None);
    }
    transaction.execute(
        "INSERT INTO runs(
                 id, session_id, command_id, user_message_id, assistant_message_id,
                 status, kind, auto_compaction, auto_compaction_for_run_id,
                 resolved_model_json, context_base_bytes, context_increment_bytes,
                 created_at_ms, started_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, 'running', 'compaction', 1, ?6, ?7, 0, 0, ?8, ?8
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
            session: Box::new(summary),
            run_id,
            plan: None,
        },
    )?;
    transaction.commit()?;
    let mut claimed = prompt_run.panic_settlement_claim();
    claimed.identity.run_id = run_id;
    claimed.identity.command_id = command_id;
    claimed.identity.kind = RunKind::Compaction;
    claimed.compaction_cutoff_ordinal = None;
    claimed.in_run_turn_cutoff = Some((prompt_run.identity.run_id, turn_cutoff));
    Ok(Some((claimed, started)))
}

/// Commits an in-run summary: validates it, inserts the scoped marker, checks
/// that the prompt run's assembled context shrank, settles the compaction
/// run, and publishes `SessionCompacted`. Returns the events and whether the
/// marker stands. All in one transaction, like `complete_compaction`.
pub(super) fn complete_in_run_compaction(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    summary: String,
    accounting: Option<RunAccounting>,
) -> Result<(Vec<SessionEventEnvelope>, bool), SessionRuntimeError> {
    let Some((scope_run_id, turn_cutoff)) = claimed.in_run_turn_cutoff else {
        return Err(SessionRuntimeError::CONSTRAINT);
    };
    let transaction = store::begin_unit(connection)?;
    if run_is_settled(&transaction, claimed.identity.run_id)? {
        return Ok((Vec::new(), false));
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
    let mut committed = false;
    if matches!(outcome, RunOutcome::Completed) {
        let now = now_ms();
        let session = claimed.identity.session_id;
        let before_bytes = assembled_context_bytes(&transaction, session)?;
        transaction.execute(
            "INSERT INTO session_compactions(
                     session_id, run_id, summary, cutoff_ordinal,
                     before_bytes, after_bytes, created_at_ms, scope_run_id, turn_cutoff
                 ) VALUES (?1, ?2, ?3, 0, ?4, 0, ?5, ?6, ?7)",
            params![
                session.to_string(),
                claimed.identity.run_id.to_string(),
                summary,
                u64::try_from(before_bytes).unwrap_or(u64::MAX),
                now,
                scope_run_id.to_string(),
                turn_cutoff,
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
            // Same bounded history as between-run markers; rollback pops the
            // newest row of either kind.
            transaction.execute(
                "DELETE FROM session_compactions
                     WHERE session_id = ?1 AND rowid NOT IN (
                         SELECT rowid FROM session_compactions WHERE session_id = ?1
                         ORDER BY rowid DESC LIMIT ?2
                     )",
                params![session.to_string(), COMPACTION_HISTORY_ROWS],
            )?;
            // The prompt run's measured occupancy described the context it
            // just replaced; the next turn's usage re-seeds it.
            transaction.execute(
                "UPDATE sessions SET context_tokens = NULL, context_occupancy_json = NULL
                     WHERE id = ?1",
                [session.to_string()],
            )?;
            committed = true;
            events.push(expect_settled(settle_run(
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
                    summary: Some(truncate_utf8(summary, MAX_EVENT_SUMMARY_BYTES)),
                    before_bytes: u64::try_from(before_bytes).unwrap_or(u64::MAX),
                    after_bytes: u64::try_from(after_bytes).unwrap_or(u64::MAX),
                },
            )?);
        }
    }
    if !committed {
        events.push(expect_settled(settle_run(
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
