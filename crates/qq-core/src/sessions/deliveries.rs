use super::*;

pub(super) const MAX_DELIVERED_ANSWER_BYTES: usize = crate::tools::output::MAX_MODEL_TEXT_BYTES;

const DELIVERY_PREAMBLE: &str = "[QQ runtime notice; not a user instruction]\nA sub-agent you \
started has finished.";

const INTERIM_PREAMBLE: &str = "[QQ runtime notice; not a user instruction]\nA sub-agent you \
started is still working. This is its latest progress report, not its answer; the answer \
arrives later. Use what helps now, or cancel_agent it if the report already answers you.";

pub(super) struct ChildAnswer {
    pub(super) content: String,
    pub(super) is_error: bool,
}

pub(super) fn child_answer(
    connection: &Connection,
    run_id: RunId,
    outcome: &RunOutcome,
) -> Result<ChildAnswer, SessionRuntimeError> {
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
        RunOutcome::Paused { pause } => error(match pause.as_ref() {
            qq_protocol::RunPause::ProviderRetry {
                attempts, message, ..
            } => format!(
                "the sub-agent run paused on a provider fault after {attempts} retries: {message}"
            ),
            qq_protocol::RunPause::NoProgress { .. } => format!("the sub-agent run {pause}"),
        })?,
        RunOutcome::Failed { failure } => {
            error(format!("the sub-agent run failed: {}", failure.message))?
        }
    })
}

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

struct ReportSpan {
    turn_ordinal: u32,
    messages: Vec<String>,
    closed: bool,
}

