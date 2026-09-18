use super::*;
use super::{
    approvals::SessionToolGate,
    runtime::SessionRuntimeInner,
    subagents::{
        ChildTasks, SessionAuditHook, SessionHistorySearcher, SessionSpillReader,
        SessionSubagentSpawner,
    },
};
use crate::runtime::RunDeadline;
use std::collections::HashSet;

/// Denies every tool call. Compaction runs summarize existing context; a
/// call the instruction forbade costs one denied round trip and persists
/// nothing.
struct CompactionRunGate;

#[cfg(test)]
struct BufferedToolOutputHook {
    tool_call_id: ToolCallId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static BUFFERED_TOOL_OUTPUT_HOOKS: Mutex<Vec<BufferedToolOutputHook>> = Mutex::new(Vec::new());

/// Holds the execution loop immediately after one call's live output enters
/// the bounded batch. Tests use this exact handoff to make cancellation-versus-
/// timer ordering deterministic without adding production sleeps or hooks.
#[cfg(test)]
pub(super) fn hold_buffered_tool_output(
    tool_call_id: ToolCallId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    BUFFERED_TOOL_OUTPUT_HOOKS
        .lock()
        .unwrap()
        .push(BufferedToolOutputHook {
            tool_call_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

#[cfg(test)]
async fn pause_after_buffering_tool_output(tool_call_id: ToolCallId) {
    let hook = {
        let mut hooks = BUFFERED_TOOL_OUTPUT_HOOKS.lock().unwrap();
        hooks
            .iter()
            .position(|hook| hook.tool_call_id == tool_call_id)
            .map(|index| hooks.remove(index))
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.await;
    }
}

impl ToolGate for CompactionRunGate {
    fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
        Box::pin(std::future::ready(GateDecision::Deny {
            message: "Tools are unavailable during compaction; produce the summary directly."
                .to_owned(),
        }))
    }
}

const COMPACTION_OUTPUT_RESERVE_TOKENS: u32 = 2_048;

struct PreparedExecution {
    events: crate::RuntimeStream,
    audit: PreparedRunAudit,
    tool_cancellation: RunCancellation,
}

#[derive(Clone, Default)]
pub(super) struct RunResources {
    tools: crate::tools::ToolTasks,
    children: Arc<ChildTasks>,
    #[cfg(test)]
    run_id: Option<RunId>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ExecutionCleanupError {
    #[error("local tool cleanup failed: {0}")]
    Tools(#[source] crate::tools::ToolDrainError),
    #[error("child cleanup failed: {0}")]
    Children(#[source] crate::runtime::ChildCleanupError),
}

/// Proof that a run's tools and children were drained. The store requires it
/// to settle a started run, so a terminal event cannot be published while
/// execution is still in flight; only `RunResources::drain` / `stop` mint it.
#[derive(Clone, Copy, Debug)]
pub(super) struct TeardownComplete(());

impl TeardownComplete {
    /// Store-level tests settle claims that never ran anything.
    #[cfg(test)]
    pub(super) const fn nothing_ran() -> Self {
        Self(())
    }
}

#[cfg(test)]
static STOP_OBSERVERS: Mutex<Vec<(RunId, oneshot::Sender<()>)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn observe_execution_stop(run_id: RunId) -> oneshot::Receiver<()> {
    let (sender, receiver) = oneshot::channel();
    STOP_OBSERVERS.lock().unwrap().push((run_id, sender));
    receiver
}

impl RunResources {
    #[cfg(test)]
    pub(super) fn for_test_run(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }
    pub(super) async fn drain(&self) -> Result<TeardownComplete, ExecutionCleanupError> {
        let (tools, children) = tokio::join!(self.tools.drain(), self.children.drain());
        match (tools, children) {
            (Ok(()), Ok(_)) => Ok(TeardownComplete(())),
            (Err(error), _) => Err(ExecutionCleanupError::Tools(error)),
            (_, Err(error)) => Err(ExecutionCleanupError::Children(error)),
        }
    }

    async fn stop(
        &self,
        events: &mut crate::RuntimeStream,
    ) -> Result<TeardownComplete, ExecutionCleanupError> {
        *events = Box::pin(futures_util::stream::empty());
        #[cfg(test)]
        {
            let hook = {
                let mut observers = STOP_OBSERVERS.lock().unwrap();
                observers
                    .iter()
                    .position(|(run, _)| Some(*run) == self.run_id)
                    .map(|index| observers.remove(index))
            };
            if let Some((_, entered)) = hook {
                let _ = entered.send(());
            }
        }
        self.drain().await
    }
}

async fn prepare_execution(
    inner: &Arc<SessionRuntimeInner>,
    claimed: &mut ClaimedRun,
    loaded: &LoadedRuntime,
    cancellation: &mut watch::Receiver<bool>,
    resources: &RunResources,
    execution_started: tokio::time::Instant,
) -> Result<PreparedExecution, RunOutcome> {
    let identity = loaded.plan.descriptor().checkpoint.as_deref();
    if claimed
        .checkpoint
        .as_ref()
        .is_some_and(|selection| !selection.matches(identity))
    {
        return Err(RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Configuration,
                message: "child loader did not preserve the parent's inherited reviewer identity"
                    .to_owned(),
            },
        });
    }
    claimed.checkpoint = Some(CheckpointSelection::from_identity(identity));
    let deadline = RunDeadline::new(claimed.limits, execution_started);
    if let Some(deadline) = deadline.filter(|deadline| deadline.expired()) {
        return Err(RunOutcome::BudgetExhausted {
            exhaustion: Box::new(deadline.exhaustion()),
        });
    }
    let tool_cancellation = RunCancellation::new();
    let internal = claimed.identity.kind == RunKind::Compaction;
    // Take the only full transcript before cloning run metadata into gates or
    // spawners. ClaimedRun clones after this point stay scalar/empty instead
    // of duplicating up to 4 MiB per tool call.
    let mut messages = std::mem::take(&mut claimed.messages);
    let input = std::mem::take(&mut claimed.input);
    let gate: Arc<dyn ToolGate> = if internal {
        Arc::new(CompactionRunGate)
    } else {
        Arc::new(SessionToolGate::new(
            Arc::clone(inner),
            claimed.clone(),
            cancellation.clone(),
            loaded.plan.network_policy(),
        ))
    };
    // The claim carried the session's file hashes, so nothing here waits on
    // the store; a cancel already recorded settles before any preparation.
    if *cancellation.borrow() {
        tool_cancellation.cancel();
        return Err(RunOutcome::Cancelled);
    }
    let file_state = Arc::new(FileState::with_entries(std::mem::take(
        &mut claimed.file_state,
    )));
    // Structured input resolves here, before the first provider request:
    // file parts are read through the plan's workspace capability and
    // recorded in the session file state. The assembled context already ends
    // with the rendered prompt text (placeholders for attachments); the
    // resolved text replaces it. A missing, changed, or oversized attachment
    // fails the run with a typed outcome and no provider work. The result is
    // kept on the claim: a retry after automatic compaction reloads the
    // placeholder from the store and must send the bytes the first attempt
    // read, not a second read of a file that may have changed since.
    if !internal
        && (claimed.resolved_input.is_some()
            || input
                .iter()
                .any(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. })))
    {
        let resolved = if let Some(resolved) = &claimed.resolved_input {
            Arc::clone(resolved)
        } else {
            let workspace = loaded.plan.workspace_handle();
            let state = Arc::clone(&file_state);
            let parts = input;
            let mut resolution = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                crate::workspace::pause_blocking_preparation(&workspace);
                crate::input::resolve_blocking(&parts, &workspace, &state)
            });
            let resolved = tokio::select! {
                biased;
                () = RunDeadline::wait(deadline) => {
                    tool_cancellation.cancel();
                    // Blocking work cannot be forcibly cancelled or detached.
                    let _ = resolution.await;
                    return Err(RunOutcome::BudgetExhausted {
                        exhaustion: Box::new(deadline.expect("only a finite deadline wakes").exhaustion()),
                    });
                }
                changed = cancellation.changed() => {
                    tool_cancellation.cancel();
                    let _ = resolution.await;
                    return if changed.is_ok() && *cancellation.borrow() {
                        Err(RunOutcome::Cancelled)
                    } else {
                        Err(RunOutcome::Interrupted)
                    };
                }
                result = &mut resolution => result,
            };
            match resolved {
                Ok(Ok(resolved)) => {
                    let resolved = Arc::new(resolved);
                    claimed.resolved_input = Some(Arc::clone(&resolved));
                    resolved
                }
                Ok(Err(error)) => {
                    tool_cancellation.cancel();
                    return Err(RunOutcome::Failed {
                        failure: RunFailure {
                            kind: error.failure_kind(),
                            message: truncate_utf8(error.to_string(), MAX_FAILURE_MESSAGE_BYTES),
                        },
                    });
                }
                Err(_) => {
                    tool_cancellation.cancel();
                    return Err(internal_failure("input resolution stopped unexpectedly"));
                }
            }
        };
        let text = resolved.text.clone();
        match messages.pop() {
            Some(prompt) if prompt.role() == Role::User => messages.push(Message::user(text)),
            Some(_) | None => {
                return Err(internal_failure(
                    "assembled context did not end with the run's prompt",
                ));
            }
        }
    }
    let mut capabilities = if internal {
        RunCapabilities::restricted()
            .with_limits(
                RunLimits {
                    max_duration_ms: claimed.limits.max_duration_ms,
                    ..RunLimits::default()
                },
                None,
            )
            .without_tools()
            .with_max_output_tokens(
                loaded
                    .resolved_model()
                    .max_output_tokens
                    .min(COMPACTION_OUTPUT_RESERVE_TOKENS),
            )
    } else {
        // A hard cost cap without pricing cannot be enforced. Reject it
        // before any provider work rather than pretend, exactly as the
        // headless adapter did before core owned the contract.
        if claimed.limits.max_cost_usd_nanos.is_some() && loaded.resolved_model().pricing.is_none()
        {
            tool_cancellation.cancel();
            return Err(RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Configuration,
                    message: format!(
                        "a cost budget cannot be enforced: model {} has no configured pricing",
                        loaded.resolved_model().route
                    ),
                },
            });
        }
        // A run may delegate while its depth is below the roster's effective
        // maximum (itself capped at the runtime ceiling; zero disables
        // delegation for the whole tree). Only depth-one children may hold
        // write authority, so deeper spawners never offer it; grandchildren
        // are read-only by construction.
        let delegation = &loaded.plan.descriptor().delegation;
        let effective_depth = delegation.max_depth.min(MAX_CHILD_DEPTH);
        let spawner = if claimed.depth < effective_depth {
            Some(Arc::new(
                SessionSubagentSpawner::new(
                    Arc::clone(inner),
                    claimed.clone(),
                    Arc::clone(&resources.children),
                )
                .with_write_children(delegation.write_children && claimed.depth == 0),
            ) as Arc<dyn SubagentSpawner>)
        } else {
            None
        };
        let base = if !claimed.user_initiated {
            match spawner {
                Some(spawner) => RunCapabilities::restricted().with_spawner(spawner),
                None => RunCapabilities::restricted(),
            }
        } else {
            RunCapabilities::user(spawner)
        };
        // A read-only session (every read child) never sees the schemas its
        // policy denies; the catalog filter is part of the request, not a
        // gate-time refusal. A Supervised child keeps the full catalog: its
        // mutating calls are held, not denied.
        let base = if claimed.approval_mode == ApprovalMode::ReadOnly {
            base.read_only()
        } else {
            base
        };
        let (sender, receiver) = crate::runtime::steering_channel();
        // Steering recorded up to the claim (the run was already `running`
        // for admission purposes) rode the claim and is queued into the
        // channel now so the first boundary applies it. Steering recorded
        // after the claim reaches the channel through `steer`, which the
        // registration below makes possible.
        for message in std::mem::take(&mut claimed.pending_steering) {
            match sender.messages.try_send(message) {
                Ok(()) => {}
                // The channel and the durable bound are the same size.
                Err(_) => break,
            }
        }
        match inner.steering.lock() {
            Ok(mut steering) => {
                steering.insert(claimed.identity.run_id, sender);
            }
            Err(_) => {
                tool_cancellation.cancel();
                return Err(internal_failure("steering registry is poisoned"));
            }
        }
        let base = base
            .with_limits(claimed.limits, loaded.resolved_model().pricing.clone())
            .with_history(Arc::new(SessionHistorySearcher::new(
                Arc::clone(inner),
                claimed.identity.session_id,
                claimed.identity.run_id,
            )))
            .with_spills(Arc::new(SessionSpillReader::new(
                Arc::clone(inner),
                claimed.identity.session_id,
            )))
            .with_steering(receiver);
        // Only user-initiated roots are audited: children answer to their
        // parent, internal runs to the runtime, and an audit child auditing
        // itself would recurse.
        if claimed.depth == 0 && claimed.user_initiated && claimed.purpose == SessionPurpose::Task {
            base.with_audit_hook(Arc::new(SessionAuditHook::new(
                Arc::clone(inner),
                claimed.clone(),
                loaded.plan.descriptor().delegation.clone(),
                Arc::clone(&resources.children),
            )))
        } else {
            base
        }
    }
    .with_literal_slash(claimed.literal_slash)
    .with_execution_started(execution_started)
    .with_tool_tasks(resources.tools.clone())
    .with_output(claimed.output.clone());
    if !internal {
        capabilities.routing_spend = loaded.routing_spend;
    }
    // The claimed workspace is the plan's workspace: the loader compiled the
    // plan for exactly this session's canonical root, so no per-run
    // canonicalization or directory open happens here.
    let mut events = loaded.plan.execute(
        messages,
        tool_cancellation.clone(),
        gate,
        file_state,
        capabilities,
    );
    loop {
        let event = tokio::select! {
            biased;
            changed = cancellation.changed() => {
                tool_cancellation.cancel();
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
                }
                return if changed.is_ok() && *cancellation.borrow() {
                    Err(RunOutcome::Cancelled)
                } else {
                    Err(RunOutcome::Interrupted)
                };
            }
            event = events.next() => event,
        };
        match event {
            Some(RuntimeEvent::Started) => {}
            Some(RuntimeEvent::Prepared {
                turn_ordinal: 1,
                identity: Some(prompt_identity),
                static_prefix,
                mut weight,
            }) => {
                let mut resolved_model = loaded.resolved_model().as_ref().clone();
                // Internal compaction deliberately reserves a smaller output
                // budget. Its immutable descriptor records the effective cap
                // actually sent on every provider turn, not the model's
                // larger configured ceiling.
                resolved_model.max_output_tokens = weight.max_output_tokens;
                let context_shape = context_request_shape(&resolved_model);
                if claimed.identity.kind == RunKind::Prompt
                    && weight.compatible_input_tokens.is_none()
                {
                    weight.compatible_input_tokens =
                        claimed.context_occupancy.and_then(|occupancy| {
                            compatible_context_tokens(
                                occupancy,
                                context_shape,
                                static_prefix,
                                weight.input_bytes(),
                            )
                        });
                }
                return Ok(PreparedExecution {
                    events,
                    audit: PreparedRunAudit {
                        prompt_identity,
                        resolved_model: Arc::new(resolved_model),
                        plan_identity: loaded.plan.identity(),
                        plan_descriptor_json: Arc::clone(loaded.plan.descriptor_json()),
                        context_shape,
                        weight,
                        static_prefix,
                    },
                    tool_cancellation,
                });
            }
            Some(RuntimeEvent::Failed { kind, message }) => {
                return Err(RunOutcome::Failed {
                    failure: RunFailure {
                        kind,
                        message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
                    },
                });
            }
            Some(RuntimeEvent::BudgetExhausted { exhaustion }) => {
                return Err(RunOutcome::BudgetExhausted {
                    exhaustion: Box::new(exhaustion),
                });
            }
            Some(_) | None => {
                return Err(internal_failure(
                    "runtime preparation ended without an initial prepared request",
                ));
            }
        }
    }
}

