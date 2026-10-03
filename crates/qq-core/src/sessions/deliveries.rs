//! Non-blocking child delivery (ADR-0054 § 4): the `child_deliveries` row a
//! detached child is admitted with, the one transaction that stamps a settled
//! child's answer into its parent's context, and the notice text assembly
//! replays. A row is stamped once; every path that delivers (the parent's
//! turn boundary, the parent's settlement, recovery) goes through
//! [`deliver_settled_children`], so an answer is delivered exactly once.

use super::*;

/// Byte bound on one delivered answer. Several answers can arrive at one
/// boundary, so each is held to a third of a turn's whole tool-output
/// budget: the three children a run may have in flight together cost the
/// parent no more context than one turn of tool results.
pub(super) const MAX_DELIVERED_ANSWER_BYTES: usize =
    crate::tools::output::MAX_TURN_TOOL_OUTPUT_BYTES / MAX_CONCURRENT_CHILDREN_PER_RUN as usize;

/// Opens a delivered answer in the parent's context. The child's text is
/// evidence the parent asked for, not a user instruction.
const DELIVERY_PREAMBLE: &str = "[QQ runtime notice; not a user instruction]\nA sub-agent you \
started has finished.";

/// What one settled child left its parent, read inside a transaction so the
/// boundary delivery and the blocking `spawn_agent` result say the same.
pub(super) struct ChildAnswer {
    pub(super) content: String,
    pub(super) is_error: bool,
}

/// The parent-facing answer of a settled child run: its final text, else its
/// latest report labelled as interim, else the outcome as an error.
pub(super) fn child_answer(
    connection: &Connection,
    run_id: RunId,
    outcome: &RunOutcome,
) -> Result<ChildAnswer, SessionRuntimeError> {
    // A child that stopped short still leaves its latest report: the
    // parent gets the partial findings with the reason it stopped.
    let error = |reason: String| -> Result<ChildAnswer, SessionRuntimeError> {
        let content = match run_latest_report_text(connection, run_id)? {
            Some(report) => format!("{reason}\n\nIts latest progress report:\n\n{report}"),
            None => reason,
        };
        Ok(ChildAnswer {
            content,
            is_error: true,
        })
    };
    Ok(match outcome {
        RunOutcome::Completed => {
            let text = run_final_text(connection, run_id)?;
            if !text.trim().is_empty() {
                ChildAnswer {
                    content: text,
                    is_error: false,
                }
            } else {
                // A child that ended without a final answer (an empty
                // final-answer turn, ADR-0054 § 3) still leaves its own
                // durable reports; the latest one is the answer, labelled.
                match run_latest_report_text(connection, run_id)? {
                    Some(report) => ChildAnswer {
                        content: format!("{}\n\n{report}", subagents::INTERIM_REPORT_LABEL),
                        is_error: false,
                    },
                    None => ChildAnswer {
                        content: "the sub-agent completed without producing any text".to_owned(),
                        is_error: true,
                    },
                }
            }
        }
        RunOutcome::Cancelled => error("the sub-agent run was cancelled".to_owned())?,
        RunOutcome::Interrupted => error("the sub-agent run was interrupted".to_owned())?,
        RunOutcome::BudgetExhausted { exhaustion } => error(format!(
            "the sub-agent run exhausted its budget: {}",
            exhaustion.message
        ))?,
        RunOutcome::Paused { pause } => error(format!(
            "the sub-agent run paused on a provider fault after {} retries: {}",
            pause.attempts, pause.message
        ))?,
        RunOutcome::Failed { failure } => {
            error(format!("the sub-agent run failed: {}", failure.message))?
        }
    })
}

