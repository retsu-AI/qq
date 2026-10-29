use super::{
    execution::{ModelTurnCommit, RunAccountingAccumulator, cancellation_requested},
    runtime::SessionRuntimeInner,
    store::Priority,
    *,
};
use crate::plan::CompiledAgentPlan;

pub(super) struct SessionInRunCompactor {
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    loaded: Arc<CompiledAgentPlan>,
    resolved_model: Arc<ResolvedModel>,
}

impl SessionInRunCompactor {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: &ClaimedRun,
        loaded: &LoadedRuntime,
    ) -> Self {
        Self {
            inner,
            claimed: claimed.panic_settlement_claim(),
            loaded: Arc::clone(&loaded.plan),
            resolved_model: Arc::clone(loaded.resolved_model()),
        }
    }
}

impl crate::runtime::InRunCompactor for SessionInRunCompactor {
    fn compact(
        &self,
        request: crate::runtime::InRunCompactionRequest,
    ) -> crate::runtime::InRunCompactionFuture {
        let inner = Arc::clone(&self.inner);
        let claimed = self.claimed.clone();
        let plan = Arc::clone(&self.loaded);
        let resolved_model = Arc::clone(&self.resolved_model);
        Box::pin(
            async move { compact_in_run(&inner, &claimed, &plan, &resolved_model, request).await },
        )
    }
}

