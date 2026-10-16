//! Current paid-request allowances; this input carries no goal-writing authority.
use super::*;

#[derive(Clone)]
pub(crate) struct GoalPaidInput {
    store: Store,
    identity: RunIdentity,
    admitted_identity: Arc<std::sync::OnceLock<RunIdentity>>,
    expected_owner: Option<RunId>,
    pricing: Option<Arc<qq_protocol::ModelPricing>>,
}

pub(crate) enum PaidRequestAdmission {
    Allowed { cursor: Option<EventCursor> },
    Reprepare { max_output_tokens: u32 },
    Cancelled,
    Exhausted(qq_protocol::BudgetExhaustion),
}

impl GoalPaidInput {
    pub(super) fn new(
        store: &Store,
        claimed: &ClaimedRun,
        pricing: Option<&qq_protocol::ModelPricing>,
    ) -> Option<Self> {
        if claimed.goal.is_none() && claimed.goal_spend_owner.is_none() {
            return None;
        }
        let mut identity = claimed.identity;
        let expected_owner = match (identity.kind, claimed.origin) {
            (
                RunKind::Compaction,
                Some(qq_protocol::RunOrigin::AutoCompaction { owner_run_id }),
            ) => {
                identity.run_id = owner_run_id;
                identity.kind = RunKind::Prompt;
                Some(owner_run_id)
            }
            _ => None,
        };
        let admitted_identity = Arc::new(std::sync::OnceLock::new());
        if expected_owner.is_none() {
            admitted_identity.get_or_init(|| claimed.identity);
        }
        Some(Self {
            store: store.clone(),
            identity,
            admitted_identity,
            expected_owner,
            pricing: pricing.cloned().map(Arc::new),
        })
    }

    pub(super) fn bind_run(&self, identity: RunIdentity) -> Result<(), SessionRuntimeError> {
        if identity.workspace_id != self.identity.workspace_id
            || identity.session_id != self.identity.session_id
            || (self.expected_owner.is_some() && identity.kind != RunKind::Compaction)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        if let Some(stored) = self.admitted_identity.get() {
            return if stored.run_id == identity.run_id {
                Ok(())
            } else {
                Err(SessionRuntimeError::CONSTRAINT)
            };
        }
        self.admitted_identity
            .set(identity)
            .map_err(|_| SessionRuntimeError::CONSTRAINT)
    }

    pub(crate) async fn preview_request(
        &self,
        ceiling: u32,
        input_tokens: u64,
    ) -> Result<PaidRequestAdmission, SessionRuntimeError> {
        let identity = self.identity;
        let pricing = self.pricing.clone();
        self.store
            .call_after_group(Priority::AwaitControl, move |connection| {
                request_allowance(
                    connection,
                    identity,
                    ceiling,
                    input_tokens,
                    pricing.as_deref(),
                )
            })
            .await
    }