/// The final committed model turn's text/refusal. Earlier assistant turns
/// remain in the child transcript but never reach the parent.
pub(super) fn run_final_text(
    connection: &Connection,
    run_id: RunId,
) -> Result<String, SessionRuntimeError> {
    let final_turn_message = connection
        .query_row(
            "SELECT m.id
             FROM model_turns t
             LEFT JOIN messages m
               ON m.run_id = t.run_id
              AND m.turn_ordinal = t.turn_ordinal
              AND m.role = 'assistant'
              AND m.state = 'complete'
             WHERE t.run_id = ?1
             ORDER BY t.turn_ordinal DESC, m.ordinal DESC
             LIMIT 1",
            [run_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let message_id = match final_turn_message {
        Some(message_id) => message_id,
        None => connection
            .query_row(
                "SELECT id FROM messages
                 WHERE run_id = ?1 AND role = 'assistant' AND state = 'complete'
                 ORDER BY turn_ordinal DESC, ordinal DESC LIMIT 1",
                [run_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?,
    };
    let Some(message_id) = message_id else {
        return Ok(String::new());
    };
    let message = load_message(connection, parse_id(&message_id)?)?;
    let output = message.output;
    let refusal = message.refusal;
    if output.is_empty() {
        return Ok(refusal);
    }
    if refusal.is_empty() {
        return Ok(output);
    }
    Ok(format!("{output}\n{refusal}"))
}

/// The run's latest report with text: the reply to the last report or
/// stall-report notice that has any (ADR-0054 § 3). A report keeps its notice
/// on the first attempt's row and spans the rows after it up to the next
/// notice. Every later row in the span was asked to continue the same reply
/// from where it stopped (an output cut, a mid-stream fault, an interrupt), so
/// the report is the span's text joined in order. `None` when the run never
/// reported with text.
pub(super) fn run_latest_report_text(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<String>, SessionRuntimeError> {
    let mut statement = connection.prepare(
        "SELECT t.notice, m.id
         FROM model_turns t
         LEFT JOIN messages m
           ON m.run_id = t.run_id
          AND m.turn_ordinal = t.turn_ordinal
          AND m.role = 'assistant'
          AND m.state = 'complete'
         WHERE t.run_id = ?1
         ORDER BY t.turn_ordinal, m.ordinal",
    )?;
    let rows = statement
        .query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // Walk forward: each notice opens a span, and a report span's text
    // accumulates across its attempts. The last report with text wins.
    let mut in_report = false;
    let mut span = String::new();
    let mut latest: Option<String> = None;
    for (notice, message) in rows {
        if let Some(notice) = notice {
            in_report = match crate::runtime::TurnNotice::from_stored(&notice) {
                Some(
                    crate::runtime::TurnNotice::Report | crate::runtime::TurnNotice::StallReport,
                ) => true,
                Some(
                    crate::runtime::TurnNotice::Continuation
                    | crate::runtime::TurnNotice::FinalAnswer,
                ) => false,
                None => return Err(SessionRuntimeError::CODEC),
            };
            span.clear();
        }
        if in_report && let Some(id) = message {
            span.push_str(&load_message(connection, parse_id(&id)?)?.output);
            if !span.trim().is_empty() {
                latest = Some(span.clone());
            }
        }
    }
    Ok(latest)
}

/// The settled run's own spend plus its exact owned descendants'. Later user
/// prompts in a child session are separate runs and do not enter it. Missing
/// usage or cost stays unknown; an unsettled descendant is an error.
pub(super) fn owned_run_spend(
    connection: &Connection,
    run_id: RunId,
) -> Result<SpawnAgentSpend, SessionRuntimeError> {
    let session_id: String = connection.query_row(
        "SELECT session_id FROM runs WHERE id = ?1",
        [run_id.to_string()],
        |row| row.get(0),
    )?;
    let workspace_id: String = connection.query_row(
        "SELECT workspace_id FROM sessions WHERE id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    // Child creation atomically stores its original user message at
    // ordinal one; compaction retains that row. Follow owner run ids
    // and this message's exact run, not all runs in a child session.
    // Left joins keep missing identities visible as hard failures.
    // Materialize the bounded workspace candidates once so recursion
    // does not rescan every session for each owned run.
    let mut statement = connection
        .prepare_cached(
            "WITH RECURSIVE candidates AS MATERIALIZED (
                 SELECT id, owner_run_id FROM sessions
                 WHERE workspace_id = ?2 AND owner_run_id IS NOT NULL
             ), owned(run_id, depth) AS (
                 VALUES (?1, 0)
                 UNION ALL
                 SELECT initial.id, owned.depth + 1
                 FROM owned
                 JOIN candidates child ON child.owner_run_id = owned.run_id
                 LEFT JOIN messages first ON first.session_id = child.id
                     AND first.ordinal = 1 AND first.role = 'user'
                 LEFT JOIN runs initial ON initial.id = first.run_id
                     AND initial.session_id = child.id
                     AND initial.user_message_id = first.id
                 WHERE owned.depth < ?3
                 LIMIT ?4
             )
             SELECT r.id,
                    r.outcome_json IS NOT NULL AND r.status IN
                        ('completed', 'cancelled', 'failed', 'interrupted', 'budget_exhausted', 'paused'),
                    r.usage_json, r.estimated_cost_usd_nanos,
                    r.status = 'cancelled' AND r.started_at_ms IS NULL AND r.routing_json IS NULL
                        AND NOT EXISTS(SELECT 1 FROM model_turns t WHERE t.run_id = r.id),
                    owned.depth = ?3 AND EXISTS(
                        SELECT 1 FROM candidates child
                        WHERE child.owner_run_id = owned.run_id
                    )
             FROM owned LEFT JOIN runs r ON r.id = owned.run_id",
        )
        ?;
    let rows = statement.query_map(
        params![
            run_id.to_string(),
            workspace_id,
            MAX_CHILD_DEPTH,
            usize::from(MAX_DESCENDANTS_PER_ROOT) + 2,
        ],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<u64>>(3)?,
                row.get::<_, Option<bool>>(4)?.unwrap_or(false),
                row.get::<_, bool>(5)?,
            ))
        },
    )?;
    let mut spend = SpawnAgentSpend::NONE;
    for (index, row) in rows.enumerate() {
        let (id, settled, encoded_usage, cost, never_started, too_deep) = row?;
        if id.is_none() || !settled || too_deep || index > usize::from(MAX_DESCENDANTS_PER_ROOT) {
            return Err(SessionRuntimeError::AccountingUnavailable);
        }
        let usage = match encoded_usage {
            Some(encoded) => Some(serde_json::from_str::<TokenUsage>(&encoded)?),
            None if never_started => SpawnAgentSpend::NONE.usage,
            None => None,
        };
        spend.usage = match (spend.usage, usage) {
            (Some(total), Some(usage)) => {
                Some(add_usage(total, usage).ok_or(SessionRuntimeError::AccountingUnavailable)?)
            }
            _ => None,
        };
        let cost = cost.or(never_started.then_some(0));
        spend.cost_usd_nanos = match (spend.cost_usd_nanos, cost) {
            (Some(total), Some(cost)) => Some(
                total
                    .checked_add(cost)
                    .ok_or(SessionRuntimeError::AccountingUnavailable)?,
            ),
            _ => None,
        };
    }
    Ok(spend)
}

/// The notice a delivered answer enters the parent's context as. Its child
/// session id lets the parent name the child it is reading about.
pub(super) fn delivery_notice(
    child_session: SessionId,
    title: &str,
    answer: &ChildAnswer,
) -> String {
    let status = if answer.is_error {
        "It did not answer"
    } else {
        "Its answer"
    };
    // Head and tail are kept, with the cut named, exactly as a long tool
    // result is; the complete answer stays in the child's transcript.
    let content = crate::tools::output::bound_text(
        crate::tools::output::mask_secrets(answer.content.clone()),
        &crate::tools::output::Bounds::new(
            MAX_DELIVERED_ANSWER_BYTES,
            crate::tools::output::MAX_MODEL_TEXT_LINES,
        ),
        Some(&format!(
            "the full answer is in sub-agent session {child_session}"
        )),
    )
    .text;
    format!("{DELIVERY_PREAMBLE}\nSub-agent {child_session} (\"{title}\"). {status}:\n\n{content}")
}

/// Records a detached child at admission, in the admission's transaction, so
/// an admitted child always has a delivery row to stamp.
pub(super) fn insert_child_delivery(
    transaction: &Connection,
    child_run_id: RunId,
    parent_session_id: SessionId,
    parent_run_id: RunId,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    transaction.execute(
        "INSERT INTO child_deliveries(child_run_id, parent_session_id, parent_run_id, created_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            child_run_id.to_string(),
            parent_session_id.to_string(),
            parent_run_id.to_string(),
            now,
        ],
    )?;
    Ok(())
}

/// One answer stamped into the parent's context by this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveredAnswer {
    pub(crate) child_run_id: RunId,
    pub(crate) child_session_id: SessionId,
    /// The notice text the parent's next request carries.
    pub(crate) notice: String,
    pub(crate) is_error: bool,
    pub(crate) spend: SpawnAgentSpend,
}