async fn route_run(
    inner: &Arc<SessionRuntimeInner>,
    claimed: &ClaimedRun,
    loaded: &mut LoadedRuntime,
    cancellation: &mut watch::Receiver<bool>,
    started: tokio::time::Instant,
) -> Result<(), RunOutcome> {
    if claimed.identity.kind != RunKind::Prompt || claimed.purpose != SessionPurpose::Task {
        return Ok(());
    }
    let Some(router) = loaded.router.clone() else {
        return Ok(());
    };
    let deadline = RunDeadline::new(claimed.limits, started);
    let mut budget = crate::runtime::BudgetMeter::new(
        claimed.limits,
        loaded.resolved_model().pricing.clone(),
        started,
    );
    if let Err(kind) = budget.remaining(tokio::time::Instant::now()) {
        return Err(RunOutcome::BudgetExhausted {
            exhaustion: Box::new(budget.exhaustion(kind, false, tokio::time::Instant::now())),
        });
    }
    if let Some(limit) = claimed.limits.max_cost_usd_nanos {
        let kind = match router.max_cost_usd_nanos() {
            None => Some(qq_protocol::BudgetLimitKind::CostUnknown),
            Some(cost) if cost > limit => Some(qq_protocol::BudgetLimitKind::Cost),
            Some(_) => None,
        };
        if let Some(kind) = kind {
            return Err(RunOutcome::BudgetExhausted {
                exhaustion: Box::new(budget.exhaustion(kind, false, tokio::time::Instant::now())),
            });
        }
    }
    let fallback = ModelSelection {
        model: Some(loaded.resolved_model().route.clone()),
        max_output_tokens: Some(loaded.resolved_model().max_output_tokens),
        organization: loaded.resolved_model().organization.clone(),
    };
    let pinned_effort = loaded.plan.descriptor().reasoning_effort;
    let fallback_decision = |reason: &str, spent: bool| qq_protocol::RoutingDecision {
        model: fallback.clone(),
        reasoning_effort: pinned_effort,
        outcome: qq_protocol::RoutingOutcome::Fallback,
        reason: reason.to_owned(),
        usage: (!spent).then_some(TokenUsage::default()),
        estimated_cost_usd_nanos: (!spent).then_some(0),
    };
    let mut task = String::new();
    let mut oversized = false;
    if let Some(message) = claimed
        .messages
        .iter()
        .rev()
        .find(|message| message.role() == Role::User)
    {
        for block in message.content() {
            if let ContentBlock::Text { text } = block {
                if task.len().saturating_add(text.len()).saturating_add(1) > 16 * 1024 {
                    oversized = true;
                    break;
                }
                task.push_str(text);
                task.push('\n');
            }
        }
    }
    if *cancellation.borrow() {
        return Err(RunOutcome::Cancelled);
    }
    match inner.store.record_routing_started(claimed).await {
        Ok(Some(event)) => inner.notify(event.cursor),
        Ok(None) => return Err(RunOutcome::Cancelled),
        Err(error) => {
            return Err(persistence_failure(
                "failed to persist routing dispatch",
                &error,
            ));
        }
    }
    let mut decision = if oversized || task.is_empty() {
        fallback_decision(
            "task exceeds routing bounds or has no text; configured model retained",
            false,
        )
    } else {
        let task = crate::tools::output::mask_secrets(task);
        let response = tokio::time::timeout(Duration::from_secs(5), router.route(task));
        tokio::select! {
            biased;
            () = RunDeadline::wait(deadline) => return Err(RunOutcome::BudgetExhausted {
                exhaustion: Box::new(deadline.expect("finite deadline woke").exhaustion()),
            }),
            changed = cancellation.changed() => return Err(if changed.is_ok() && *cancellation.borrow() { RunOutcome::Cancelled } else { RunOutcome::Interrupted }),
            result = response => match result {
                Ok(decision) => decision,
                Err(_) => fallback_decision("routing timed out; configured model retained", true),
            },
        }
    };
    decision.reason = truncate_utf8(decision.reason, 1024);
    // A router may optimize omission, but an already pinned effort is authoritative.
    if pinned_effort.is_some() {
        decision.reasoning_effort = pinned_effort;
    }
    if decision.outcome == qq_protocol::RoutingOutcome::Fallback {
        decision.model = fallback.clone();
        decision.reasoning_effort = pinned_effort;
    } else {
        match inner
            .store
            .record_routing_spend(
                claimed,
                qq_protocol::CheckpointSpend {
                    usage: decision.usage,
                    estimated_cost_usd_nanos: decision.estimated_cost_usd_nanos,
                },
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(RunOutcome::Cancelled),
            Err(error) => {
                return Err(persistence_failure(
                    "failed to persist routing spend",
                    &error,
                ));
            }
        }
        decision.model.max_output_tokens = claimed.model.max_output_tokens;
        decision.model.organization = claimed.model.organization.clone();
        let mut load = inner.loader.load(RuntimeLoadRequest {
            reasoning_effort: decision.reasoning_effort,
            checkpoint: Some(CheckpointSelection::from_identity(
                loaded.plan.descriptor().checkpoint.as_deref(),
            )),
            workspace: claimed.workspace.clone(),
            model: decision.model.clone(),
            profile: claimed.profile.clone(),
        });
        let selected = tokio::select! {
            biased;
            () = RunDeadline::wait(deadline) => {
                let _ = load.await;
                return Err(RunOutcome::BudgetExhausted { exhaustion: Box::new(deadline.expect("finite deadline woke").exhaustion()) });
            },
            changed = cancellation.changed() => {
                let _ = load.await;
                return Err(if changed.is_ok() && *cancellation.borrow() { RunOutcome::Cancelled } else { RunOutcome::Interrupted });
            },
            result = &mut load => result,
        };
        match selected {
            Ok(selected)
                if selected.plan.workspace_path() == loaded.plan.workspace_path()
                    && Some(selected.resolved_model().route.as_str())
                        == decision.model.model.as_deref()
                    && selected.plan.descriptor().reasoning_effort == decision.reasoning_effort
                    && selected.plan.descriptor().checkpoint
                        == loaded.plan.descriptor().checkpoint
                    && selected.plan.descriptor().profile == loaded.plan.descriptor().profile =>
            {
                decision.model = ModelSelection {
                    model: Some(selected.resolved_model().route.clone()),
                    max_output_tokens: Some(selected.resolved_model().max_output_tokens),
                    organization: selected.resolved_model().organization.clone(),
                };
                *loaded = selected;
            }
            _ => {
                decision.model = fallback;
                decision.reasoning_effort = pinned_effort;
                decision.outcome = qq_protocol::RoutingOutcome::Fallback;
                decision.reason =
                    "selected route unavailable or incompatible; configured model retained"
                        .to_owned();
            }
        }
    }
    let spend = qq_protocol::CheckpointSpend {
        usage: decision.usage,
        estimated_cost_usd_nanos: decision.estimated_cost_usd_nanos,
    };
    match inner
        .store
        .record_routing_completed(claimed, decision)
        .await
    {
        Ok(Some(event)) => inner.notify(event.cursor),
        Ok(None) => return Err(RunOutcome::Cancelled),
        Err(error) => {
            return Err(persistence_failure(
                "failed to persist routing receipt",
                &error,
            ));
        }
    }
    loaded.routing_spend = Some(spend);
    budget.charge_child(spend.usage, spend.estimated_cost_usd_nanos);
    if let Err(kind) = budget.remaining(tokio::time::Instant::now()) {
        return Err(RunOutcome::BudgetExhausted {
            exhaustion: Box::new(budget.exhaustion(kind, false, tokio::time::Instant::now())),
        });
    }
    Ok(())
}