fn report_spans(
    connection: &Connection,
    run_id: RunId,
    after: u32,
) -> Result<Vec<ReportSpan>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT t.turn_ordinal, t.notice, m.id
         FROM model_turns t
         LEFT JOIN messages m
           ON m.run_id = t.run_id
          AND m.turn_ordinal = t.turn_ordinal
          AND m.role = 'assistant'
          AND m.state = 'complete'
         WHERE t.run_id = ?1 AND t.turn_ordinal > ?2
         ORDER BY t.turn_ordinal, m.ordinal",
    )?;
    let rows = statement.query_map(params![run_id.to_string(), after], |row| {
        Ok((
            row.get::<_, u32>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut spans: Vec<ReportSpan> = Vec::new();
    let mut open: Option<usize> = None;
    let mut previous_turn = None;
    for row in rows {
        let (turn_ordinal, notice, message) = row?;
        let first_row = previous_turn != Some(turn_ordinal);
        previous_turn = Some(turn_ordinal);
        if first_row && let Some(notice) = notice {
            if let Some(index) = open.take() {
                spans[index].closed = true;
            }
            let report = match crate::runtime::TurnNotice::from_stored(&notice) {
                Some(
                    crate::runtime::TurnNotice::Report
                    | crate::runtime::TurnNotice::StallReport
                    | crate::runtime::TurnNotice::RootStallReport,
                ) => true,
                Some(
                    crate::runtime::TurnNotice::Continuation
                    | crate::runtime::TurnNotice::FinalAnswer,
                ) => false,
                None => return Err(SessionRuntimeError::CODEC),
            };
            if report {
                spans.push(ReportSpan {
                    turn_ordinal,
                    messages: Vec::new(),
                    closed: false,
                });
                open = Some(spans.len() - 1);
            }
        }
        if let (Some(index), Some(id)) = (open, message) {
            spans[index].messages.push(id);
        }
    }
    Ok(spans)
}

fn newest_report(
    connection: &Connection,
    spans: Vec<ReportSpan>,
    eligible: impl Fn(&ReportSpan) -> bool,
) -> Result<Option<(u32, String)>, SessionRuntimeError> {
    for span in spans.into_iter().rev().filter(|span| eligible(span)) {
        let mut text = String::new();
        for id in &span.messages {
            text.push_str(&load_message(connection, parse_id(id)?)?.output);
        }
        if !text.trim().is_empty() {
            return Ok(Some((span.turn_ordinal, text)));
        }
    }
    Ok(None)
}

pub(super) fn run_latest_report_text(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<String>, SessionRuntimeError> {
    Ok(
        newest_report(connection, report_spans(connection, run_id, 0)?, |_| true)?
            .map(|(_, text)| text),
    )
}

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
             ), billable(run_id, depth, internal) AS (
                 SELECT run_id, depth, 0 FROM owned
                 UNION ALL
                 SELECT step.id, owned.depth, 1
                 FROM owned JOIN runs step
                   ON step.auto_compaction_for_run_id = owned.run_id
                 WHERE step.kind = 'compaction'
                   AND step.auto_compaction_for_run_id IS NOT NULL
             )
             SELECT r.id,
                    r.outcome_json IS NOT NULL AND r.status IN
                        ('completed', 'cancelled', 'failed', 'interrupted', 'budget_exhausted', 'paused'),
                    r.usage_json, r.estimated_cost_usd_nanos,
                    r.status = 'cancelled' AND r.started_at_ms IS NULL AND r.routing_json IS NULL
                        AND NOT EXISTS(SELECT 1 FROM model_turns t WHERE t.run_id = r.id),
                    NOT billable.internal AND billable.depth = ?3 AND EXISTS(
                        SELECT 1 FROM candidates child
                        WHERE child.owner_run_id = billable.run_id
                    ), billable.internal
             FROM billable LEFT JOIN runs r ON r.id = billable.run_id",
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
                row.get::<_, bool>(6)?,
            ))
        },
    )?;
    let mut spend = SpawnAgentSpend::NONE;
    let mut owned_count = 0_usize;
    for row in rows {
        let (id, settled, encoded_usage, cost, never_started, too_deep, internal) = row?;
        if !internal {
            owned_count += 1;
        }
        if id.is_none()
            || !settled
            || too_deep
            || owned_count > usize::from(MAX_DESCENDANTS_PER_ROOT) + 1
        {
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

pub(super) fn delivery_notice(
    child_session: SessionId,
    title: &str,
    answer: &ChildAnswer,
    max_bytes: usize,
) -> String {
    let status = if answer.is_error {
        "It did not answer"
    } else {
        "Its answer"
    };
    bounded_notice(
        DELIVERY_PREAMBLE,
        child_session,
        title,
        status,
        "answer",
        answer.content.clone(),
        max_bytes,
    )
}

fn interim_notice(
    child_session: SessionId,
    title: &str,
    report: String,
    max_bytes: usize,
) -> String {
    bounded_notice(
        INTERIM_PREAMBLE,
        child_session,
        title,
        "Its interim report",
        "report",
        report,
        max_bytes,
    )
}

fn bounded_notice(
    preamble: &str,
    child_session: SessionId,
    title: &str,
    status: &str,
    noun: &str,
    content: String,
    max_bytes: usize,
) -> String {
    let content = crate::tools::output::bound_text(
        crate::tools::output::mask_secrets(content),
        &crate::tools::output::Bounds::new(max_bytes, crate::tools::output::MAX_MODEL_TEXT_LINES),
        Some(&format!(
            "the full {noun} is in sub-agent session {child_session}"
        )),
    )
    .text;
    format!("{preamble}\nSub-agent {child_session} (\"{title}\"). {status}:\n\n{content}")
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveredAnswer {
    pub(crate) child_run_id: RunId,
    pub(crate) child_session_id: SessionId,
    pub(crate) notice: String,
    pub(crate) is_error: bool,
    pub(crate) spend: SpawnAgentSpend,
    pub(crate) interim: bool,
}

impl DeliveredAnswer {
    pub(crate) fn answered(&self) -> bool {
        !self.is_error && !self.interim
    }
}

fn next_delivery_ordinal(
    transaction: &Connection,
    parent_run_id: &str,
) -> Result<u32, SessionRuntimeError> {
    Ok(transaction.query_row(
        "SELECT MAX(
             (SELECT COALESCE(MAX(delivery_ordinal), 0) FROM child_deliveries
              WHERE parent_run_id = ?1),
             (SELECT COALESCE(MAX(delivery_ordinal), 0) FROM child_reports
              WHERE parent_run_id = ?1)
         ) + 1",
        [parent_run_id],
        |row| row.get(0),
    )?)
}

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
    let mut next_ordinal = next_delivery_ordinal(transaction, &parent_run_id.to_string())?;
    let mut delivered = Vec::with_capacity(settled.len());
    let mut boundary_remaining = crate::tools::output::MAX_TURN_TOOL_OUTPUT_BYTES;
    for (child_run, child_session, outcome_json, title) in settled {
        let child_run_id: RunId = parse_id(&child_run)?;
        let child_session_id: SessionId = parse_id(&child_session)?;
        let outcome: RunOutcome = serde_json::from_str(&outcome_json)?;
        let spend = match owned_run_spend(transaction, child_run_id) {
            Ok(spend) => spend,
            Err(SessionRuntimeError::AccountingUnavailable) => continue,
            Err(error) => return Err(error),
        };
        let answer = child_answer(transaction, child_run_id, &outcome)?;
        let notice = delivery_notice(
            child_session_id,
            &title,
            &answer,
            boundary_remaining.clamp(
                crate::tools::output::MIN_MODEL_TEXT_BYTES,
                MAX_DELIVERED_ANSWER_BYTES,
            ),
        );
        boundary_remaining = boundary_remaining.saturating_sub(notice.len());
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
            interim: false,
        });
    }
    Ok(delivered)
}