/// Stamps every settled, undelivered detached child of `parent_run_id`, at
/// most `limit` of them, in settlement order. `turn_ordinal` is the parent
/// turn whose request carries the notices; `None` when the parent settled
/// first and the notices follow its run. Runs inside the caller's
/// transaction: a live boundary commits it alone, a settlement commits it
/// with the parent's outcome. A child whose spend cannot be read stays
/// undelivered and is reported as such, never stamped without it.
pub(super) fn deliver_settled_children(
    transaction: &Connection,
    parent_run_id: RunId,
    turn_ordinal: Option<u32>,
    limit: usize,
    now: u64,
) -> Result<Vec<DeliveredAnswer>, SessionRuntimeError> {
    let mut statement = transaction.prepare_cached(
        "SELECT d.child_run_id, r.session_id, r.outcome_json, s.title
         FROM child_deliveries d
         JOIN runs r ON r.id = d.child_run_id
         JOIN sessions s ON s.id = r.session_id
         WHERE d.parent_run_id = ?1 AND d.delivered_at_ms IS NULL
           AND r.outcome_json IS NOT NULL
         ORDER BY r.finished_at_ms, r.rowid
         LIMIT ?2",
    )?;
    let settled = statement
        .query_map(params![parent_run_id.to_string(), limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if settled.is_empty() {
        return Ok(Vec::new());
    }
    let mut next_ordinal: u32 = transaction.query_row(
        "SELECT COALESCE(MAX(delivery_ordinal), 0) + 1 FROM child_deliveries
         WHERE parent_run_id = ?1",
        [parent_run_id.to_string()],
        |row| row.get(0),
    )?;
    let mut delivered = Vec::with_capacity(settled.len());
    for (child_run, child_session, outcome_json, title) in settled {
        let child_run_id: RunId = parse_id(&child_run)?;
        let child_session_id: SessionId = parse_id(&child_session)?;
        let outcome: RunOutcome = serde_json::from_str(&outcome_json)?;
        // A descendant still settling (recovery settles runs in no order)
        // leaves the spend unreadable: deliver on a later pass, never
        // without the spend.
        let spend = match owned_run_spend(transaction, child_run_id) {
            Ok(spend) => spend,
            Err(SessionRuntimeError::AccountingUnavailable) => continue,
            Err(error) => return Err(error),
        };
        let answer = child_answer(transaction, child_run_id, &outcome)?;
        let notice = delivery_notice(child_session_id, &title, &answer);
        let stamped = transaction.execute(
            "UPDATE child_deliveries
                 SET delivered_at_ms = ?2, delivery_ordinal = ?3, turn_ordinal = ?4, text = ?5
                 WHERE child_run_id = ?1 AND delivered_at_ms IS NULL",
            params![child_run, now, next_ordinal, turn_ordinal, notice],
        )?;
        if stamped != 1 {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        next_ordinal = next_ordinal.saturating_add(1);
        delivered.push(DeliveredAnswer {
            child_run_id,
            child_session_id,
            notice,
            is_error: answer.is_error,
            spend,
        });
    }
    Ok(delivered)
}

/// A child settling after its parent did (recovery, a panicked parent)
/// delivers into that parent's session at once: no boundary will come.
pub(super) fn deliver_to_settled_parent(
    transaction: &Connection,
    child_run_id: RunId,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let parent = transaction
        .query_row(
            "SELECT d.parent_run_id FROM child_deliveries d
             JOIN runs p ON p.id = d.parent_run_id
             WHERE d.child_run_id = ?1 AND d.delivered_at_ms IS NULL
               AND p.outcome_json IS NOT NULL",
            [child_run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(parent) = parent {
        deliver_settled_children(
            transaction,
            parse_id(&parent)?,
            None,
            usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
            now,
        )?;
    }
    Ok(())
}

/// After recovery has settled every run: each settled parent with a settled,
/// undelivered child receives it.
pub(super) fn deliver_orphaned_answers(
    transaction: &Connection,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let mut statement = transaction.prepare(
        "SELECT DISTINCT d.parent_run_id FROM child_deliveries d
         JOIN runs p ON p.id = d.parent_run_id
         JOIN runs c ON c.id = d.child_run_id
         WHERE d.delivered_at_ms IS NULL
           AND p.outcome_json IS NOT NULL AND c.outcome_json IS NOT NULL",
    )?;
    let parents = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for parent in parents {
        deliver_settled_children(
            transaction,
            parse_id(&parent)?,
            None,
            usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
            now,
        )?;
    }
    Ok(())
}

/// Delivered notices per parent run, in delivery order, keyed by the parent
/// turn whose request first carried each (`None`: after the run).
pub(super) type DeliveredNotices =
    HashMap<String, std::collections::VecDeque<(Option<u32>, String)>>;

/// Every delivered notice of the runs a context assembly retains: the same
/// prompt window and state filter the turn query uses.
pub(super) fn retained_deliveries(
    transaction: &Connection,
    session: &str,
    through_ordinal: u64,
    cutoff_ordinal: u64,
) -> Result<DeliveredNotices, SessionRuntimeError> {
    let mut statement = transaction.prepare_cached(
        "SELECT d.parent_run_id, d.turn_ordinal, d.text
             FROM messages m
             JOIN child_deliveries d ON d.parent_run_id = m.run_id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
               AND d.delivered_at_ms IS NOT NULL
             ORDER BY d.parent_run_id, d.delivery_ordinal",
    )?;
    let rows = statement.query_map(params![session, through_ordinal, cutoff_ordinal], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<u32>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut notices = DeliveredNotices::new();
    for row in rows {
        let (run, turn, text) = row?;
        notices
            .entry(run)
            .or_default()
            .push_back((turn, text.ok_or(SessionRuntimeError::CODEC)?));
    }
    Ok(notices)
}