pub(super) async fn execute_run(
    inner: Arc<SessionRuntimeInner>,
    mut claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    resources: RunResources,
) {
    let execution_started = tokio::time::Instant::now();
    let deadline = RunDeadline::new(claimed.limits, execution_started);
    if *cancellation.borrow() {
        finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }
    let load_progress = RuntimeLoadProgress::default();
    let mut load = inner.loader.load_with_progress(
        RuntimeLoadRequest {
            reasoning_effort: None,
            checkpoint: claimed.checkpoint.clone(),
            workspace: claimed.workspace.clone(),
            model: claimed.model.clone(),
            profile: claimed.profile.clone(),
        },
        load_progress.clone(),
    );
    let mut loaded = tokio::select! {
        biased;
        () = RunDeadline::wait(deadline) => {
            let unfinished_stage = load_progress.stage();
            // The loader owns construction until it returns, even after expiry.
            let _ = load.await;
            let mut exhaustion = deadline.expect("only a finite deadline wakes").exhaustion();
            exhaustion.message.push_str("; runtime preparation was still ");
            exhaustion.message.push_str(unfinished_stage.description());
            exhaustion.message.push_str(" when the budget expired; no model request was started");
            finish_reserved_run(&inner, &claimed, RunOutcome::BudgetExhausted {
                exhaustion: Box::new(exhaustion),
            }).await;
            return;
        }
        result = &mut load => match result {
            Ok(runtime) => runtime,
            Err(error) => {
                finish_reserved_run(&inner, &claimed, RunOutcome::Failed {
                    failure: RunFailure {
                        kind: error.kind,
                        message: truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES),
                    },
                }).await;
                return;
            }
        },
        changed = cancellation.changed() => {
            if changed.is_ok() && *cancellation.borrow() {
                // A terminal event releases the session, so construction must exit first.
                let _ = load.await;
                finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
                return;
            }
            return;
        }
    };
    if *cancellation.borrow() {
        finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }

    // A compiled plan is built from its resolved model, so the two cannot
    // disagree; the loader is still accountable for compiling the plan for
    // this run's workspace.
    if loaded.plan.workspace_path() != Path::new(&claimed.workspace) {
        finish_reserved_run(
            &inner,
            &claimed,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Configuration,
                    message: format!(
                        "loaded plan was compiled for workspace {} but the run belongs to {}",
                        loaded.plan.workspace_path().display(),
                        claimed.workspace
                    ),
                },
            },
        )
        .await;
        return;
    }
    if let Err(outcome) = route_run(
        &inner,
        &claimed,
        &mut loaded,
        &mut cancellation,
        execution_started,
    )
    .await
    {
        finish_reserved_run(&inner, &claimed, outcome).await;
        return;
    }
    let mut bounded_manual_compaction = false;
    loop {
        let mut prepared = match prepare_execution(
            &inner,
            &mut claimed,
            &loaded,
            &mut cancellation,
            &resources,
            execution_started,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(outcome) => {
                if resources.drain().await.is_err() {
                    inner.failed.send_replace(true);
                    return;
                }
                if *inner.failed.borrow() {
                    return;
                }
                finish_reserved_run(&inner, &claimed, outcome).await;
                return;
            }
        };
        let plan = context::plan(context::ContextInput {
            context_window: loaded.resolved_model().context_window,
            max_output_tokens: prepared.audit.weight.max_output_tokens,
            system_bytes: prepared.audit.weight.system_bytes,
            tool_schema_bytes: prepared.audit.weight.tool_schema_bytes,
            reducible_message_bytes: prepared.audit.weight.reducible_message_bytes,
            irreducible_message_bytes: prepared.audit.weight.irreducible_message_bytes,
            compatible_input_tokens: prepared.audit.weight.compatible_input_tokens,
            compaction: if claimed.identity.kind == RunKind::Compaction {
                context::CompactionDisposition::Summarizing
            } else {
                compaction_disposition(&claimed)
            },
        });
        let repeats_known_overflow = matches!(plan, context::ContextPlan::Send { .. })
            && claimed.context_overflow_basis.is_some_and(|basis| {
                repeats_context_basis(
                    basis,
                    prepared.audit.context_shape,
                    prepared.audit.static_prefix,
                )
            })
            && claimed.identity.kind == RunKind::Prompt;
        if repeats_known_overflow {
            let disposition = compaction_disposition(&claimed);
            if let context::CompactionDisposition::Exhausted(exhaustion) = disposition {
                let context::ContextPlan::Send { estimate } = plan else {
                    unreachable!("known overflow override only applies to a send plan")
                };
                finish_prepared_run(
                    &inner,
                    &claimed,
                    &prepared.audit,
                    planned_context_failure(context::ContextPlan::Reject {
                        estimate,
                        reason: context::ContextRejectReason::ProviderReportedOverflow(exhaustion),
                    }),
                )
                .await;
                return;
            }
            let audit = prepared.audit.clone();
            drop(prepared);
            if !run_auto_compaction(
                &inner,
                &mut claimed,
                &loaded,
                audit,
                &mut cancellation,
                &resources,
                execution_started,
            )
            .await
            {
                return;
            }
            continue;
        }
        // A manual compaction whose transcript is estimated past the window
        // reads one window of it at a unit boundary instead, exactly as an
        // automatic step does; the summary covers that span and a further
        // `/compact` folds the rest. Once per run: the bounded reload is
        // itself judged by the provider.
        if let context::ContextPlan::Send { estimate } = plan
            && claimed.identity.kind == RunKind::Compaction
            && !bounded_manual_compaction
            && let Some(window) = estimate.context_window
            && estimate
                .estimated_input_tokens
                .saturating_add(estimate.output_reserve_tokens)
                > u64::from(window)
        {
            let audit = prepared.audit.clone();
            drop(prepared);
            let summarizer = inner
                .store
                .load_summarizer_input(
                    claimed.identity.session_id,
                    context::summarizer_message_byte_budget(
                        Some(window),
                        audit.weight.max_output_tokens,
                        audit.weight.system_bytes,
                        audit.weight.tool_schema_bytes,
                    ),
                )
                .await;
            match summarizer {
                Ok(summarizer) => {
                    bounded_manual_compaction = true;
                    claimed.messages = summarizer.messages;
                    claimed.compaction_cutoff_ordinal = summarizer.cutoff_ordinal;
                }
                Err(error) => {
                    finish_prepared_run(
                        &inner,
                        &claimed,
                        &audit,
                        persistence_failure("failed to bound the compaction request", &error),
                    )
                    .await;
                    return;
                }
            }
            continue;
        }
        match plan {
            context::ContextPlan::Send { .. } => {
                if *inner.failed.borrow() {
                    finish_prepared_run(
                        &inner,
                        &claimed,
                        &prepared.audit,
                        internal_failure("session runtime failed before run start"),
                    )
                    .await;
                    return;
                }
                let started = inner
                    .store
                    .start_reserved_run(
                        &claimed,
                        prepared.audit.clone(),
                        claimed.resolved_input.clone(),
                    )
                    .await;
                match started {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        finish_reserved_run(
                            &inner,
                            &claimed,
                            internal_failure("run preparation lost its durable reservation"),
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        finish_reserved_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist prepared run state", &error),
                        )
                        .await;
                        return;
                    }
                };
                claimed.model = ModelSelection {
                    model: Some(prepared.audit.resolved_model.route.clone()),
                    max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
                    organization: prepared.audit.resolved_model.organization.clone(),
                };
                let initial_accounting = loaded.routing_spend.map(|spend| {
                    RunAccountingAccumulator::new(
                        prepared.audit.resolved_model.pricing.clone(),
                        context_occupancy_basis(
                            prepared.audit.context_shape.digest,
                            prepared.audit.static_prefix,
                            prepared.audit.weight.input_bytes(),
                        ),
                    )
                    .with_routing_spend(Some(spend))
                    .snapshot()
                });
                let cancelled = match cancellation_requested(&inner, claimed.identity.run_id).await
                {
                    Ok(cancelled) => cancelled || *cancellation.borrow(),
                    Err(error) => {
                        // The row is started but the provider stream was
                        // never polled: dropping it is the whole teardown.
                        let Ok(teardown) = resources.stop(&mut prepared.events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run_accounted(
                            &inner,
                            &claimed,
                            persistence_failure("failed to re-read run cancellation", &error),
                            initial_accounting.clone(),
                            teardown,
                        )
                        .await;
                        return;
                    }
                };
                if *inner.failed.borrow() {
                    prepared.tool_cancellation.cancel();
                    let Ok(teardown) = resources.stop(&mut prepared.events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run_accounted(
                        &inner,
                        &claimed,
                        internal_failure("session runtime failed before provider work"),
                        initial_accounting.clone(),
                        teardown,
                    )
                    .await;
                    return;
                }
                if cancelled {
                    prepared.tool_cancellation.cancel();
                    let Ok(teardown) = resources.stop(&mut prepared.events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run_accounted(
                        &inner,
                        &claimed,
                        RunOutcome::Cancelled,
                        initial_accounting,
                        teardown,
                    )
                    .await;
                    return;
                }
                execute_started_run(
                    inner,
                    claimed,
                    cancellation,
                    prepared,
                    &resources,
                    loaded.routing_spend,
                    loaded
                        .plan
                        .runtime
                        .checkpoint
                        .as_ref()
                        .is_some_and(|reviewer| reviewer.reviews_tools()),
                )
                .await;
                return;
            }
            context::ContextPlan::Compact { .. } if claimed.identity.kind == RunKind::Prompt => {
                let audit = prepared.audit.clone();
                drop(prepared);
                if !run_auto_compaction(
                    &inner,
                    &mut claimed,
                    &loaded,
                    audit,
                    &mut cancellation,
                    &resources,
                    execution_started,
                )
                .await
                {
                    return;
                }
            }
            plan => {
                finish_prepared_run(
                    &inner,
                    &claimed,
                    &prepared.audit,
                    planned_context_failure(plan),
                )
                .await;
                return;
            }
        }
    }
}

/// Whether the prompt may spend another summarizer step. Each step reads a
/// window of transcript after the latest cutoff and commits a summary that
/// advances it, so a further step is new input as long as the transcript is
/// not fully summarized, no step for this prompt has failed, and the step
/// cap is not reached. Any of those exhausts the fold; the reason names
/// which so the failure is actionable.
fn compaction_disposition(claimed: &ClaimedRun) -> context::CompactionDisposition {
    if let Some(bytes) = claimed.context_compaction_oversized_unit_bytes {
        return context::CompactionDisposition::Exhausted(
            context::CompactionExhaustion::OversizedUnit { bytes },
        );
    }
    let steps = claimed.context_compaction_attempted;
    let may_continue = steps == 0
        || (claimed.context_compaction_remaining
            && !claimed.context_compaction_failed
            && steps < context::MAX_COMPACTION_STEPS);
    if may_continue {
        context::CompactionDisposition::Eligible
    } else {
        context::CompactionDisposition::Exhausted(context::CompactionExhaustion::Attempted {
            steps,
        })
    }
}

async fn run_auto_compaction(
    inner: &Arc<SessionRuntimeInner>,
    original: &mut ClaimedRun,
    loaded: &LoadedRuntime,
    original_audit: PreparedRunAudit,
    cancellation: &mut watch::Receiver<bool>,
    resources: &RunResources,
    execution_started: tokio::time::Instant,
) -> bool {
    if *inner.failed.borrow() {
        finish_prepared_run(
            inner,
            original,
            &original_audit,
            internal_failure("session runtime failed before automatic compaction"),
        )
        .await;
        return false;
    }
    // The summarizer reads at most one window of transcript: the budget is
    // the window less the summarizer's output reserve and the fixed prefix
    // the prompt's own preparation just measured. Without a declared window
    // the whole transcript is read and the storage backstop alone applies.
    let summarizer = inner
        .store
        .load_summarizer_input(
            original.identity.session_id,
            context::summarizer_message_byte_budget(
                loaded.resolved_model().context_window,
                loaded
                    .resolved_model()
                    .max_output_tokens
                    .min(COMPACTION_OUTPUT_RESERVE_TOKENS),
                original_audit.weight.system_bytes,
                original_audit.weight.tool_schema_bytes,
            ),
        )
        .await;
    let summarizer = match summarizer {
        Ok(summarizer) => summarizer,
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure("failed to assemble automatic compaction", &error),
            )
            .await;
            return false;
        }
    };
    let mut candidate = original.clone();
    candidate.identity.run_id = match RunId::generate() {
        Ok(run_id) => run_id,
        Err(_) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("failed to allocate an automatic compaction run id"),
            )
            .await;
            return false;
        }
    };
    candidate.identity.command_id = match CommandId::generate() {
        Ok(command_id) => command_id,
        Err(_) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("failed to allocate an automatic compaction command id"),
            )
            .await;
            return false;
        }
    };
    candidate.identity.kind = RunKind::Compaction;
    candidate.user_initiated = false;
    candidate.literal_slash = false;
    candidate.messages = summarizer.messages;
    candidate.context_compaction_attempted =
        original.context_compaction_attempted.saturating_add(1);
    candidate.compaction_cutoff_ordinal = summarizer.cutoff_ordinal;
    candidate.context_occupancy = None;
    let mut prepared = match prepare_execution(
        inner,
        &mut candidate,
        loaded,
        cancellation,
        resources,
        execution_started,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(outcome) => {
            if resources.drain().await.is_err() {
                inner.failed.send_replace(true);
                return false;
            }
            if *inner.failed.borrow() {
                return false;
            }
            finish_prepared_run(inner, original, &original_audit, outcome).await;
            return false;
        }
    };
    let plan = context::plan(context::ContextInput {
        context_window: loaded.resolved_model().context_window,
        max_output_tokens: prepared.audit.weight.max_output_tokens,
        system_bytes: prepared.audit.weight.system_bytes,
        tool_schema_bytes: prepared.audit.weight.tool_schema_bytes,
        reducible_message_bytes: prepared.audit.weight.reducible_message_bytes,
        irreducible_message_bytes: prepared.audit.weight.irreducible_message_bytes,
        compatible_input_tokens: prepared.audit.weight.compatible_input_tokens,
        compaction: context::CompactionDisposition::Summarizing,
    });
    if !matches!(plan, context::ContextPlan::Send { .. }) {
        finish_prepared_run(
            inner,
            original,
            &original_audit,
            planned_context_failure(plan),
        )
        .await;
        return false;
    }
    if *inner.failed.borrow() {
        finish_prepared_run(
            inner,
            original,
            &original_audit,
            internal_failure("session runtime failed before automatic compaction start"),
        )
        .await;
        return false;
    }
    let started = inner
        .store
        .start_auto_compaction(original, prepared.audit.clone(), summarizer.cutoff_ordinal)
        .await;
    let mut compaction = match started {
        Ok(Some((compaction, _))) => compaction,
        Ok(None) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("automatic compaction lost its prompt reservation"),
            )
            .await;
            return false;
        }
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure("failed to persist automatic compaction start", &error),
            )
            .await;
            return false;
        }
    };
    let (compaction_cancel, compaction_cancellation) = watch::channel(false);
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.insert(compaction.identity.run_id, compaction_cancel);
    } else {
        inner.failed.send_replace(true);
        let outcome = internal_failure("run cancellation registry is unavailable");
        // Started row, unpolled provider stream: dropping it is the teardown.
        let Ok(teardown) = resources.stop(&mut prepared.events).await else {
            return false;
        };
        finish_run(inner, &compaction, outcome.clone(), teardown).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    compaction.model = ModelSelection {
        model: Some(prepared.audit.resolved_model.route.clone()),
        max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
        organization: prepared.audit.resolved_model.organization.clone(),
    };
    let cancelled = match cancellation_requested(inner, compaction.identity.run_id).await {
        Ok(cancelled) => cancelled,
        Err(error) => {
            let outcome = persistence_failure("failed to re-read compaction cancellation", &error);
            let Ok(teardown) = resources.stop(&mut prepared.events).await else {
                inner.failed.send_replace(true);
                return false;
            };
            finish_run(inner, &compaction, outcome.clone(), teardown).await;
            finish_prepared_run(inner, original, &original_audit, outcome).await;
            return false;
        }
    };
    if *inner.failed.borrow() {
        prepared.tool_cancellation.cancel();
        let outcome = internal_failure("session runtime failed before compaction provider work");
        let Ok(teardown) = resources.stop(&mut prepared.events).await else {
            return false;
        };
        finish_run(inner, &compaction, outcome.clone(), teardown).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    let compaction_run_id = compaction.identity.run_id;
    if cancelled {
        prepared.tool_cancellation.cancel();
        let Ok(teardown) = resources.stop(&mut prepared.events).await else {
            inner.failed.send_replace(true);
            return false;
        };
        finish_run(inner, &compaction, RunOutcome::Cancelled, teardown).await;
    } else {
        execute_started_run(
            Arc::clone(inner),
            compaction,
            compaction_cancellation,
            prepared,
            resources,
            None,
            loaded
                .plan
                .runtime
                .checkpoint
                .as_ref()
                .is_some_and(|reviewer| reviewer.reviews_tools()),
        )
        .await;
    }
    if *inner.failed.borrow() {
        return false;
    }
    let committed = inner
        .store
        .compaction_committed(original.identity.session_id, compaction_run_id)
        .await;
    let compacted = match committed {
        Ok(compacted) => compacted,
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure("failed to verify automatic compaction", &error),
            )
            .await;
            return false;
        }
    };
    match inner.store.reload_reserved_messages(original).await {
        Ok(Some((messages, progress))) => {
            original.messages = messages;
            original.context_compaction_attempted = progress.steps;
            original.context_compaction_failed = progress.failed;
            original.context_compaction_remaining = progress.remaining;
            // A step that sent one unit larger than the budget and still
            // got rejected has proven that unit irreducible; remember it so
            // the prompt's failure names it instead of a step count.
            original.compaction_cutoff_ordinal = None;
            if !compacted && let Some(bytes) = summarizer.oversized_unit_bytes {
                original.context_compaction_oversized_unit_bytes = Some(bytes);
            }
            if compacted {
                original.context_overflow_basis = None;
                original.context_occupancy = None;
            }
            true
        }
        Ok(None) => {
            clear_run_registration(inner, original.identity.run_id);
            false
        }
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure(
                    "failed to reload the reserved prompt after automatic compaction",
                    &error,
                ),
            )
            .await;
            false
        }
    }
}