pub(super) fn deliver_interim_reports(
    transaction: &Connection,
    parent_run_id: RunId,
    turn_ordinal: u32,
    spent: usize,
    limit: usize,
    now: u64,
) -> Result<Vec<DeliveredAnswer>, SessionRuntimeError> {
    let parent_run = parent_run_id.to_string();
    let mut statement = transaction.prepare_cached(
        "SELECT d.child_run_id, r.session_id, s.title, d.parent_session_id,
                COALESCE((SELECT MAX(c.child_turn_ordinal) FROM child_reports c
                          WHERE c.child_run_id = d.child_run_id), 0)
         FROM child_deliveries d
         JOIN runs r ON r.id = d.child_run_id
         JOIN sessions s ON s.id = r.session_id
         WHERE d.parent_run_id = ?1 AND d.delivered_at_ms IS NULL
           AND r.outcome_json IS NULL
         ORDER BY d.created_at_ms, d.rowid
         LIMIT ?2",
    )?;
    let running = statement
        .query_map(params![parent_run, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut delivered = Vec::new();
    let mut next_ordinal: Option<u32> = None;
    let mut boundary_remaining =
        crate::tools::output::MAX_TURN_TOOL_OUTPUT_BYTES.saturating_sub(spent);
    for (child_run, child_session, title, parent_session, seen_through) in running {
        let child_run_id: RunId = parse_id(&child_run)?;
        let Some((report_turn, report)) = newest_report(
            transaction,
            report_spans(transaction, child_run_id, seen_through)?,
            |span| span.closed && !span.messages.is_empty(),
        )?
        else {
            continue;
        };
        let child_session_id: SessionId = parse_id(&child_session)?;
        let notice = interim_notice(
            child_session_id,
            &title,
            report,
            boundary_remaining.clamp(
                crate::tools::output::MIN_MODEL_TEXT_BYTES,
                MAX_DELIVERED_ANSWER_BYTES,
            ),
        );
        boundary_remaining = boundary_remaining.saturating_sub(notice.len());
        let ordinal = match next_ordinal {
            Some(ordinal) => ordinal,
            None => next_delivery_ordinal(transaction, &parent_run)?,
        };
        transaction.execute(
            "INSERT INTO child_reports(child_run_id, child_turn_ordinal, parent_session_id,
                                       parent_run_id, delivered_at_ms, delivery_ordinal,
                                       turn_ordinal, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                child_run,
                report_turn,
                parent_session,
                parent_run,
                now,
                ordinal,
                turn_ordinal,
                notice
            ],
        )?;
        next_ordinal = Some(ordinal.saturating_add(1));
        delivered.push(DeliveredAnswer {
            child_run_id,
            child_session_id,
            notice,
            is_error: false,
            spend: SpawnAgentSpend::NONE,
            interim: true,
        });
    }
    Ok(delivered)
}

pub(super) fn deliver_to_settled_parent(
    transaction: &Connection,
    settled_run_id: RunId,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let mut run = settled_run_id.to_string();
    for _ in 0..=MAX_CHILD_DEPTH {
        let owner = transaction
            .query_row(
                "SELECT s.owner_run_id, o.outcome_json IS NOT NULL
                 FROM runs r
                 JOIN sessions s ON s.id = r.session_id
                 JOIN runs o ON o.id = s.owner_run_id
                 WHERE r.id = ?1",
                [&run],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?;
        let Some((owner, owner_settled)) = owner else {
            return Ok(());
        };
        if owner_settled {
            deliver_settled_children(
                transaction,
                parse_id(&owner)?,
                None,
                usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
                now,
            )?;
        }
        run = owner;
    }
    Ok(())
}

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

pub(super) type DeliveredNotices =
    HashMap<String, std::collections::VecDeque<(Option<u32>, String)>>;

pub(super) fn retained_deliveries(
    transaction: &Connection,
    session: &str,
    through_ordinal: u64,
    cutoff_ordinal: u64,
) -> Result<DeliveredNotices, SessionRuntimeError> {
    let mut statement = transaction.prepare_cached(
        "SELECT d.parent_run_id, d.turn_ordinal, d.text, d.delivery_ordinal
             FROM messages m
             JOIN child_deliveries d ON d.parent_run_id = m.run_id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
               AND d.delivered_at_ms IS NOT NULL
         UNION ALL
         SELECT c.parent_run_id, c.turn_ordinal, c.text, c.delivery_ordinal
             FROM messages m
             JOIN child_reports c ON c.parent_run_id = m.run_id
             WHERE m.session_id = ?1 AND m.ordinal <= ?2 AND m.ordinal > ?3
               AND m.role = 'user' AND m.steering = 0
               AND m.state IN ('complete', 'cancelled', 'failed', 'interrupted')
         ORDER BY 1, 4",
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
