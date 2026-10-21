//! Read-side projections: workspace and session snapshots, summaries, and
//! accounting folds over persisted runs.

use super::*;
use crate::tools::output::escaped_len;

pub(super) fn load_snapshot(
    connection: &mut Connection,
    store_id: StoreId,
    request: SnapshotRequest,
) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (path, sequence) = transaction
        .query_row(
            "SELECT path, next_sequence FROM workspaces WHERE id = ?1",
            [request.workspace_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .optional()?
        .ok_or(SessionRuntimeError::WorkspaceNotFound)?;
    let mut statement = transaction.prepare(
        "SELECT id FROM sessions WHERE workspace_id = ?1
             ORDER BY updated_at_ms DESC, rowid DESC LIMIT ?2",
    )?;
    let ids = statement
        .query_map(
            params![
                request.workspace_id.to_string(),
                u64::from(request.session_limit) + 1
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let has_older_sessions = ids.len() > usize::from(request.session_limit);
    // One grouped pass over the workspace's runs supplies every summary's
    // accounting instead of a subtree query per session.
    let mut folds = load_accounting_folds(&transaction, None, request.workspace_id)?;
    let mut sessions = Vec::with_capacity(ids.len().min(usize::from(request.session_limit)));
    for id in ids.into_iter().take(usize::from(request.session_limit)) {
        let accounting = folds.remove(&id).unwrap_or_default().total();
        sessions.push(load_session_summary_with_accounting(
            &transaction,
            parse_id(&id)?,
            accounting,
        )?);
    }
    // One byte budget spans every body in the response, spent by the focused
    // body first and then by the included bodies in request order, so the
    // response always fits the wire cap however large the transcripts are
    // (F11). Each body keeps its newest rows and reports what it dropped.
    let mut budget = SnapshotBudget::new(SNAPSHOT_BODY_BUDGET_BYTES);
    let mut focused = request
        .focused_session_id
        .map(|session_id| {
            if session_workspace(&transaction, session_id)? != request.workspace_id {
                return Err(SessionRuntimeError::SessionNotFound);
            }
            load_session_body(&transaction, session_id, request.message_limit, &mut budget)
        })
        .transpose()?;
    // Extra bodies are best-effort: a session that left the workspace or was
    // deleted since the client asked is skipped rather than failing the
    // whole snapshot. The focused body keeps its strict contract above.
    let mut included = Vec::with_capacity(request.include_sessions.len());
    for session_id in &request.include_sessions {
        if Some(*session_id) == request.focused_session_id {
            continue;
        }
        match session_workspace(&transaction, *session_id) {
            Ok(workspace_id) if workspace_id == request.workspace_id => {
                included.push(load_session_body(
                    &transaction,
                    *session_id,
                    request.message_limit,
                    &mut budget,
                )?);
            }
            Ok(_) | Err(SessionRuntimeError::SessionNotFound) => {}
            Err(error) => return Err(error),
        }
    }
    // Tool calls spend what the transcripts left, in the same order. Their
    // results are the heavy rows; admitting them per body before the next
    // body's messages let one session's large reads starve every later body
    // of its transcript, which a client then showed as an empty session.
    for body in focused.iter_mut().chain(included.iter_mut()) {
        load_tool_call_tail(&transaction, body, &mut budget)?;
    }
    let check_interruption = super::goal_check::interruption(&transaction, request.workspace_id)?;
    transaction.commit()?;
    Ok(WorkspaceSnapshot {
        check_interruption,
        cursor: EventCursor {
            store_id,
            workspace_id: request.workspace_id,
            sequence,
        },
        workspace: WorkspaceSummary {
            id: request.workspace_id,
            path,
        },
        sessions,
        focused,
        included,
        has_older_sessions,
    })
}

/// Remaining serialized bytes a snapshot response may still spend on body
/// rows. Rows are admitted newest-first, so when the budget runs out the
/// client holds the most recent history and a `has_older_*` flag telling it
/// the rest is reachable through paging and recall, never a body the wire
/// layer would refuse.
pub(super) struct SnapshotBudget {
    remaining: usize,
}

impl SnapshotBudget {
    pub(super) fn new(bytes: usize) -> Self {
        Self { remaining: bytes }
    }

    /// Charges one row whose variable text totals `text_bytes` (escaped) and
    /// says whether it fits. The check never reserves a partial row.
    pub(super) fn admit(&mut self, text_bytes: usize) -> bool {
        let cost = text_bytes.saturating_add(SNAPSHOT_ROW_OVERHEAD_BYTES);
        if cost > self.remaining {
            return false;
        }
        self.remaining -= cost;
        true
    }

    pub(super) const fn remaining(&self) -> usize {
        self.remaining
    }

    /// Charges rows admitted against a separate reserve.
    pub(super) fn charge(&mut self, text_bytes: usize, rows: usize) {
        let cost = text_bytes.saturating_add(rows.saturating_mul(SNAPSHOT_ROW_OVERHEAD_BYTES));
        self.remaining = self.remaining.saturating_sub(cost);
    }
}

/// One session body: transcript, run receipts, and its newest tool calls.
#[cfg(test)]
pub(super) fn load_session_snapshot(
    transaction: &Connection,
    session_id: SessionId,
    message_limit: u16,
    budget: &mut SnapshotBudget,
) -> Result<SessionSnapshot, SessionRuntimeError> {
    let mut body = load_session_body(transaction, session_id, message_limit, budget)?;
    load_tool_call_tail(transaction, &mut body, budget)?;
    Ok(body)
}

/// A session body without tool calls: summary, side questions, the newest
/// messages, and the run receipts they reference.
fn load_session_body(
    transaction: &Connection,
    session_id: SessionId,
    message_limit: u16,
    budget: &mut SnapshotBudget,
) -> Result<SessionSnapshot, SessionRuntimeError> {
    let summary = load_session_summary(transaction, session_id)?;
    // Side questions draw first from a reserved share so a long transcript
    // cannot starve a running question out of a reconnect snapshot.
    let side_questions = side_questions::load_side_snapshots(transaction, session_id, budget)?;
    // Messages order by run first, then by ordinal within the run, so a
    // prompt queued while a run streams does not interleave with that run's
    // later per-turn messages (which receive higher session ordinals).
    let mut statement = transaction.prepare(
        "SELECT m.id FROM messages m JOIN runs r ON r.id = m.run_id
             WHERE m.session_id = ?1 AND NOT (m.role = 'assistant' AND m.state = 'queued')
             ORDER BY r.created_at_ms DESC, r.rowid DESC, m.ordinal DESC LIMIT ?2",
    )?;
    let mut message_ids = statement
        .query_map(
            params![session_id.to_string(), u64::from(message_limit) + 1],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut has_older_messages = message_ids.len() > usize::from(message_limit);
    message_ids.truncate(usize::from(message_limit));
    // A displayed message and its authoritative run receipt are admitted together.
    // Reserve active-run metadata first; bounded bodies never contain orphan messages.
    let mut wanted_receipts = Vec::with_capacity(message_ids.len());
    if message_limit > 0
        && let Some(active) = summary.active_run_id
        && budget.admit(0)
    {
        wanted_receipts.push(active);
    }
    let mut messages = Vec::with_capacity(message_ids.len());
    for id in message_ids {
        let message = load_message(transaction, parse_id(&id)?)?;
        let needs_receipt = !wanted_receipts.contains(&message.run_id);
        if needs_receipt && wanted_receipts.len() == usize::from(message_limit) {
            has_older_messages = true;
            break;
        }
        let text_bytes = escaped_len(&message.output).saturating_add(escaped_len(&message.refusal));
        let reserved_bytes = if needs_receipt {
            text_bytes.saturating_add(SNAPSHOT_ROW_OVERHEAD_BYTES)
        } else {
            text_bytes
        };
        if !budget.admit(reserved_bytes) {
            has_older_messages = true;
            break;
        }
        if needs_receipt {
            wanted_receipts.push(message.run_id);
        }
        messages.push(message);
    }
    messages.reverse();
    if wanted_receipts.is_empty() && message_limit > 0 {
        // Standalone compaction can have no transcript messages at all.
        let terminal = transaction
            .query_row(
                "SELECT id FROM runs WHERE session_id = ?1 AND outcome_json IS NOT NULL
                 AND (kind <> 'compaction' OR auto_compaction_for_run_id IS NULL)
             ORDER BY finished_at_ms DESC, rowid DESC LIMIT 1",
                [session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(terminal) = terminal
            && budget.admit(0)
        {
            wanted_receipts.push(parse_id(&terminal)?);
        }
    }
    let remaining_runs = usize::from(message_limit).saturating_sub(wanted_receipts.len());
    let mut statement = transaction.prepare(
        "WITH pinned AS (
             SELECT id, created_at_ms, rowid AS creation_order FROM runs
             WHERE session_id = ?1 AND id IN (SELECT value FROM json_each(?2))
         ), recent AS (
             SELECT id, created_at_ms, rowid AS creation_order FROM runs
             WHERE session_id = ?1 AND id NOT IN (SELECT id FROM pinned)
             ORDER BY created_at_ms DESC, rowid DESC LIMIT ?3
         )
         SELECT id, precharged FROM (
             SELECT id, created_at_ms, creation_order, 1 AS precharged FROM pinned
             UNION ALL
             SELECT id, created_at_ms, creation_order, 0 AS precharged FROM recent
         ) ORDER BY created_at_ms DESC, creation_order DESC",
    )?;
    let run_ids = statement
        .query_map(
            params![
                session_id.to_string(),
                serde_json::to_string(&wanted_receipts)?,
                remaining_runs
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if run_ids.iter().filter(|(_, precharged)| *precharged).count() != wanted_receipts.len() {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let mut runs = Vec::with_capacity(run_ids.len());
    for (id, precharged) in run_ids {
        if precharged || budget.admit(0) {
            runs.push(load_run(transaction, parse_id(&id)?)?);
        }
    }
    runs.reverse();
    Ok(SessionSnapshot {
        goal: goals::load_live(transaction, session_id)?.map(Box::new),
        side_questions,
        summary,
        messages,
        runs,
        tool_calls: Vec::new(),
        has_older_tool_calls: false,
        has_older_messages,
    })
}

/// Admit `body`'s newest tool calls, newest first, while the budget lasts.
fn load_tool_call_tail(
    transaction: &Connection,
    body: &mut SessionSnapshot,
    budget: &mut SnapshotBudget,
) -> Result<(), SessionRuntimeError> {
    let session_id = body.summary.id;
    let mut statement = transaction.prepare(
        "SELECT t.id FROM tool_calls t JOIN runs r ON r.id = t.run_id
             WHERE r.session_id = ?1
             ORDER BY r.created_at_ms DESC, r.rowid DESC, t.turn_ordinal DESC, t.call_ordinal DESC
             LIMIT ?2",
    )?;
    let mut tool_call_ids = statement
        .query_map(
            params![
                session_id.to_string(),
                u64::try_from(MAX_SNAPSHOT_TOOL_CALLS + 1).expect("snapshot bound fits u64")
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut has_older_tool_calls = tool_call_ids.len() > MAX_SNAPSHOT_TOOL_CALLS;
    tool_call_ids.truncate(MAX_SNAPSHOT_TOOL_CALLS);
    let mut tool_calls = Vec::with_capacity(tool_call_ids.len());
    for id in tool_call_ids {
        let tool_call = load_tool_call(transaction, parse_id(&id)?)?;
        let display_bytes = match &tool_call.display {
            Some(ToolCallDisplay::Diff { path, diff }) => {
                escaped_len(path).saturating_add(escaped_len(diff))
            }
            None => 0,
        };
        let text_bytes = escaped_len(&tool_call.arguments)
            .saturating_add(tool_call.result.as_deref().map_or(0, escaped_len))
            .saturating_add(display_bytes);
        if !budget.admit(text_bytes) {
            has_older_tool_calls = true;
            break;
        }
        tool_calls.push(tool_call);
    }
    tool_calls.reverse();
    body.tool_calls = tool_calls;
    body.has_older_tool_calls = has_older_tool_calls;
    Ok(())
}

pub(super) fn session_file_state_rows(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<(String, String)>, SessionRuntimeError> {
    let mut statement = connection
        .prepare_cached("SELECT path, content_hash FROM session_files WHERE session_id = ?1")?;
    statement
        .query_map([session_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
}

pub(super) fn session_parent(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Option<SessionId>, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT parent_id FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)?
        .as_deref()
        .map(parse_id)
        .transpose()
}

#[derive(Default)]
pub(super) struct AccountingAggregate {
    pub(super) usage: TokenUsage,
    pub(super) usage_known: bool,
    pub(super) saw_usage: bool,
    pub(super) cost: Option<u64>,
}

impl AccountingAggregate {
    pub(super) fn known_zero() -> Self {
        Self {
            usage_known: true,
            saw_usage: true,
            cost: Some(0),
            ..Self::default()
        }
    }

    pub(super) fn add(
        &mut self,
        usage: TokenUsage,
        cost: Option<u64>,
    ) -> Result<(), SessionRuntimeError> {
        self.usage =
            add_usage(self.usage, usage).ok_or(SessionRuntimeError::AccountingUnavailable)?;
        self.saw_usage = true;
        self.cost = match (self.cost, cost) {
            (Some(total), Some(cost)) => Some(
                total
                    .checked_add(cost)
                    .ok_or(SessionRuntimeError::AccountingUnavailable)?,
            ),
            _ => None,
        };
        Ok(())
    }

    pub(super) fn mark_unknown(&mut self) {
        self.usage_known = false;
        self.cost = None;
    }

    pub(super) fn total(self) -> AccountingTotal {
        AccountingTotal {
            usage: (self.saw_usage && self.usage_known).then_some(self.usage),
            estimated_cost_usd_nanos: self.cost,
        }
    }
}

/// Per-session direct and inclusive aggregates, folded from run rows. The
/// default is known-zero: a session with no runs has spent nothing.
pub(super) struct SessionAccountingFold {
    pub(super) direct: AccountingAggregate,
    pub(super) inclusive: AccountingAggregate,
}

impl Default for SessionAccountingFold {
    fn default() -> Self {
        Self {
            direct: AccountingAggregate::known_zero(),
            inclusive: AccountingAggregate::known_zero(),
        }
    }
}

impl SessionAccountingFold {
    pub(super) fn total(self) -> SessionAccounting {
        SessionAccounting {
            direct: self.direct.total(),
            inclusive: self.inclusive.total(),
        }
    }
}

/// The accounting query for one root or for every session in a workspace:
/// each row is one run in the bounded subtree of `root`, tagged with that
/// root so one pass can fold every session at once.
pub(super) const ACCOUNTING_ROWS_SQL: &str = "WITH RECURSIVE subtree(root, id, depth) AS (
         SELECT id, id, 0 FROM sessions WHERE (?1 IS NULL AND workspace_id = ?2) OR id = ?1
         UNION ALL
         SELECT subtree.root, child.id, subtree.depth + 1
         FROM sessions child JOIN subtree ON child.parent_id = subtree.id
         WHERE subtree.depth < ?3
     )
     SELECT subtree.root, r.session_id, r.status, r.usage_json, r.estimated_cost_usd_nanos,
            EXISTS(SELECT 1 FROM model_turns turn WHERE turn.run_id = r.id),
            r.started_at_ms, r.routing_json IS NOT NULL OR r.goal_helper_admitted OR r.goal_request_pending
     FROM runs r
     JOIN subtree ON subtree.id = r.session_id
     ORDER BY r.rowid";

pub(super) fn load_session_accounting(
    connection: &Connection,
    session_id: SessionId,
) -> Result<SessionAccounting, SessionRuntimeError> {
    let mut folds = load_accounting_folds(
        connection,
        Some(session_id),
        WorkspaceId::from_bytes([0; 16]),
    )?;
    Ok(folds
        .remove(&session_id.to_string())
        .unwrap_or_default()
        .total())
}

/// Folds run rows into per-root accounting. `Some(root)` scopes to one
/// session's subtree; `None` covers every session in `workspace_id` in one
/// grouped pass, which is what a snapshot needs.
pub(super) fn load_accounting_folds(
    connection: &Connection,
    root: Option<SessionId>,
    workspace_id: WorkspaceId,
) -> Result<HashMap<String, SessionAccountingFold>, SessionRuntimeError> {
    // Inclusive totals are the bounded subtree, computed from run rows every
    // time: never a sum of cached child inclusives, which could double count
    // or go stale. The depth bound is the runtime's, so a store touched by a
    // deeper future build still reads back the same tree this build runs.
    let mut statement = connection.prepare_cached(ACCOUNTING_ROWS_SQL)?;
    let rows = statement.query_map(
        params![
            root.map(|root| root.to_string()),
            workspace_id.to_string(),
            MAX_CHILD_DEPTH
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<u64>>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, Option<u64>>(6)?,
                row.get::<_, bool>(7)?,
            ))
        },
    )?;

    let mut folds: HashMap<String, SessionAccountingFold> = HashMap::new();
    for row in rows {
        let (session_id, owner_id, status, encoded_usage, cost, saw_turn, started_at_ms, admitted) =
            row?;
        let fold = folds.entry(session_id.clone()).or_default();
        let SessionAccountingFold { direct, inclusive } = fold;
        let Some(encoded_usage) = encoded_usage else {
            let terminal = matches!(
                status.as_str(),
                "completed"
                    | "cancelled"
                    | "failed"
                    | "interrupted"
                    | "budget_exhausted"
                    | "paused"
            );
            // Cancellation before provider admission preserves known prior
            // accounting; an admitted goal/helper request remains uncertain.
            // Other terminal
            // rows without usage stay unknown for legacy/provider-failure
            // compatibility; a committed turn is always an explicit unknown.
            if admitted
                || saw_turn
                || (terminal && status != "cancelled" && started_at_ms.is_some())
            {
                inclusive.mark_unknown();
                if owner_id == session_id {
                    direct.mark_unknown();
                }
            }
            continue;
        };
        let usage = match serde_json::from_str::<TokenUsage>(&encoded_usage) {
            Ok(usage) => usage,
            Err(_) => {
                inclusive.mark_unknown();
                if owner_id == session_id {
                    direct.mark_unknown();
                }
                continue;
            }
        };
        inclusive.add(usage, cost)?;
        if owner_id == session_id {
            direct.add(usage, cost)?;
        }
    }
    Ok(folds)
}

pub(super) fn load_session_summary(
    connection: &Connection,
    session_id: SessionId,
) -> Result<SessionSummary, SessionRuntimeError> {
    let accounting = load_session_accounting(connection, session_id)?;
    load_session_summary_with_accounting(connection, session_id, accounting)
}

pub(super) fn load_session_summary_with_accounting(
    connection: &Connection,
    session_id: SessionId,
    accounting: SessionAccounting,
) -> Result<SessionSummary, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT s.workspace_id, s.parent_id, s.title, s.status, s.active_run_id,
                     s.queued_prompts, s.model, s.context_tokens, s.updated_at_ms,
                     (SELECT outcome_json FROM runs
                      WHERE session_id = s.id AND outcome_json IS NOT NULL
                        AND (kind <> 'compaction' OR auto_compaction_for_run_id IS NULL)
                      ORDER BY finished_at_ms DESC, rowid DESC LIMIT 1),
                     s.owner_run_id, s.spawned_by_tool_call_id, s.profile, s.correlation_json,
                     s.approval_mode, s.depth, s.purpose,
                     (SELECT activity FROM runs WHERE id = s.active_run_id), s.model_is_fallback,
                     s.reasoning_effort, s.approval_delegate,
                     (SELECT group_concat(h.id || ':' || h.approval_delegated) FROM (
                          SELECT id, approval_delegated FROM tool_calls
                           WHERE run_id = s.active_run_id AND state = 'awaiting_approval'
                             AND approval_resolution IS NULL
                           ORDER BY approval_delegated, turn_ordinal, call_ordinal
                           LIMIT ?2) h),
                     (SELECT id FROM side_questions
                       WHERE session_id = s.id AND state = 'running')
              FROM sessions s WHERE s.id = ?1",
            params![
                session_id.to_string(),
                u64::try_from(MAX_PENDING_HOLDS).expect("hold bound fits u64")
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, u16>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                    row.get::<_, u64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, u16>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, bool>(18)?,
                    row.get::<_, Option<String>>(19)?,
                    row.get::<_, Option<String>>(20)?,
                    row.get::<_, Option<String>>(21)?,
                    row.get::<_, Option<String>>(22)?,
                ))
            },
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)
        .and_then(
            |(
                workspace,
                parent,
                title,
                status,
                active,
                queued,
                model,
                context_tokens,
                updated,
                last_outcome,
                owner_run,
                spawned_by_call,
                profile,
                correlation,
                approval_mode,
                depth,
                purpose,
                activity,
                model_is_fallback,
                reasoning_effort,
                approval_delegate,
                holds,
                running_side_question,
            )| {
                let direct_cost = accounting.direct.estimated_cost_usd_nanos;
                let active_run_id: Option<RunId> = active.as_deref().map(parse_id).transpose()?;
                let activity = match active_run_id {
                    Some(_) => activity.as_deref().map(parse_run_activity).transpose()?,
                    None => None,
                };
                let spawned_by = match owner_run {
                    Some(owner_run) => Some(SpawnOrigin {
                        run_id: parse_id(&owner_run)?,
                        tool_call_id: spawned_by_call.as_deref().map(parse_id).transpose()?,
                        depth,
                    }),
                    None => None,
                };
                Ok(SessionSummary {
                    model_is_fallback,
                    id: session_id,
                    workspace_id: parse_id(&workspace)?,
                    parent_id: parent.as_deref().map(parse_id).transpose()?,
                    spawned_by,
                    purpose: match purpose.as_str() {
                        "task" => SessionPurpose::Task,
                        "audit" => SessionPurpose::Audit,
                        _ => return Err(SessionRuntimeError::CODEC),
                    },
                    title,
                    status: parse_session_status(&status)?,
                    active_run_id,
                    activity,
                    queued_prompts: queued,
                    model,
                    profile: parse_profile(profile.as_deref())?,
                    approval_mode: parse_approval_mode(&approval_mode)?,
                    approval_delegate: parse_approval_delegate(approval_delegate.as_deref())?,
                    reasoning_effort: parse_reasoning_effort(reasoning_effort.as_deref())?,
                    correlation: parse_correlation(correlation.as_deref())?,
                    context_tokens,
                    accounting: Some(accounting),
                    estimated_cost_usd_nanos: direct_cost,
                    updated_at_ms: updated,
                    last_outcome: last_outcome
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?,
                    pending_holds: match holds {
                        // `id:delegated` pairs; ids are hex and never hold
                        // the separators.
                        Some(holds) if active_run_id.is_some() => holds
                            .split(',')
                            .map(|hold| match hold.split_once(':') {
                                Some((id, "0")) => Ok(PendingHold {
                                    tool_call_id: parse_id(id)?,
                                    delegated: false,
                                }),
                                Some((id, "1")) => Ok(PendingHold {
                                    tool_call_id: parse_id(id)?,
                                    delegated: true,
                                }),
                                Some(_) | None => Err(SessionRuntimeError::CODEC),
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                        Some(_) | None => Vec::new(),
                    },
                    running_side_question: running_side_question
                        .as_deref()
                        .map(parse_id)
                        .transpose()?,
                })
            },
        )
}

/// Estimated cost of one usage report under `pricing`, in USD nanos. `None`
/// when the pricing table lacks a rate the usage needs (a cached read without
/// a cache price) or the arithmetic overflows.
pub fn run_cost(usage: TokenUsage, pricing: &ModelPricing) -> Option<u64> {
    let total_input = usage
        .input_tokens
        .checked_add(usage.cache_read_input_tokens)?
        .checked_add(usage.cache_write_input_tokens)?;
    let tier = pricing
        .context_tier
        .as_ref()
        .filter(|tier| total_input > tier.above_input_tokens);
    let input_rate = tier.map_or(pricing.input_usd_nanos_per_token, |tier| {
        tier.input_usd_nanos_per_token
    });
    let output_rate = tier.map_or(pricing.output_usd_nanos_per_token, |tier| {
        tier.output_usd_nanos_per_token
    });
    let cache_read_price = tier
        .and_then(|tier| tier.cache_read_usd_nanos_per_token)
        .or(pricing.cache_read_usd_nanos_per_token);
    let cache_write_price = tier
        .and_then(|tier| tier.cache_write_usd_nanos_per_token)
        .or(pricing.cache_write_usd_nanos_per_token);
    let cache_read_rate = if usage.cache_read_input_tokens == 0 {
        0
    } else {
        cache_read_price?
    };
    let cache_write_rate = if usage.cache_write_input_tokens == 0 {
        0
    } else {
        cache_write_price?
    };
    let total = u128::from(usage.input_tokens)
        .checked_mul(u128::from(input_rate))?
        .checked_add(u128::from(usage.output_tokens).checked_mul(u128::from(output_rate))?)?
        .checked_add(
            u128::from(usage.cache_read_input_tokens).checked_mul(u128::from(cache_read_rate))?,
        )?
        .checked_add(
            u128::from(usage.cache_write_input_tokens).checked_mul(u128::from(cache_write_rate))?,
        )?;
    u64::try_from(total).ok()
}

/// Keyset pagination is bounded and stays on the store's blocking worker.
pub(super) fn load_workspace_catalog(
    connection: &mut Connection,
    store_id: StoreId,
    request: qq_protocol::WorkspaceCatalogRequest,
) -> Result<qq_protocol::WorkspaceCatalog, SessionRuntimeError> {
    let after = request.after.map(|cursor| cursor.workspace_id.to_string());
    let mut statement = connection.prepare(
        "SELECT id, path FROM workspaces WHERE (?1 IS NULL OR id > ?1) ORDER BY id LIMIT ?2",
    )?;
    let rows = statement
        .query_map(params![after, u32::from(request.limit) + 1], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = rows.len() > usize::from(request.limit);
    let workspaces = rows
        .into_iter()
        .take(usize::from(request.limit))
        .map(|(id, path)| {
            Ok(WorkspaceSummary {
                id: parse_id(&id)?,
                path,
            })
        })
        .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
    let next = has_more.then(|| qq_protocol::WorkspaceCatalogCursor {
        store_id,
        workspace_id: workspaces.last().expect("a nonempty bounded page").id,
    });
    Ok(qq_protocol::WorkspaceCatalog {
        store_id,
        workspaces,
        next,
    })
}