async fn cancellation_requested(
    inner: &SessionRuntimeInner,
    run_id: RunId,
) -> Result<bool, SessionRuntimeError> {
    if *inner.failed.borrow() {
        return Err(SessionRuntimeError::Unavailable);
    }
    let result = inner.store.cancellation_requested(run_id).await;
    if *inner.failed.borrow() {
        return Err(SessionRuntimeError::Unavailable);
    }
    result
}

async fn finish_reserved_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) {
    match inner.store.finish_reserved_run(claimed, outcome).await {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor);
            }
            inner
                .settlements
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            clear_run_registration(inner, claimed.identity.run_id);
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
}

fn clear_run_registration(inner: &SessionRuntimeInner, run_id: RunId) {
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.remove(&run_id);
    }
    inner.clear_run_approvals(run_id);
}

async fn finish_prepared_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    audit: &PreparedRunAudit,
    outcome: RunOutcome,
) {
    match inner
        .store
        .finish_prepared_run(claimed, audit.clone(), outcome)
        .await
    {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor);
            }
            inner
                .settlements
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            clear_run_registration(inner, claimed.identity.run_id);
        }
        Err(error) => {
            // A trigger or storage failure on one descriptor/identity column
            // must still terminally settle the queued run without pretending
            // the failed audit write was durable.
            finish_reserved_run(
                inner,
                claimed,
                persistence_failure("failed to persist prepared run state", &error),
            )
            .await;
        }
    }
}