async fn compact_in_run(
    inner: &Arc<SessionRuntimeInner>,
    prompt_run: &ClaimedRun,
    plan: &Arc<CompiledAgentPlan>,
    resolved_model: &Arc<ResolvedModel>,
    request: crate::runtime::InRunCompactionRequest,
) -> Result<crate::runtime::InRunCompaction, crate::runtime::InRunCompactionError> {
    use crate::runtime::InRunCompactionError as Error;
    if *inner.failed.borrow() {
        return Err(Error::Unavailable("session runtime failed".to_owned()));
    }
    let now = tokio::time::Instant::now();
    let allowance = match request.budget.limits_at(now) {
        Ok(allowance) => allowance,
        Err(kind) => {
            return Err(Error::BudgetExhausted {
                kind,
                usage: Some(TokenUsage::default()),
                estimated_cost_usd_nanos: Some(0),
            });
        }
    };
    let mut budget =
        crate::runtime::BudgetMeter::new(allowance, resolved_model.pricing.clone(), now);
    let basis = context_occupancy_basis(
        qq_protocol::ContentHash::from_bytes([0; 32]),
        crate::runtime::PreparedStaticPrefix::new(
            qq_protocol::ContentHash::from_bytes([0; 32]),
            None,
        ),
        0,
    );
    let mut observed_accounting =
        RunAccountingAccumulator::new(resolved_model.pricing.clone(), basis);
    let turn_cutoff = request.turn_cutoff;
    let retained_bytes = request.retained_bytes;
    let goal_notice = request.goal_notice;
    let started = inner
        .store
        .start_in_run_compaction_target(
            prompt_run,
            resolved_model,
            turn_cutoff,
            request.inherited_prefix,
        )
        .await;
    let (mut compaction, started_event) = match started {
        Ok(Some(started)) => started,
        Ok(None) => {
            return Err(Error::Unavailable(
                "the prompt run is no longer running".to_owned(),
            ));
        }
        Err(error) => return Err(Error::Persistence(error.to_string())),
    };
    let mut messages = if request.inherited_prefix {
        std::mem::take(&mut compaction.messages)
    } else {
        request.transcript
    };
    inner.notify(started_event.cursor);
    let (cancel, mut cancelled) = watch::channel(false);
    match inner.cancellations.lock() {
        Ok(mut cancellations) => {
            cancellations.insert(compaction.identity.run_id, cancel);
        }
        Err(_) => {
            inner.failed.send_replace(true);
            return Err(Error::Unavailable(
                "run cancellation registry is unavailable".to_owned(),
            ));
        }
    }
    let mut guard = SettleOnDrop::new(Arc::clone(inner), compaction.clone());
    match cancellation_requested(inner, compaction.identity.run_id).await {
        Ok(true) => {
            guard.disarm();
            settle_cancelled(inner, &compaction).await;
            return Err(Error::Unavailable("cancelled".to_owned()));
        }
        Ok(false) => {}
        Err(error) => {
            guard.disarm();
            settle_failed_with(
                inner,
                &compaction,
                RunFailure {
                    kind: RunFailureKind::Server,
                    message: format!("failed to read the cancellation request: {error}"),
                },
            )
            .await;
            return Err(Error::Persistence(error.to_string()));
        }
    }
    messages.push(Message::user(if request.inherited_prefix {
        COMPACTION_INSTRUCTION.to_owned()
    } else {
        format!("{IN_RUN_COMPACTION_INSTRUCTION_PREFIX}\n\n{COMPACTION_INSTRUCTION}")
    }));
    let paid =
        super::GoalPaidInput::new(&inner.store, &compaction, resolved_model.pricing.as_ref());
    let ceiling = super::context::summarizer_output_tokens(
        resolved_model.max_output_tokens,
        resolved_model.context_window,
    );
    let paid_output_cap = paid
        .as_ref()
        .map(|_| Arc::new(std::sync::atomic::AtomicU32::new(ceiling)));
    let summarize = plan.runtime.summarize(
        messages,
        request.system,
        request.tools,
        Some(Arc::from(prompt_run.identity.session_id.to_string())),
        ceiling,
        paid_output_cap.clone(),
    );
    tokio::pin!(summarize);
    let reply = loop {
        let step = tokio::select! {
            biased;
            changed = cancelled.changed() => {
                guard.disarm();
                if changed.is_ok() && *cancelled.borrow() {
                    settle_cancelled(inner, &compaction).await;
                    return Err(Error::Unavailable("cancelled".to_owned()));
                }
                settle_failed(inner, &compaction, "cancellation channel closed".to_owned()).await;
                return Err(Error::Unavailable("cancellation channel closed".to_owned()));
            }
            step = summarize.next() => step,
        };
        match step {
            Some(Ok(crate::runtime::SummarizeStep::RequestStarting { input_tokens })) => {
                let now = tokio::time::Instant::now();
                let mut refusal = request
                    .budget
                    .limits_at(now)
                    .and_then(|_| budget.child_budget(now))
                    .err();
                if refusal.is_none()
                    && let Some(paid) = paid.as_ref()
                {
                    let Some(input_tokens) = input_tokens else {
                        return Err(Error::Persistence(
                            "goal summary request is missing its input estimate".to_owned(),
                        ));
                    };
                    let mut proposed = ceiling;
                    loop {
                        let now = tokio::time::Instant::now();
                        let mut caller = match budget.child_budget(now) {
                            Ok(caller) => caller,
                            Err(kind) => {
                                refusal = Some(kind);
                                break;
                            }
                        };
                        caller.deadline = match (caller.deadline, request.budget.deadline) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                        let deadline = async {
                            match caller.deadline {
                                Some(deadline) => tokio::time::sleep_until(deadline).await,
                                None => std::future::pending::<()>().await,
                            }
                        };
                        let admission = tokio::select! {
                            biased;
                            _ = cancelled.changed()=>None,
                            () = deadline=>Some(Ok(super::goal_paid::PaidRequestAdmission::Exhausted(
                                budget.exhaustion(qq_protocol::BudgetLimitKind::Duration,false,tokio::time::Instant::now())))),
                            admission=paid.admit_request(proposed,input_tokens,caller)=>Some(admission),
                        };
                        match admission {
                            None | Some(Ok(super::goal_paid::PaidRequestAdmission::Cancelled)) => {
                                guard.disarm();
                                settle_cancelled(inner, &compaction).await;
                                return Err(Error::Unavailable("cancelled".to_owned()));
                            }
                            Some(Err(error)) => {
                                inner.failed.send_replace(true);
                                return Err(Error::Persistence(error.to_string()));
                            }
                            Some(Ok(super::goal_paid::PaidRequestAdmission::Reprepare {
                                max_output_tokens,
                            })) => {
                                proposed = max_output_tokens;
                            }
                            Some(Ok(super::goal_paid::PaidRequestAdmission::Allowed {
                                ..
                            })) => {
                                paid_output_cap
                                    .as_ref()
                                    .expect("paid summary has a cap")
                                    .store(proposed, std::sync::atomic::Ordering::Release);
                                break;
                            }
                            Some(Ok(super::goal_paid::PaidRequestAdmission::Exhausted(
                                exhaustion,
                            ))) => {
                                refusal = Some(exhaustion.limit);
                                break;
                            }
                        }
                    }
                }
                if let Some(kind) = refusal {
                    let mut accounting = observed_accounting.snapshot();
                    let exhaustion = budget.exhaustion(kind, false, tokio::time::Instant::now());
                    guard.disarm();
                    match inner
                        .store
                        .finish_in_run_compaction_failed(
                            &compaction,
                            RunOutcome::BudgetExhausted {
                                exhaustion: Box::new(exhaustion),
                            },
                        )
                        .await
                    {
                        Ok(events) => {
                            for event in events {
                                inner.notify(event.cursor);
                            }
                        }
                        Err(error) => {
                            inner.failed.send_replace(true);
                            return Err(Error::Persistence(error.to_string()));
                        }
                    }
                    if paid.is_some() {
                        let run = compaction.identity.run_id;
                        let settled=inner.store.call(Priority::Control,move |connection| {
                            let (usage,cost):(Option<String>,Option<u64>)=connection.query_row(
                                "SELECT usage_json,estimated_cost_usd_nanos FROM runs WHERE id=?1 AND outcome_json IS NOT NULL",
                                [run.to_string()],|row|Ok((row.get(0)?,row.get(1)?)))?;
                            let usage=usage.as_deref().map(serde_json::from_str::<TokenUsage>).transpose()?;
                            Ok((usage,cost))
                        }).await;
                        match settled {
                            Ok((usage, cost)) => {
                                accounting.usage = usage;
                                accounting.estimated_cost_usd_nanos = cost;
                            }
                            Err(error) => {
                                inner.failed.send_replace(true);
                                return Err(Error::Persistence(error.to_string()));
                            }
                        }
                    }
                    return Err(Error::BudgetExhausted {
                        kind,
                        usage: accounting.usage,
                        estimated_cost_usd_nanos: accounting.estimated_cost_usd_nanos,
                    });
                }
                if paid.is_none()
                    && let Err(error) = inner.store.start_in_run_summary_request(&compaction).await
                {
                    inner.failed.send_replace(true);
                    return Err(Error::Persistence(error.to_string()));
                }
            }
            Some(Ok(crate::runtime::SummarizeStep::TurnCompleted {
                model_work,
                ordinal,
                message,
                usage,
                truncated,
            })) => {
                observed_accounting.record_turn(usage);
                let accounting = observed_accounting.snapshot();
                let cost = usage.and_then(|usage| {
                    resolved_model
                        .pricing
                        .as_ref()
                        .and_then(|price| run_cost(usage, price))
                });
                budget.charge_child(usage, cost);
                let persisted = inner
                    .store
                    .persist_in_run_summary_turn(
                        &compaction,
                        ModelTurnCommit {
                            model_work,
                            goal_notices: Vec::new(),
                            reserved_goal_notices: 0,
                            turn_ordinal: ordinal,
                            message,
                            calls: Vec::new(),
                            turn_message: None,
                            context_tokens: accounting.context_tokens,
                            occupancy_basis: None,
                            usage,
                            estimated_cost_usd_nanos: cost,
                            accounting: Some(accounting),
                            truncated,
                            notice: None,
                        },
                    )
                    .await;
                match persisted {
                    Ok(events) => {
                        for event in events {
                            inner.notify(event.cursor);
                        }
                    }
                    Err(error) => {
                        inner.failed.send_replace(true);
                        return Err(Error::Persistence(error.to_string()));
                    }
                }
            }
            Some(Ok(crate::runtime::SummarizeStep::Summary(summary))) => break Ok(summary),
            Some(Err(error)) => break Err(error),
            None => {
                break Err(crate::runtime::SummarizeError::Rejected(
                    "summarizer ended without a summary".to_owned(),
                ));
            }
        }
    };
    guard.disarm();
    let summary = match reply {
        Ok(reply) => reply,
        Err(crate::runtime::SummarizeError::RejectedBeforeWork {
            kind,
            message,
            retry_after,
        }) => {
            let mut accounting = observed_accounting.snapshot();
            if !accounting.saw_turn {
                accounting.usage = Some(TokenUsage::default());
                accounting.estimated_cost_usd_nanos = Some(0);
            }
            let usage = accounting.usage;
            let estimated_cost_usd_nanos = accounting.estimated_cost_usd_nanos;
            let outcome = RunOutcome::Failed {
                failure: RunFailure {
                    kind,
                    message: truncate_utf8(message.clone(), MAX_FAILURE_MESSAGE_BYTES),
                },
            };
            match inner
                .store
                .finish_rejected_in_run_compaction(&compaction, outcome, accounting)
                .await
            {
                Ok(events) => {
                    for event in events {
                        inner.notify(event.cursor);
                    }
                }
                Err(error) => {
                    inner.failed.send_replace(true);
                    return Err(Error::Persistence(error.to_string()));
                }
            }
            return Err(if crate::recoverable_turn_fault(kind) {
                Error::SummarizerUnavailable {
                    kind,
                    message,
                    retry_after,
                    usage,
                    estimated_cost_usd_nanos,
                }
            } else {
                Error::SummarizerFailed(message)
            });
        }
        Err(crate::runtime::SummarizeError::Transient {
            kind,
            message,
            retry_after,
        }) => {
            settle_failed_with(
                inner,
                &compaction,
                RunFailure {
                    kind,
                    message: message.clone(),
                },
            )
            .await;
            return Err(Error::SummarizerUnavailable {
                kind,
                message,
                retry_after,
                usage: None,
                estimated_cost_usd_nanos: None,
            });
        }
        Err(crate::runtime::SummarizeError::Rejected(message)) => {
            settle_failed(inner, &compaction, message.clone()).await;
            return Err(Error::SummarizerFailed(message));
        }
    };
    let accounting = observed_accounting.snapshot();
    let usage = accounting.usage;
    let estimated_cost_usd_nanos = accounting.estimated_cost_usd_nanos;
    let committed = inner
        .store
        .finish_in_run_compaction(
            &compaction,
            summary,
            Some(accounting),
            record_budget(resolved_model.context_window),
            retained_bytes,
            goal_notice,
        )
        .await;
    match committed {
        Ok((events, Some(summary))) => {
            for event in events {
                inner.notify(event.cursor);
            }
            Ok(crate::runtime::InRunCompaction {
                summary,
                usage,
                estimated_cost_usd_nanos,
            })
        }
        Ok((events, None)) => {
            let reason = events
                .iter()
                .find_map(|event| match &event.event {
                    SessionEvent::RunFinished {
                        outcome: RunOutcome::Failed { failure },
                        ..
                    } => Some(failure.message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    "the summary was rejected or did not shrink the context".to_owned()
                });
            for event in events {
                inner.notify(event.cursor);
            }
            Err(Error::SummarizerFailed(reason))
        }
        Err(error) => {
            inner.failed.send_replace(true);
            Err(Error::Persistence(error.to_string()))
        }
    }
}

struct SettleOnDrop {
    inner: Arc<SessionRuntimeInner>,
    run_id: RunId,
    armed: Option<ClaimedRun>,
}

impl SettleOnDrop {
    fn new(inner: Arc<SessionRuntimeInner>, compaction: ClaimedRun) -> Self {
        Self {
            inner,
            run_id: compaction.identity.run_id,
            armed: Some(compaction),
        }
    }

    fn disarm(&mut self) {
        self.armed = None;
    }
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if let Ok(mut cancellations) = self.inner.cancellations.lock() {
            cancellations.remove(&self.run_id);
        }
        if let Some(compaction) = self.armed.take() {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                settle_cancelled(&inner, &compaction).await;
            });
        }
    }
}