    pub(crate) async fn admit_request(
        &self,
        proposed: u32,
        input_tokens: u64,
    ) -> Result<PaidRequestAdmission, SessionRuntimeError> {
        let identity = *self
            .admitted_identity
            .get()
            .ok_or(SessionRuntimeError::CONSTRAINT)?;
        let expected_owner = self.expected_owner;
        let store_id = self.store.store_id();
        let pricing = self.pricing.clone();
        self.store.call_write(Priority::AwaitControl, move |connection| {
            let transaction = store::begin_unit(connection)?;
            if let Some(expected_owner) = expected_owner {
                let owner: Option<String> = transaction.query_row(
                    "SELECT auto_compaction_for_run_id FROM runs WHERE id=?1 AND session_id=?2 AND kind='compaction' AND auto_compaction=1",
                    params![identity.run_id.to_string(), identity.session_id.to_string()], |row| row.get(0),
                )?;
                if owner.as_deref() != Some(expected_owner.to_string().as_str()) { return Err(SessionRuntimeError::CONSTRAINT); }
            }
            let allowed = request_allowance(&transaction, identity, proposed, input_tokens, pricing.as_deref())?;
            match allowed {
                PaidRequestAdmission::Reprepare { max_output_tokens } if max_output_tokens == proposed => {}
                other => return Ok(other),
            }
            let running: bool = transaction.query_row(
                "SELECT status='running' AND cancel_requested=0 AND outcome_json IS NULL FROM runs WHERE id=?1 AND session_id=?2",
                params![identity.run_id.to_string(), identity.session_id.to_string()], |row| row.get(0),
            )?;
            if !running { return Ok(PaidRequestAdmission::Cancelled); }
            let updated = transaction.execute(
                "UPDATE runs SET goal_request_pending=1, usage_json=NULL, estimated_cost_usd_nanos=NULL,
                    goal_helper_admitted=CASE WHEN kind='compaction' THEN 1 ELSE goal_helper_admitted END,
                    activity=CASE WHEN kind='prompt' THEN 'waiting_for_provider' ELSE activity END
                 WHERE id=?1 AND session_id=?2 AND status='running' AND cancel_requested=0 AND outcome_json IS NULL",
                params![identity.run_id.to_string(), identity.session_id.to_string()],
            )?;
            if updated != 1 { return Err(SessionRuntimeError::CONSTRAINT); }
            let cursor = if identity.kind == RunKind::Prompt {
                Some(append_event(&transaction, EventContext::for_run(store_id, identity, now_ms()),
                    SessionEvent::RunActivityChanged { run_id: identity.run_id, activity: qq_protocol::RunActivity::WaitingForProvider })?.cursor)
            } else { None };
            transaction.commit()?;
            Ok(PaidRequestAdmission::Allowed { cursor })
        }).await
    }
}