async fn execute_started_run(
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    prepared: PreparedExecution,
    resources: &RunResources,
    routing_spend: Option<qq_protocol::CheckpointSpend>,
    checkpoint_enforced: bool,
) {
    let PreparedExecution {
        mut events,
        tool_cancellation,
        audit,
    } = prepared;
    let resolved_model = Arc::clone(&audit.resolved_model);
    let context_shape = audit.context_shape;
    let initial_occupancy_basis = context_occupancy_basis(
        context_shape.digest,
        audit.static_prefix,
        audit.weight.input_bytes(),
    );
    let mut accounting =
        RunAccountingAccumulator::new(resolved_model.pricing.clone(), initial_occupancy_basis)
            .with_routing_spend(routing_spend);
    let mut runtime_failed = inner.failed.subscribe();
    if *runtime_failed.borrow() {
        tool_cancellation.cancel();
        let Ok(teardown) = resources.stop(&mut events).await else {
            inner.failed.send_replace(true);
            return;
        };
        finish_run_accounted(
            &inner,
            &claimed,
            internal_failure("session runtime failed before provider work"),
            routing_spend.map(|_| accounting.snapshot()),
            teardown,
        )
        .await;
        return;
    }
    let internal = claimed.identity.kind == RunKind::Compaction;
    let mut pending_text = String::new();
    let mut pending_channel = None;
    let mut reasoning_kind = None;
    let mut reasoning_delta_persisted = false;
    let mut pending_reasoning_kind = None;
    let mut pending_reasoning_text = String::new();
    let mut flush_at = None;
    // One assistant message per model turn: the message row is created
    // lazily at the turn's first text delta (so call-only turns persist no
    // message row) and finalized when the turn's `persist_model_turn`
    // commits. `current_turn` is the 1-based ordinal of the turn currently
    // streaming; text deltas always belong to it.
    let mut current_turn: u32 = 1;
    let mut current_message: Option<MessageId> = None;
    let mut current_occupancy_basis = Some(initial_occupancy_basis);
    // Live tool output batches on the same timer as model text. Text and tool
    // output never accumulate at the same time: a turn's text is fully
    // flushed when the turn completes, before any of its calls execute.
    let mut pending_tool_call: Option<ToolCallId> = None;
    let mut pending_tool_output = String::new();
    let mut checkpoint_in_flight = None;
    let mut tools_awaiting_checkpoint = std::collections::HashSet::<ToolCallId>::new();
    // An internal run's streamed output never joins the transcript; the
    // summary accumulates here and persists as a compaction row instead.
    let mut summary_text = String::new();
    loop {
        let input = if let Some(deadline) = flush_at {
            tokio::select! {
                biased;
                _ = runtime_failed.changed() => RunInput::RuntimeFailed,
                changed = cancellation.changed() => {
                    if changed.is_ok() && *cancellation.borrow() {
                        RunInput::Cancelled
                    } else {
                        RunInput::Interrupted
                    }
                }
                () = tokio::time::sleep_until(deadline) => RunInput::Flush,
                event = events.next() => RunInput::Event(event),
            }
        } else {
            tokio::select! {
                biased;
                _ = runtime_failed.changed() => RunInput::RuntimeFailed,
                changed = cancellation.changed() => {
                    if changed.is_ok() && *cancellation.borrow() {
                        RunInput::Cancelled
                    } else {
                        RunInput::Interrupted
                    }
                }
                event = events.next() => RunInput::Event(event),
            }
        };
        let (continues_text, continues_tool_output, continues_reasoning) = match &input {
            RunInput::Event(Some(event)) => (
                matches!(
                    event,
                    RuntimeEvent::OutputTextDelta { .. }
                        if pending_channel == Some(TextChannel::Output)
                ) || matches!(
                    event,
                    RuntimeEvent::RefusalDelta { .. }
                        if pending_channel == Some(TextChannel::Refusal)
                ),
                matches!(
                    event,
                    RuntimeEvent::ToolCallOutputDelta { id, .. }
                        if pending_tool_call == Some(*id)
                ),
                matches!(
                    event,
                    RuntimeEvent::ReasoningDelta { kind, .. }
                        if pending_reasoning_kind == Some(*kind)
                ),
            ),
            _ => (false, false, false),
        };
        if !pending_text.is_empty()
            && !continues_text
            && let Err(error) = flush_pending_text(
                &inner,
                &claimed,
                current_turn,
                &mut current_message,
                &mut pending_channel,
                &mut pending_text,
            )
            .await
        {
            let Ok(teardown) = resources.stop(&mut events).await else {
                inner.failed.send_replace(true);
                return;
            };
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist model output", &error),
                teardown,
            )
            .await;
            return;
        }
        let stopped = matches!(
            &input,
            RunInput::Cancelled | RunInput::Interrupted | RunInput::RuntimeFailed
        );
        if !pending_tool_output.is_empty()
            && !continues_tool_output
            && !stopped
            && let Err(error) = flush_pending_tool_output(
                &inner,
                &claimed,
                &mut pending_tool_call,
                &mut pending_tool_output,
            )
            .await
        {
            let Ok(teardown) = resources.stop(&mut events).await else {
                inner.failed.send_replace(true);
                return;
            };
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist tool output", &error),
                teardown,
            )
            .await;
            return;
        }
        if !pending_reasoning_text.is_empty()
            && !continues_reasoning
            && let Err(error) = flush_pending_reasoning(
                &inner,
                &claimed,
                &mut pending_reasoning_kind,
                &mut pending_reasoning_text,
            )
            .await
        {
            let Ok(teardown) = resources.stop(&mut events).await else {
                inner.failed.send_replace(true);
                return;
            };
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist reasoning", &error),
                teardown,
            )
            .await;
            return;
        }
        if pending_text.is_empty()
            && pending_tool_output.is_empty()
            && pending_reasoning_text.is_empty()
        {
            flush_at = None;
        }
        match input {
            RunInput::Flush => {
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_tool_output(
                    &inner,
                    &claimed,
                    &mut pending_tool_call,
                    &mut pending_tool_output,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                flush_at = None;
            }
            stopped @ (RunInput::Cancelled | RunInput::Interrupted) => {
                tool_cancellation.cancel();
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    if let Err(checkpoint_error) = record_unreviewed_checkpoints(
                        &inner,
                        &claimed,
                        &mut tools_awaiting_checkpoint,
                        &mut checkpoint_in_flight,
                        &mut accounting,
                        "failed while settling cancellation",
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist terminal JEV checkpoint status",
                                &checkpoint_error,
                            ),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    if let Err(checkpoint_error) = record_unreviewed_checkpoints(
                        &inner,
                        &claimed,
                        &mut tools_awaiting_checkpoint,
                        &mut checkpoint_in_flight,
                        &mut accounting,
                        "failed while settling cancellation",
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist terminal JEV checkpoint status",
                                &checkpoint_error,
                            ),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                // Buffered live tool output is dropped rather than flushed:
                // the interrupted call's terminal result replaces it.
                let outcome = if matches!(stopped, RunInput::Cancelled) {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                let cancellation_label = if matches!(stopped, RunInput::Cancelled) {
                    "cancelled"
                } else {
                    "interrupted"
                };
                if let Err(error) = record_unreviewed_checkpoints(
                    &inner,
                    &claimed,
                    &mut tools_awaiting_checkpoint,
                    &mut checkpoint_in_flight,
                    &mut accounting,
                    cancellation_label,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure(
                            "failed to persist terminal JEV checkpoint status",
                            &error,
                        ),
                        teardown,
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    outcome,
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
            RunInput::RuntimeFailed => {
                tool_cancellation.cancel();
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    if let Err(checkpoint_error) = record_unreviewed_checkpoints(
                        &inner,
                        &claimed,
                        &mut tools_awaiting_checkpoint,
                        &mut checkpoint_in_flight,
                        &mut accounting,
                        "failed while settling a runtime failure",
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist terminal JEV checkpoint status",
                                &checkpoint_error,
                            ),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    if let Err(checkpoint_error) = record_unreviewed_checkpoints(
                        &inner,
                        &claimed,
                        &mut tools_awaiting_checkpoint,
                        &mut checkpoint_in_flight,
                        &mut accounting,
                        "failed while settling a runtime failure",
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist terminal JEV checkpoint status",
                                &checkpoint_error,
                            ),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                if let Err(error) = record_unreviewed_checkpoints(
                    &inner,
                    &claimed,
                    &mut tools_awaiting_checkpoint,
                    &mut checkpoint_in_flight,
                    &mut accounting,
                    "failed",
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure(
                            "failed to persist terminal JEV checkpoint status",
                            &error,
                        ),
                        teardown,
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    internal_failure("session runtime failed during provider work"),
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::Started)) => {}
            RunInput::Event(Some(RuntimeEvent::Prepared {
                turn_ordinal: _,
                identity,
                static_prefix,
                weight,
            })) => {
                if let Some(identity) = identity
                    && let Err(error) = inner.store.record_prompt_identity(&claimed, identity).await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist the run prompt identity", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                let plan = context::plan(context::ContextInput {
                    context_window: resolved_model.context_window,
                    max_output_tokens: weight.max_output_tokens,
                    system_bytes: weight.system_bytes,
                    tool_schema_bytes: weight.tool_schema_bytes,
                    reducible_message_bytes: weight.reducible_message_bytes,
                    irreducible_message_bytes: weight.irreducible_message_bytes,
                    compatible_input_tokens: weight.compatible_input_tokens,
                    // Compaction is only legal between runs. The second slice
                    // will turn the first-turn Compact result into a reserved
                    // auto-compaction; later turns must fail closed without
                    // polling the provider. The summarizer's own request is
                    // planned against storage only.
                    compaction: if internal {
                        context::CompactionDisposition::Summarizing
                    } else {
                        context::CompactionDisposition::BetweenRunsOnly
                    },
                });
                match plan {
                    context::ContextPlan::Send { .. } => {}
                    plan => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(&inner, &claimed, planned_context_failure(plan), teardown).await;
                        return;
                    }
                }
                let basis = context_occupancy_basis(
                    context_shape.digest,
                    static_prefix,
                    weight.input_bytes(),
                );
                current_occupancy_basis = Some(basis);
                accounting.request_basis = basis;
            }
            RunInput::Event(Some(RuntimeEvent::ActivityChanged { activity })) => {
                if internal {
                    continue;
                }
                match inner.store.append_run_activity(&claimed, activity).await {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist run activity", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ReasoningStarted { kind })) => {
                if internal {
                    continue;
                }
                reasoning_kind = Some(kind);
                reasoning_delta_persisted = false;
                match inner
                    .store
                    .append_reasoning(&claimed, ReasoningEvent::Started { kind })
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ReasoningDelta { kind, text })) => {
                if internal || text.is_empty() {
                    continue;
                }
                if reasoning_kind != Some(kind) {
                    reasoning_kind = Some(kind);
                    reasoning_delta_persisted = false;
                }
                if !reasoning_delta_persisted {
                    match inner
                        .store
                        .append_reasoning(&claimed, ReasoningEvent::Delta { kind, text })
                        .await
                    {
                        Ok(_) => {}
                        Err(error) => {
                            let Ok(teardown) = resources.stop(&mut events).await else {
                                inner.failed.send_replace(true);
                                return;
                            };
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure("failed to persist reasoning", &error),
                                teardown,
                            )
                            .await;
                            return;
                        }
                    }
                    reasoning_delta_persisted = true;
                    continue;
                }
                if pending_reasoning_text.is_empty() {
                    pending_reasoning_kind = Some(kind);
                    if flush_at.is_none() {
                        flush_at = Some(tokio::time::Instant::now() + OUTPUT_BATCH_DELAY);
                    }
                }
                pending_reasoning_text.push_str(&text);
                if pending_reasoning_text.len() >= OUTPUT_BATCH_BYTES {
                    if let Err(error) = flush_pending_reasoning(
                        &inner,
                        &claimed,
                        &mut pending_reasoning_kind,
                        &mut pending_reasoning_text,
                    )
                    .await
                    {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    if pending_text.is_empty() && pending_tool_output.is_empty() {
                        flush_at = None;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ReasoningCompleted { kind })) => {
                if internal {
                    continue;
                }
                match inner
                    .store
                    .append_reasoning(&claimed, ReasoningEvent::Completed { kind })
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
                reasoning_kind = None;
                reasoning_delta_persisted = false;
            }
            RunInput::Event(Some(RuntimeEvent::AssistantTurnCompleted {
                turn_ordinal,
                message,
                usage,
                calls,
                truncated,
            })) => {
                if internal {
                    // Usage, cost, and provider-turn identity persist like
                    // any run. The turn's text joins the summary instead of
                    // the transcript, and compaction tool calls remain
                    // non-authoritative and unpublished.
                    let turn_cost = usage.and_then(|usage| {
                        accounting
                            .pricing
                            .as_ref()
                            .and_then(|pricing| run_cost(usage, pricing))
                    });
                    accounting.record_turn(usage);
                    for block in message.content() {
                        if let ContentBlock::Text { text } = block {
                            if !summary_text.is_empty() {
                                summary_text.push('\n');
                            }
                            summary_text.push_str(text);
                        }
                    }
                    current_turn = turn_ordinal.saturating_add(1);
                    match inner
                        .store
                        .persist_model_turn(
                            &claimed,
                            ModelTurnCommit {
                                turn_ordinal,
                                message,
                                calls,
                                turn_message: None,
                                context_tokens: usage.map(turn_context_tokens),
                                occupancy_basis: None,
                                usage,
                                estimated_cost_usd_nanos: turn_cost,
                                accounting: Some(accounting.snapshot()),
                                truncated,
                            },
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(error) => {
                            let Ok(teardown) = resources.stop(&mut events).await else {
                                inner.failed.send_replace(true);
                                return;
                            };
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure(
                                    "failed to persist the completed compaction turn",
                                    &error,
                                ),
                                teardown,
                            )
                            .await;
                            return;
                        }
                    }
                    continue;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                flush_at = None;
                let turn_cost = usage.and_then(|usage| {
                    accounting
                        .pricing
                        .as_ref()
                        .and_then(|pricing| run_cost(usage, pricing))
                });
                accounting.record_turn(usage);
                let turn_accounting = accounting.snapshot();
                // The completed turn's usage measures the context the run now
                // occupies; the turn transaction publishes it so the meter
                // moves while the tool loop is still running.
                let context_tokens = usage.map(turn_context_tokens);
                // Only an exact provider identity may seed a later request;
                // a route-level fallback basis is never persisted for reuse.
                let occupancy_basis = usage
                    .and(current_occupancy_basis.take())
                    .filter(|_| context_shape.provider_identity);
                // The completed turn's message (if any) finalizes inside the
                // same transaction as the turn row; the next turn's text will
                // lazily start a fresh message.
                let turn_message = current_message.take();
                current_turn = turn_ordinal.saturating_add(1);
                match inner
                    .store
                    .persist_model_turn(
                        &claimed,
                        ModelTurnCommit {
                            turn_ordinal,
                            message,
                            calls,
                            turn_message,
                            context_tokens,
                            occupancy_basis,
                            usage,
                            estimated_cost_usd_nanos: turn_cost,
                            accounting: Some(turn_accounting),
                            truncated,
                        },
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist the completed model turn",
                                &error,
                            ),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            // Approval transitions (including denials) are persisted and
            // published by the tool gate before this event is emitted.
            RunInput::Event(Some(
                RuntimeEvent::ToolCallDenied { .. } | RuntimeEvent::ToolCallAnswered { .. },
            )) => {}
            // The audit record is durable on the run before the run settles
            // or revises. Its spend is stored on the audit child and included
            // by subtree accounting, never duplicated in the parent's direct totals.
            RunInput::Event(Some(RuntimeEvent::Audited {
                outcome,
                findings,
                revisions,
                usage,
                cost_usd_nanos,
                audit_session: _,
            })) => {
                let record = AuditRecord {
                    outcome,
                    findings,
                    revisions,
                    usage,
                    estimated_cost_usd_nanos: cost_usd_nanos,
                };
                match inner.store.record_audit(&claimed, record).await {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the audit record", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            // Reviewer spend joins the run's accounting; the next persisted
            // turn or the run's settlement carries the updated totals.
            RunInput::Event(Some(RuntimeEvent::ReviewCharged {
                usage,
                cost_usd_nanos,
            })) => {
                accounting.record_review(usage, cost_usd_nanos);
            }
            RunInput::Event(Some(RuntimeEvent::CheckpointStarted {
                correlation,
                phase,
                tool_call_id,
            })) => {
                if let Err(error) = inner
                    .store
                    .record_checkpoint_started(&claimed, correlation.clone(), phase, tool_call_id)
                    .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist Jev review start", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                checkpoint_in_flight = Some((correlation, phase, tool_call_id));
            }
            RunInput::Event(Some(RuntimeEvent::CheckpointReviewed {
                correlation,
                phase,
                tool_call_id,
                outcome,
                confidence,
                feedback,
                spend,
            })) => {
                if let Some(spend) = spend {
                    accounting.record_review(spend.usage, spend.estimated_cost_usd_nanos);
                }
                let confidence_basis_points =
                    confidence.map(|value| (value.clamp(0.0, 1.0) * 10_000.0).round() as u16);
                if let Err(error) = inner
                    .store
                    .record_checkpoint(
                        &claimed,
                        streaming::CheckpointRecord {
                            correlation,
                            phase,
                            tool_call_id,
                            outcome,
                            confidence_basis_points,
                            feedback,
                            spend,
                        },
                        Some(accounting.snapshot()),
                    )
                    .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist JEV checkpoint", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                checkpoint_in_flight = None;
                if let Some(tool_call_id) = tool_call_id {
                    tools_awaiting_checkpoint.remove(&tool_call_id);
                }
            }
            RunInput::Event(Some(RuntimeEvent::ToolCallStarted { id })) => {
                if internal {
                    continue;
                }
                match inner.store.start_tool_call(&claimed, id).await {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the started tool call", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ToolCallOutputDelta { id, chunk })) => {
                if internal || chunk.is_empty() {
                    continue;
                }
                if pending_tool_call.is_some_and(|pending| pending != id)
                    && let Err(error) = flush_pending_tool_output(
                        &inner,
                        &claimed,
                        &mut pending_tool_call,
                        &mut pending_tool_output,
                    )
                    .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if pending_tool_output.is_empty() {
                    pending_tool_call = Some(id);
                    if flush_at.is_none() {
                        flush_at = Some(tokio::time::Instant::now() + OUTPUT_BATCH_DELAY);
                    }
                }
                pending_tool_output.push_str(&chunk);
                #[cfg(test)]
                pause_after_buffering_tool_output(id).await;
                if pending_tool_output.len() >= OUTPUT_BATCH_BYTES {
                    if let Err(error) = flush_pending_tool_output(
                        &inner,
                        &claimed,
                        &mut pending_tool_call,
                        &mut pending_tool_output,
                    )
                    .await
                    {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist tool output", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    flush_at = None;
                }
            }
            RunInput::Event(Some(RuntimeEvent::ToolCallFinished {
                id,
                result,
                is_error,
                file_states,
                display,
                spill,
            })) => {
                if internal {
                    continue;
                }
                // Any buffered live output flushes first so replay preserves
                // the chunk-then-result order.
                if let Err(error) = flush_pending_tool_output(
                    &inner,
                    &claimed,
                    &mut pending_tool_call,
                    &mut pending_tool_output,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if checkpoint_enforced {
                    // Mark before the durable write awaits: subscribers can
                    // observe the committed result and request cancellation
                    // before this task resumes from the store actor.
                    tools_awaiting_checkpoint.insert(id);
                }
                match inner
                    .store
                    .finish_tool_call(&claimed, id, result, is_error, file_states, display, spill)
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        tools_awaiting_checkpoint.remove(&id);
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the tool result", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(
                event @ (RuntimeEvent::OutputTextDelta { .. } | RuntimeEvent::RefusalDelta { .. }),
            )) => {
                let (channel, text) = match event {
                    RuntimeEvent::OutputTextDelta { text } => (TextChannel::Output, text),
                    RuntimeEvent::RefusalDelta { text } => (TextChannel::Refusal, text),
                    _ => unreachable!("matched text event"),
                };
                // Internal runs stream no transcript text; the summary is
                // captured from the completed turn instead.
                if internal || text.is_empty() {
                    continue;
                }
                if current_message.is_none() {
                    // A turn's first delta persists immediately: it creates
                    // the turn's message row and publishes the new message
                    // without batching latency.
                    if let Err(error) = persist_text(
                        &inner,
                        &claimed,
                        current_turn,
                        &mut current_message,
                        channel,
                        text,
                    )
                    .await
                    {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist model output", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    continue;
                }
                if pending_channel.is_some_and(|pending| pending != channel)
                    && let Err(error) = flush_pending_text(
                        &inner,
                        &claimed,
                        current_turn,
                        &mut current_message,
                        &mut pending_channel,
                        &mut pending_text,
                    )
                    .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if pending_text.is_empty() {
                    pending_channel = Some(channel);
                    flush_at = Some(tokio::time::Instant::now() + OUTPUT_BATCH_DELAY);
                }
                pending_text.push_str(&text);
                if pending_text.len() >= OUTPUT_BATCH_BYTES {
                    if let Err(error) = flush_pending_text(
                        &inner,
                        &claimed,
                        current_turn,
                        &mut current_message,
                        &mut pending_channel,
                        &mut pending_text,
                    )
                    .await
                    {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist model output", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    flush_at = None;
                }
            }
            RunInput::Event(Some(RuntimeEvent::SteeringApplied {
                message_id,
                turn_ordinal,
            })) => {
                match inner
                    .store
                    .apply_steering(&claimed, message_id, turn_ordinal)
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist applied steering", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::Interrupted { turn_ordinal })) => {
                // The partial turn's text was committed by the preceding
                // `AssistantTurnCompleted`; drop any buffered live tool output
                // (the interrupted result replaces it) and settle the rows.
                pending_tool_call = None;
                pending_tool_output.clear();
                match inner.store.record_interrupted(&claimed, turn_ordinal).await {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the interrupted turn", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::OutputTruncated {
                turn_ordinal,
                continuation,
            })) => {
                if internal {
                    continue;
                }
                match inner
                    .store
                    .record_output_truncated(&claimed, turn_ordinal, continuation)
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the truncated turn", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            // The failing turn is already committed via AssistantTurnCompleted;
            // the repair notice is a runtime message this run alone sees, so
            // nothing further is persisted or published here.
            RunInput::Event(Some(RuntimeEvent::OutputRepairRequested { .. })) => {}
            RunInput::Event(Some(RuntimeEvent::Completed { final_output })) => {
                if internal {
                    let summary = std::mem::take(&mut summary_text);
                    if summary.trim().is_empty() {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run_accounted(
                            &inner,
                            &claimed,
                            RunOutcome::Failed {
                                failure: RunFailure {
                                    kind: RunFailureKind::ProviderResponse,
                                    message: "compaction produced an empty summary".to_owned(),
                                },
                            },
                            Some(accounting.snapshot()),
                            teardown,
                        )
                        .await;
                        return;
                    }
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    match inner
                        .store
                        .finish_compaction_run(
                            &claimed,
                            summary,
                            Some(accounting.snapshot()),
                            teardown,
                        )
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
                    if let Ok(mut cancellations) = inner.cancellations.lock() {
                        cancellations.remove(&claimed.identity.run_id);
                    }
                    inner.clear_run_approvals(claimed.identity.run_id);
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                let unreviewed = match record_unreviewed_checkpoints(
                    &inner,
                    &claimed,
                    &mut tools_awaiting_checkpoint,
                    &mut checkpoint_in_flight,
                    &mut accounting,
                    "completed",
                )
                .await
                {
                    Ok(count) => count,
                    Err(error) => {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist terminal JEV checkpoint status",
                                &error,
                            ),
                            teardown,
                        )
                        .await;
                        return;
                    }
                };
                if unreviewed != 0 {
                    finish_run_accounted(
                        &inner,
                        &claimed,
                        internal_failure(
                            "run attempted completion with a durable tool result that was not reviewed",
                        ),
                        Some(accounting.snapshot()),
                        teardown,
                    )
                    .await;
                    return;
                }
                let mut settled = accounting.snapshot();
                settled.final_output = final_output;
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Completed,
                    Some(settled),
                    teardown,
                )
                .await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::BudgetExhausted { exhaustion })) => {
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                if let Err(error) = record_unreviewed_checkpoints(
                    &inner,
                    &claimed,
                    &mut tools_awaiting_checkpoint,
                    &mut checkpoint_in_flight,
                    &mut accounting,
                    "stopped by its budget",
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure(
                            "failed to persist terminal JEV checkpoint status",
                            &error,
                        ),
                        teardown,
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::BudgetExhausted {
                        exhaustion: Box::new(exhaustion),
                    },
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::Failed { kind, message })) => {
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                if let Err(error) = record_unreviewed_checkpoints(
                    &inner,
                    &claimed,
                    &mut tools_awaiting_checkpoint,
                    &mut checkpoint_in_flight,
                    &mut accounting,
                    "failed",
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure(
                            "failed to persist terminal JEV checkpoint status",
                            &error,
                        ),
                        teardown,
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Failed {
                        failure: RunFailure {
                            kind,
                            message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
                        },
                    },
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
            RunInput::Event(None) => {
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
                    &mut pending_channel,
                    &mut pending_text,
                )
                .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                if let Err(error) = record_unreviewed_checkpoints(
                    &inner,
                    &claimed,
                    &mut tools_awaiting_checkpoint,
                    &mut checkpoint_in_flight,
                    &mut accounting,
                    "ended without a terminal event",
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure(
                            "failed to persist terminal JEV checkpoint status",
                            &error,
                        ),
                        teardown,
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    internal_failure("model stream ended without a terminal event"),
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
        }
    }
}

enum RunInput {
    Event(Option<RuntimeEvent>),
    Flush,
    Cancelled,
    Interrupted,
    RuntimeFailed,
}

async fn flush_pending_reasoning(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    kind: &mut Option<qq_provider::ReasoningKind>,
    text: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(kind) = kind.take() else {
        return Ok(());
    };
    let text = std::mem::take(text);
    if text.is_empty() {
        return Ok(());
    }
    inner
        .store
        .append_reasoning(claimed, ReasoningEvent::Delta { kind, text })
        .await?;
    Ok(())
}

async fn flush_pending_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    current_turn: u32,
    current_message: &mut Option<MessageId>,
    channel: &mut Option<TextChannel>,
    text: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(channel) = channel.take() else {
        return Ok(());
    };
    persist_text(
        inner,
        claimed,
        current_turn,
        current_message,
        channel,
        std::mem::take(text),
    )
    .await
}

/// Publishes any buffered live tool output as one batched
/// `ToolCallOutputDelta` event.
async fn flush_pending_tool_output(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    pending_call: &mut Option<ToolCallId>,
    pending_output: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(tool_call_id) = pending_call.take() else {
        return Ok(());
    };
    let chunk = std::mem::take(pending_output);
    if chunk.is_empty() {
        return Ok(());
    }
    inner
        .store
        .append_tool_output(claimed, tool_call_id, chunk)
        .await?;
    Ok(())
}

/// Persists model text into the current turn's assistant message, creating
/// that message on the turn's first chunk.
async fn persist_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    current_turn: u32,
    current_message: &mut Option<MessageId>,
    channel: TextChannel,
    text: String,
) -> Result<(), SessionRuntimeError> {
    let mut remaining = text.as_str();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(MAX_TEXT_CHUNK_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = remaining[..end].to_owned();
        match *current_message {
            Some(message_id) => {
                inner
                    .store
                    .append_text(claimed, message_id, channel, chunk)
                    .await?;
            }
            None => {
                let message_id =
                    MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                inner
                    .store
                    .begin_assistant_message(claimed, message_id, current_turn, channel, chunk)
                    .await?;
                *current_message = Some(message_id);
            }
        }
        remaining = &remaining[end..];
    }
    Ok(())
}

/// Persists an explicit fail-closed status for every durable tool result whose
/// reviewer future was cut short by terminal run settlement. This is separate
/// from the terminal outcome: it never claims that the remote reviewer ran.
async fn record_unreviewed_checkpoints(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    pending: &mut HashSet<ToolCallId>,
    in_flight: &mut Option<(String, qq_protocol::CheckpointPhase, Option<ToolCallId>)>,
    accounting: &mut RunAccountingAccumulator,
    terminal_reason: &str,
) -> Result<usize, SessionRuntimeError> {
    let mut count = 0;
    if let Some((correlation, phase, tool_call_id)) = in_flight.take() {
        accounting.record_review(None, None);
        inner.store.record_checkpoint(claimed, streaming::CheckpointRecord {
            correlation, phase, tool_call_id, outcome: qq_protocol::CheckpointOutcome::Unavailable, confidence_basis_points: None,
            feedback: format!("No Jev reviewer verdict was durably recorded before the run {terminal_reason}; remote spend is unknown"),
            spend: Some(qq_protocol::CheckpointSpend::default()),
        }, Some(accounting.snapshot())).await?;
        if let Some(id) = tool_call_id {
            pending.remove(&id);
        }
        count += 1;
    }
    let mut tool_call_ids = pending.drain().collect::<Vec<_>>();
    tool_call_ids.sort_by_key(ToString::to_string);
    count += tool_call_ids.len();
    for tool_call_id in tool_call_ids {
        inner
            .store
            .record_checkpoint(
                claimed,
                streaming::CheckpointRecord {
                    correlation: format!("tool:{tool_call_id}"),
                    phase: qq_protocol::CheckpointPhase::ToolResult,
                    tool_call_id: Some(tool_call_id),
                    outcome: qq_protocol::CheckpointOutcome::Unavailable,
                    confidence_basis_points: None,
                    feedback: format!("No JEV reviewer verdict was durably recorded before the run {terminal_reason} after the tool result became durable"),
                    spend: None,
                },
                None,
            )
            .await?;
    }
    Ok(count)
}

/// Settles a started run. `teardown` proves the run's tools and children were
/// drained first; the store will not settle a started run without it.
pub(super) async fn finish_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    teardown: TeardownComplete,
) {
    finish_run_accounted(inner, claimed, outcome, None, teardown).await;
}

async fn finish_run_accounted(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
    teardown: TeardownComplete,
) {
    match inner
        .store
        .finish_run(claimed, outcome, accounting, teardown)
        .await
    {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor);
            }
            inner
                .settlements
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            clear_run_registration(inner, claimed.identity.run_id);
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
}

#[derive(Clone)]
pub(super) struct RunAccounting {
    pub(super) usage: Option<TokenUsage>,
    pub(super) context_tokens: Option<u64>,
    pub(super) estimated_cost_usd_nanos: Option<u64>,
    pub(super) saw_turn: bool,
    /// Basis of the most recently prepared provider request. A provider
    /// overflow persists it so the retry cannot repeat the same request.
    pub(super) request_basis: ContextOccupancyBasis,
    /// The typed-output verdict the runtime reached with `Completed`; set
    /// only for runs claimed with a contract, persisted in the settlement
    /// transaction.
    pub(super) final_output: Option<Box<FinalOutput>>,
}

pub(super) struct RunAccountingAccumulator {
    usage: Option<TokenUsage>,
    context_tokens: Option<u64>,
    estimated_cost_usd_nanos: Option<u64>,
    pricing: Option<ModelPricing>,
    saw_turn: bool,
    saw_spend: bool,
    request_basis: ContextOccupancyBasis,
}

impl RunAccountingAccumulator {
    pub(super) fn new(pricing: Option<ModelPricing>, request_basis: ContextOccupancyBasis) -> Self {
        Self {
            usage: Some(TokenUsage::default()),
            context_tokens: None,
            estimated_cost_usd_nanos: pricing.as_ref().map(|_| 0),
            pricing,
            saw_turn: false,
            saw_spend: false,
            request_basis,
        }
    }

    pub(super) fn with_routing_spend(
        mut self,
        spend: Option<qq_protocol::CheckpointSpend>,
    ) -> Self {
        if let Some(spend) = spend {
            self.usage = spend.usage;
            self.estimated_cost_usd_nanos = spend.estimated_cost_usd_nanos;
            self.saw_spend = true;
        }
        self
    }

    /// Adds a reviewer's provider spend to the run's totals. It is not a turn
    /// of this run (context occupancy is untouched) but the run is
    /// accountable for it; unknown spend makes the totals unknown.
    pub(super) fn record_review(&mut self, usage: Option<TokenUsage>, cost_usd_nanos: Option<u64>) {
        self.saw_spend = true;
        match usage {
            Some(usage) => self.usage = self.usage.and_then(|total| add_usage(total, usage)),
            None => self.usage = None,
        }
        self.estimated_cost_usd_nanos = match (self.estimated_cost_usd_nanos, cost_usd_nanos) {
            (Some(total), Some(cost)) => total.checked_add(cost),
            _ => None,
        };
    }

    pub(super) fn record_turn(&mut self, usage: Option<TokenUsage>) {
        self.saw_turn = true;
        self.saw_spend = true;
        let Some(usage) = usage else {
            self.usage = None;
            // A newer completed request without usage makes the run's and
            // session's current occupancy unknown. Retaining an older exact
            // turn would present stale state as authoritative.
            self.context_tokens = None;
            self.estimated_cost_usd_nanos = None;
            return;
        };
        // Context occupancy is the latest reported turn's input total, not a
        // sum: every model request re-sends the whole conversation, so the
        // last measured request describes what the context window held.
        self.context_tokens = Some(turn_context_tokens(usage));
        self.usage = self.usage.and_then(|total| add_usage(total, usage));
        if self.usage.is_none() {
            self.estimated_cost_usd_nanos = None;
            return;
        }
        self.estimated_cost_usd_nanos = self.estimated_cost_usd_nanos.and_then(|total| {
            run_cost(usage, self.pricing.as_ref()?).and_then(|cost| total.checked_add(cost))
        });
    }

    pub(super) fn snapshot(&self) -> RunAccounting {
        RunAccounting {
            usage: self.saw_spend.then_some(self.usage).flatten(),
            context_tokens: self.context_tokens,
            estimated_cost_usd_nanos: self
                .saw_spend
                .then_some(self.estimated_cost_usd_nanos)
                .flatten(),
            saw_turn: self.saw_turn,
            request_basis: self.request_basis,
            final_output: None,
        }
    }
}

/// The input-token total of one model turn (fresh input plus cache reads and
/// writes): what that turn's request occupied of the model context window.
const fn turn_context_tokens(usage: TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_write_input_tokens)
}

pub(super) fn add_usage(left: TokenUsage, right: TokenUsage) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: left.input_tokens.checked_add(right.input_tokens)?,
        cache_read_input_tokens: left
            .cache_read_input_tokens
            .checked_add(right.cache_read_input_tokens)?,
        cache_write_input_tokens: left
            .cache_write_input_tokens
            .checked_add(right.cache_write_input_tokens)?,
        output_tokens: left.output_tokens.checked_add(right.output_tokens)?,
        // A known reasoning total stays known only while every turn reports
        // one; a single turn without it makes the sum unknown, never a lie.
        reasoning_tokens: match (left.reasoning_tokens, right.reasoning_tokens) {
            (Some(left), Some(right)) => Some(left.checked_add(right)?),
            _ => None,
        },
    })
}