#[cfg(test)]
struct DropSettlementHook {
    run_id: RunId,
    entered: tokio::sync::oneshot::Sender<()>,
    held: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
static DROP_SETTLEMENT_HOOKS: Mutex<Vec<DropSettlementHook>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn hold_drop_settlement(
    run_id: RunId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    DROP_SETTLEMENT_HOOKS
        .lock()
        .unwrap()
        .push(DropSettlementHook {
            run_id,
            entered,
            held,
        });
    (observed, release)
}

async fn settle_cancelled(inner: &Arc<SessionRuntimeInner>, compaction: &ClaimedRun) {
    #[cfg(test)]
    {
        let hook = {
            let mut hooks = DROP_SETTLEMENT_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.run_id == compaction.identity.run_id)
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.held.await;
        }
    }
    match inner
        .store
        .finish_in_run_compaction_failed(compaction, RunOutcome::Cancelled)
        .await
    {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor);
            }
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
}

async fn settle_failed(inner: &Arc<SessionRuntimeInner>, compaction: &ClaimedRun, message: String) {
    settle_failed_with(
        inner,
        compaction,
        RunFailure {
            kind: RunFailureKind::ProviderResponse,
            message,
        },
    )
    .await;
}

async fn settle_failed_with(
    inner: &Arc<SessionRuntimeInner>,
    compaction: &ClaimedRun,
    failure: RunFailure,
) {
    let outcome = RunOutcome::Failed {
        failure: RunFailure {
            kind: failure.kind,
            message: truncate_utf8(failure.message, MAX_FAILURE_MESSAGE_BYTES),
        },
    };
    match inner
        .store
        .finish_in_run_compaction_failed(compaction, outcome)
        .await
    {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor);
            }
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
}

const IN_RUN_COMPACTION_INSTRUCTION_PREFIX: &str = "The task above is still in progress. The \
messages after its prompt (the last user message before your work began) are your own earlier \
work on it; summarize that work so it can replace those messages while you continue. Earlier \
session context before that prompt stays as it is; do not summarize it. Record what was done, what each tool \
returned that still matters, and what remains. The task prompt stays verbatim above the \
summary.";