fn request_allowance(
    connection: &Connection,
    identity: RunIdentity,
    ceiling: u32,
    input_tokens: u64,
    pricing: Option<&qq_protocol::ModelPricing>,
) -> Result<PaidRequestAdmission, SessionRuntimeError> {
    let (base, cancelled): (Option<String>, bool) = connection.query_row(
        "SELECT CASE WHEN kind='prompt' THEN id ELSE auto_compaction_for_run_id END,
            cancel_requested OR outcome_json IS NOT NULL FROM runs WHERE id=?1 AND session_id=?2",
        params![identity.run_id.to_string(), identity.session_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if cancelled {
        return Ok(PaidRequestAdmission::Cancelled);
    }
    let base: RunId = parse_id(base.as_deref().ok_or(SessionRuntimeError::CONSTRAINT)?)?;
    let mut cap = ceiling;
    let mut seen = Vec::new();
    for (_, session, goal_id) in goals::paid_goal_owners(connection, base)? {
        let Some(goal_id) = goal_id else {
            continue;
        };
        if seen.contains(&goal_id) {
            continue;
        }
        seen.push(goal_id);
        let (payload, live): (String, bool) = connection.query_row(
            "SELECT payload_json,live FROM session_goals WHERE goal_id=?1 AND session_id=?2",
            params![goal_id.to_string(), session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let goal: qq_protocol::GoalSnapshot = serde_json::from_str(&payload)?;
        if !live || goal.status != qq_protocol::GoalStatus::Active {
            return Ok(PaidRequestAdmission::Cancelled);
        }
        let exhausted = |limit: qq_protocol::BudgetLimitKind| {
            PaidRequestAdmission::Exhausted(qq_protocol::BudgetExhaustion {
                limit,
                final_response: false,
                message: format!(
                    "current goal allowance cannot admit a provider request: {}",
                    limit.as_str()
                ),
            })
        };
        if now_ms() >= goal.budget.deadline_ms {
            return Ok(exhausted(qq_protocol::BudgetLimitKind::Duration));
        }
        let mut tokens = goal.counters.usage.and_then(total_tokens);
        let mut cost = goal.counters.estimated_cost_usd_nanos;
        // Each direct run/helper aggregate is folded once. Terminal rows are already
        // in goal counters; inclusive owner receipts must never be added again.
        if goal.budget.max_total_tokens.is_some() || goal.budget.max_cost_usd_nanos.is_some() {
            let mut statement = connection.prepare(&format!("{} SELECT r.usage_json,r.estimated_cost_usd_nanos,
            r.goal_request_pending OR EXISTS(SELECT 1 FROM model_turns WHERE run_id=r.id AND work_disposition='uncertain')
            FROM obligations JOIN runs r ON r.id=obligations.id
            WHERE r.outcome_json IS NULL AND (r.goal_helper_admitted=1 OR r.goal_request_pending=1
              OR EXISTS(SELECT 1 FROM model_turns WHERE run_id=r.id AND work_disposition!='rejected_before_work'))", goals::PAID_WORK_CTE))?;
            let mut rows = statement.query(params![
                session.to_string(),
                goal_id.to_string(),
                MAX_CHILD_DEPTH
            ])?;
            while let Some(row) = rows.next()? {
                let usage: Option<String> = row.get(0)?;
                let row_cost: Option<u64> = row.get(1)?;
                let uncertain: bool = row.get(2)?;
                let usage: Option<TokenUsage> =
                    usage.as_deref().map(serde_json::from_str).transpose()?;
                tokens = if uncertain {
                    None
                } else {
                    tokens
                        .zip(usage.and_then(total_tokens))
                        .and_then(|(a, b)| a.checked_add(b))
                };
                cost = if uncertain {
                    None
                } else {
                    cost.zip(row_cost).and_then(|(a, b)| a.checked_add(b))
                };
            }
        }
        if let Some(limit) = goal.budget.max_total_tokens {
            let Some(spent) = tokens else {
                return Ok(exhausted(qq_protocol::BudgetLimitKind::TokensUnknown));
            };
            let remaining = limit.saturating_sub(spent).saturating_sub(input_tokens);
            if remaining == 0 {
                return Ok(exhausted(qq_protocol::BudgetLimitKind::TotalTokens));
            }
            cap = cap.min(u32::try_from(remaining).unwrap_or(u32::MAX));
        }
        if let Some(limit) = goal.budget.max_cost_usd_nanos {
            let (Some(spent), Some(pricing)) = (cost, pricing) else {
                return Ok(exhausted(qq_protocol::BudgetLimitKind::CostUnknown));
            };
            let input_rate =
                pricing
                    .context_tier
                    .as_ref()
                    .map_or(pricing.input_usd_nanos_per_token, |tier| {
                        tier.input_usd_nanos_per_token
                            .max(pricing.input_usd_nanos_per_token)
                    });
            let input_rate = input_rate
                .max(pricing.cache_read_usd_nanos_per_token.unwrap_or(input_rate))
                .max(
                    pricing
                        .cache_write_usd_nanos_per_token
                        .unwrap_or(input_rate),
                );
            let input_rate = pricing.context_tier.as_ref().map_or(input_rate, |tier| {
                input_rate
                    .max(tier.cache_read_usd_nanos_per_token.unwrap_or(input_rate))
                    .max(tier.cache_write_usd_nanos_per_token.unwrap_or(input_rate))
            });
            let output_rate =
                pricing
                    .context_tier
                    .as_ref()
                    .map_or(pricing.output_usd_nanos_per_token, |tier| {
                        tier.output_usd_nanos_per_token
                            .max(pricing.output_usd_nanos_per_token)
                    });
            let Some(input_cost) = input_tokens.checked_mul(input_rate) else {
                return Ok(exhausted(qq_protocol::BudgetLimitKind::Cost));
            };
            let remaining = limit.saturating_sub(spent).saturating_sub(input_cost);
            if spent >= limit || input_cost > limit.saturating_sub(spent) {
                return Ok(exhausted(qq_protocol::BudgetLimitKind::Cost));
            }
            if output_rate > 0 {
                cap = cap.min(u32::try_from(remaining / output_rate).unwrap_or(u32::MAX));
            }
        }
    }
    if cap == 0 {
        return Ok(PaidRequestAdmission::Exhausted(
            qq_protocol::BudgetExhaustion {
                limit: qq_protocol::BudgetLimitKind::Cost,
                final_response: false,
                message: "current goal allowance has no output reserve".to_owned(),
            },
        ));
    }
    Ok(PaidRequestAdmission::Reprepare {
        max_output_tokens: cap,
    })
}

fn total_tokens(usage: TokenUsage) -> Option<u64> {
    usage
        .input_tokens
        .checked_add(usage.cache_read_input_tokens)
        .and_then(|n| n.checked_add(usage.cache_write_input_tokens))
        .and_then(|n| n.checked_add(usage.output_tokens))
}