pub(super) fn internal_failure(message: &str) -> RunOutcome {
    RunOutcome::Failed {
        failure: RunFailure {
            kind: RunFailureKind::Server,
            message: message.to_owned(),
        },
    }
}

/// Maps a store error during a run into a run outcome. The deliberate session
/// context budget surfaces as a user-meaningful policy failure; every other
/// error is an internal failure that carries the store error rather than
/// discarding it, since qq-core has no logging facility to record it.
pub(super) fn persistence_failure(action: &str, error: &SessionRuntimeError) -> RunOutcome {
    match error {
        SessionRuntimeError::OutputTooLarge | SessionRuntimeError::ContextTooLarge => {
            context_budget_failure()
        }
        error => RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                message: format!("{action}: {error}"),
            },
        },
    }
}

/// The deliberate context-budget policy failure. Since auto-compaction, a
/// prompt run reaches it only after one compaction attempt could not bring
/// the assembly back under the budget (or mid-run, when model output alone
/// pushes past it — never compacted mid-run).
fn context_budget_failure() -> RunOutcome {
    RunOutcome::Failed {
        failure: RunFailure {
            kind: RunFailureKind::Policy,
            message: format!(
                "session context reached its {} MiB limit; start a new session to continue",
                MAX_CONTEXT_BYTES / (1024 * 1024)
            ),
        },
    }
}

fn planned_context_failure(plan: context::ContextPlan) -> RunOutcome {
    let Some(message) = context::rejection_message(plan) else {
        return internal_failure("context planner rejected a sendable request");
    };
    RunOutcome::Failed {
        failure: RunFailure {
            kind: RunFailureKind::Policy,
            message,
        },
    }
}

pub(super) struct ModelTurnCommit {
    pub(super) turn_ordinal: u32,
    pub(super) message: Message,
    pub(super) calls: Vec<RuntimeToolCall>,
    pub(super) turn_message: Option<MessageId>,
    pub(super) context_tokens: Option<u64>,
    pub(super) occupancy_basis: Option<ContextOccupancyBasis>,
    pub(super) usage: Option<TokenUsage>,
    pub(super) estimated_cost_usd_nanos: Option<u64>,
    pub(super) accounting: Option<RunAccounting>,
    /// The provider cut this turn at its output token limit. Persisted on the
    /// turn row and the turn's message so context assembly can replay the
    /// continuation notice and clients can mark the prefix.
    pub(super) truncated: bool,
}
