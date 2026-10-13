//! In-run compaction: the session layer's side of summarizing a running
//! prompt's own earlier turns so the run continues in one window.
//!
//! The run loop asks (`InRunCompactor::compact`) at a safe boundary with the
//! exact messages to replace. This module runs the summarizer as an internal
//! `RunKind::Compaction` run — a durable row with its own usage, cost, and
//! `RunStarted`/`RunFinished`/`SessionCompacted` events, exactly like a
//! between-run step — but without taking the session's active-run slot,
//! which the prompt run holds. The marker it commits is scoped to the prompt
//! run (`session_compactions.scope_run_id`, `turn_cutoff`), so replay renders
//! the summary where the replaced turns stood and the between-run cutoff is
//! untouched. The summarizer request is provider-direct: no steering, no
//! audit. It declares the run's tools and system prompt only so it shares the
//! run's provider cache; a call is rejected, never run (ADR-0056 § 5).

use super::{
    execution::{ModelTurnCommit, RunAccountingAccumulator, cancellation_requested},
    runtime::SessionRuntimeInner,
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
    let started = inner
        .store
        .start_in_run_compaction(prompt_run, resolved_model, turn_cutoff)
        .await;
    let (compaction, started_event) = match started {
        Ok(Some(started)) => started,
        Ok(None) => {
            return Err(Error::Unavailable(
                "the prompt run is no longer running".to_owned(),
            ));
        }
        Err(error) => return Err(Error::Persistence(error.to_string())),
    };
    inner.notify(started_event.cursor);
    // Register for cancellation like any run, so a cancel of the prompt run
    // (which cascades here) or of this run directly stops the summarizer.
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
    // If the prompt run's task is torn down while this future is pending
    // (cancel, deadline, runtime failure drop the stream that awaits us),
    // the compaction row must still settle: the guard spawns that on drop.
    // It is disarmed on every path that settles explicitly below.
    let mut guard = SettleOnDrop::new(Arc::clone(inner), compaction.clone());
    // A cancel recorded between the start transaction and the registration
    // above would otherwise be missed.
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
    // One provider turn: the run's transcript through the cutoff, then the
    // instruction. The summarizer sees the prompt so it knows the task, and
    // is told the summary replaces the model's own work, not the user's.
    let mut messages = request.transcript;
    messages.push(Message::user(format!(
        "{IN_RUN_COMPACTION_INSTRUCTION_PREFIX}\n\n{COMPACTION_INSTRUCTION}"
    )));
    let summarize = plan.runtime.summarize(
        messages,
        request.system,
        request.tools,
        super::context::summarizer_output_tokens(
            resolved_model.max_output_tokens,
            resolved_model.context_window,
        ),
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
            Some(Ok(crate::runtime::SummarizeStep::RequestStarting)) => {
                let now = tokio::time::Instant::now();
                if let Err(kind) = request
                    .budget
                    .limits_at(now)
                    .and_then(|_| budget.remaining(now))
                {
                    let accounting = observed_accounting.snapshot();
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
                    return Err(Error::BudgetExhausted {
                        kind,
                        usage: accounting.usage,
                        estimated_cost_usd_nanos: accounting.estimated_cost_usd_nanos,
                    });
                }
                if let Err(error) = inner.store.start_in_run_summary_request(&compaction).await {
                    inner.failed.send_replace(true);
                    return Err(Error::Persistence(error.to_string()));
                }
            }
            Some(Ok(crate::runtime::SummarizeStep::TurnCompleted {
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
        // The compaction run is settled before the loop's backoff, so a retry
        // never stacks a second running row; if that write fails the runtime
        // is marked failed and the next attempt returns `Unavailable`.
        Err(crate::runtime::SummarizeError::Transient { kind, message }) => {
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
        )
        .await;
    match committed {
        // The splice uses the stored text, so the live request and replay
        // render the same bytes.
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
            for event in events {
                inner.notify(event.cursor);
            }
            Err(Error::SummarizerFailed(
                "the summary was rejected or did not shrink the context".to_owned(),
            ))
        }
        Err(error) => {
            inner.failed.send_replace(true);
            Err(Error::Persistence(error.to_string()))
        }
    }
}

/// Settles the compaction run `Cancelled` if the awaiting task is dropped
/// before an explicit settlement, and unregisters its cancellation sender
/// either way. The settlement is spawned: `Drop` cannot await, and the store
/// call must complete even though the dropping task is gone.
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

async fn settle_cancelled(inner: &Arc<SessionRuntimeInner>, compaction: &ClaimedRun) {
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

/// Prepended to the standard instruction so the summarizer knows the summary
/// replaces the assistant's own earlier turns of a task still in progress.
const IN_RUN_COMPACTION_INSTRUCTION_PREFIX: &str = "The task above is still in progress. The \
messages after its prompt (the last user message before your work began) are your own earlier \
work on it; summarize that work so it can replace those messages while you continue. Earlier \
session context before that prompt stays as it is; do not summarize it. Record what was done, what each tool \
returned that still matters, and what remains. The task prompt stays verbatim above the \
summary.";
