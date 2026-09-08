use super::*;
use super::{
    approvals::SessionToolGate,
    runtime::SessionRuntimeInner,
    subagents::{ChildTasks, SessionAuditHook, SessionHistorySearcher, SessionSubagentSpawner},
};

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
static BUFFERED_TOOL_OUTPUT_HOOK: Mutex<Option<BufferedToolOutputHook>> = Mutex::new(None);

/// Holds the execution loop immediately after one call's live output enters
/// the bounded batch. Tests use this exact handoff to make cancellation-versus-
/// timer ordering deterministic without adding production sleeps or hooks.
#[cfg(test)]
pub(super) fn hold_buffered_tool_output(
    tool_call_id: ToolCallId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    *BUFFERED_TOOL_OUTPUT_HOOK.lock().unwrap() = Some(BufferedToolOutputHook {
        tool_call_id,
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[cfg(test)]
async fn pause_after_buffering_tool_output(tool_call_id: ToolCallId) {
    let hook = {
        let mut hook = BUFFERED_TOOL_OUTPUT_HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|hook| hook.tool_call_id == tool_call_id)
        {
            hook.take()
        } else {
            None
        }
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
    tool_cancellation: Arc<AtomicBool>,
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
    pub(super) async fn drain(&self) -> Result<(), ExecutionCleanupError> {
        let (tools, children) = tokio::join!(self.tools.drain(), self.children.drain());
        match (tools, children) {
            (Ok(()), Ok(_)) => Ok(()),
            (Err(error), _) => Err(ExecutionCleanupError::Tools(error)),
            (_, Err(error)) => Err(ExecutionCleanupError::Children(error)),
        }
    }

    async fn stop(&self, events: &mut crate::RuntimeStream) -> Result<(), ExecutionCleanupError> {
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
) -> Result<PreparedExecution, RunOutcome> {
    let tool_cancellation = Arc::new(AtomicBool::new(false));
    let internal = claimed.kind == RunKind::Compaction;
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
        ))
    };
    // The claim carried the session's file hashes, so nothing here waits on
    // the store; a cancel already recorded settles before any preparation.
    if *cancellation.borrow() {
        tool_cancellation.store(true, Ordering::Release);
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
    // fails the run with a typed outcome and no provider work.
    if !internal
        && input
            .iter()
            .any(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. }))
    {
        let workspace = loaded.plan.workspace_handle();
        let state = Arc::clone(&file_state);
        let parts = input;
        let resolved = tokio::select! {
            result = tokio::task::spawn_blocking(move || {
                crate::input::resolve_blocking(&parts, &workspace, &state)
            }) => result,
            changed = cancellation.changed() => {
                tool_cancellation.store(true, Ordering::Release);
                return if changed.is_ok() && *cancellation.borrow() {
                    Err(RunOutcome::Cancelled)
                } else {
                    Err(RunOutcome::Interrupted)
                };
            }
        };
        let text = match resolved {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => {
                tool_cancellation.store(true, Ordering::Release);
                return Err(RunOutcome::Failed {
                    failure: RunFailure {
                        kind: error.failure_kind(),
                        message: truncate_utf8(error.to_string(), MAX_FAILURE_MESSAGE_BYTES),
                    },
                });
            }
            Err(_) => {
                tool_cancellation.store(true, Ordering::Release);
                return Err(internal_failure("input resolution stopped unexpectedly"));
            }
        };
        match messages.pop() {
            Some(prompt) if prompt.role() == Role::User => messages.push(Message::user(text)),
            Some(_) | None => {
                return Err(internal_failure(
                    "assembled context did not end with the run's prompt",
                ));
            }
        }
    }
    let capabilities = if internal {
        RunCapabilities::restricted()
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
            tool_cancellation.store(true, Ordering::Release);
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
                steering.insert(claimed.run_id, sender);
            }
            Err(_) => {
                tool_cancellation.store(true, Ordering::Release);
                return Err(internal_failure("steering registry is poisoned"));
            }
        }
        let base = base
            .with_limits(claimed.limits, loaded.resolved_model().pricing.clone())
            .with_history(Arc::new(SessionHistorySearcher::new(
                Arc::clone(inner),
                claimed.session_id,
                claimed.run_id,
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
    .with_tool_tasks(resources.tools.clone());
    // The claimed workspace is the plan's workspace: the loader compiled the
    // plan for exactly this session's canonical root, so no per-run
    // canonicalization or directory open happens here.
    let mut events = loaded.plan.execute(
        messages,
        Arc::clone(&tool_cancellation),
        gate,
        file_state,
        capabilities,
    );
    loop {
        let event = tokio::select! {
            biased;
            changed = cancellation.changed() => {
                tool_cancellation.store(true, Ordering::Release);
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
                if claimed.kind == RunKind::Prompt && weight.compatible_input_tokens.is_none() {
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
            Some(_) | None => {
                return Err(internal_failure(
                    "runtime preparation ended without an initial prepared request",
                ));
            }
        }
    }
}

pub(super) async fn execute_run(
    inner: Arc<SessionRuntimeInner>,
    mut claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    resources: RunResources,
) {
    if *cancellation.borrow() {
        finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }
    let mut load = inner.loader.load(RuntimeLoadRequest {
        workspace: claimed.workspace.clone(),
        model: claimed.model.clone(),
        profile: claimed.profile.clone(),
    });
    let loaded = tokio::select! {
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
    loop {
        let prepared =
            match prepare_execution(&inner, &mut claimed, &loaded, &mut cancellation, &resources)
                .await
            {
                Ok(prepared) => prepared,
                Err(outcome) => {
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
            compaction: if claimed.kind == RunKind::Compaction
                || claimed.context_compaction_attempted
            {
                context::CompactionDisposition::AlreadyAttempted
            } else {
                context::CompactionDisposition::Eligible
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
            && claimed.kind == RunKind::Prompt;
        if repeats_known_overflow {
            if claimed.context_compaction_attempted {
                let context::ContextPlan::Send { estimate } = plan else {
                    unreachable!("known overflow override only applies to a send plan")
                };
                finish_prepared_run(
                    &inner,
                    &claimed,
                    &prepared.audit,
                    planned_context_failure(context::ContextPlan::Reject {
                        estimate,
                        reason: context::ContextRejectReason::ProviderReportedOverflow,
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
            )
            .await
            {
                return;
            }
            continue;
        }
        match plan {
            context::ContextPlan::Send { .. } => {
                let started = loop {
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
                    let result = inner
                        .store
                        .start_reserved_run(&claimed, prepared.audit.clone())
                        .await;
                    if matches!(result, Err(SessionRuntimeError::Overloaded)) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        continue;
                    }
                    break result;
                };
                let started = match started {
                    Ok(Some(started)) => started,
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
                inner.notify(started.cursor);
                claimed.model = ModelSelection {
                    model: Some(prepared.audit.resolved_model.route.clone()),
                    max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
                    organization: prepared.audit.resolved_model.organization.clone(),
                };
                let cancelled =
                    match cancellation_requested_with_retry(&inner, claimed.run_id).await {
                        Ok(cancelled) => cancelled || *cancellation.borrow(),
                        Err(error) => {
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure("failed to re-read run cancellation", &error),
                            )
                            .await;
                            return;
                        }
                    };
                if *inner.failed.borrow() {
                    prepared.tool_cancellation.store(true, Ordering::Release);
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("session runtime failed before provider work"),
                    )
                    .await;
                    return;
                }
                if cancelled {
                    prepared.tool_cancellation.store(true, Ordering::Release);
                    finish_run(&inner, &claimed, RunOutcome::Cancelled).await;
                    return;
                }
                execute_started_run(
                    inner,
                    claimed,
                    cancellation,
                    prepared.events,
                    prepared.tool_cancellation,
                    &prepared.audit,
                    &resources,
                )
                .await;
                return;
            }
            context::ContextPlan::Compact { .. } if claimed.kind == RunKind::Prompt => {
                let audit = prepared.audit.clone();
                drop(prepared);
                if !run_auto_compaction(
                    &inner,
                    &mut claimed,
                    &loaded,
                    audit,
                    &mut cancellation,
                    &resources,
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

async fn run_auto_compaction(
    inner: &Arc<SessionRuntimeInner>,
    original: &mut ClaimedRun,
    loaded: &LoadedRuntime,
    original_audit: PreparedRunAudit,
    cancellation: &mut watch::Receiver<bool>,
    resources: &RunResources,
) -> bool {
    let messages = loop {
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
        let result = inner
            .store
            .load_auto_compaction_messages(original.session_id)
            .await;
        if matches!(result, Err(SessionRuntimeError::Overloaded)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        break result;
    };
    let messages = match messages {
        Ok(messages) => messages,
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
    candidate.run_id = match RunId::generate() {
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
    candidate.command_id = match CommandId::generate() {
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
    candidate.kind = RunKind::Compaction;
    candidate.user_initiated = false;
    candidate.literal_slash = false;
    candidate.messages = messages;
    candidate.context_compaction_attempted = true;
    candidate.context_occupancy = None;
    let prepared =
        match prepare_execution(inner, &mut candidate, loaded, cancellation, resources).await {
            Ok(prepared) => prepared,
            Err(outcome) => {
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
        compaction: context::CompactionDisposition::AlreadyAttempted,
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
    let started = loop {
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
        let result = inner
            .store
            .start_auto_compaction(original, prepared.audit.clone())
            .await;
        if matches!(result, Err(SessionRuntimeError::Overloaded)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        break result;
    };
    let (mut compaction, started) = match started {
        Ok(Some(started)) => started,
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
        cancellations.insert(compaction.run_id, compaction_cancel);
    } else {
        inner.failed.send_replace(true);
        let outcome = internal_failure("run cancellation registry is unavailable");
        finish_run(inner, &compaction, outcome.clone()).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    inner.notify(started.cursor);
    compaction.model = ModelSelection {
        model: Some(prepared.audit.resolved_model.route.clone()),
        max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
        organization: prepared.audit.resolved_model.organization.clone(),
    };
    let cancelled = match cancellation_requested_with_retry(inner, compaction.run_id).await {
        Ok(cancelled) => cancelled,
        Err(error) => {
            let outcome = persistence_failure("failed to re-read compaction cancellation", &error);
            finish_run(inner, &compaction, outcome.clone()).await;
            finish_prepared_run(inner, original, &original_audit, outcome).await;
            return false;
        }
    };
    if *inner.failed.borrow() {
        prepared.tool_cancellation.store(true, Ordering::Release);
        let outcome = internal_failure("session runtime failed before compaction provider work");
        finish_run(inner, &compaction, outcome.clone()).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    let compaction_run_id = compaction.run_id;
    if cancelled {
        prepared.tool_cancellation.store(true, Ordering::Release);
        finish_run(inner, &compaction, RunOutcome::Cancelled).await;
    } else {
        execute_started_run(
            Arc::clone(inner),
            compaction,
            compaction_cancellation,
            prepared.events,
            prepared.tool_cancellation,
            &prepared.audit,
            resources,
        )
        .await;
    }
    if *inner.failed.borrow() {
        return false;
    }
    let committed = loop {
        let result = inner
            .store
            .compaction_committed(original.session_id, compaction_run_id)
            .await;
        if matches!(result, Err(SessionRuntimeError::Overloaded)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        break result;
    };
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
    loop {
        match inner.store.reload_reserved_messages(original).await {
            Ok(Some((messages, attempted))) => {
                original.messages = messages;
                original.context_compaction_attempted = attempted;
                if compacted {
                    original.context_overflow_basis = None;
                    original.context_occupancy = None;
                }
                return true;
            }
            Ok(None) => {
                clear_run_registration(inner, original.run_id);
                return false;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
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
                return false;
            }
        }
    }
}

async fn cancellation_requested_with_retry(
    inner: &SessionRuntimeInner,
    run_id: RunId,
) -> Result<bool, SessionRuntimeError> {
    loop {
        if *inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let result = inner.store.cancellation_requested(run_id).await;
        if *inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        match result {
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            result => return result,
        }
    }
}

async fn finish_reserved_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) {
    loop {
        match inner
            .store
            .finish_reserved_run(claimed, outcome.clone())
            .await
        {
            Ok(events) => {
                for event in events {
                    inner.notify(event.cursor);
                }
                inner
                    .settlements
                    .send_modify(|generation| *generation = generation.wrapping_add(1));
                clear_run_registration(inner, claimed.run_id);
                return;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(_) => {
                inner.failed.send_replace(true);
                return;
            }
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
    loop {
        match inner
            .store
            .finish_prepared_run(claimed, audit.clone(), outcome.clone())
            .await
        {
            Ok(events) => {
                for event in events {
                    inner.notify(event.cursor);
                }
                inner
                    .settlements
                    .send_modify(|generation| *generation = generation.wrapping_add(1));
                clear_run_registration(inner, claimed.run_id);
                return;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
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
                return;
            }
        }
    }
}

async fn execute_started_run(
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    mut events: crate::RuntimeStream,
    tool_cancellation: Arc<AtomicBool>,
    audit: &PreparedRunAudit,
    resources: &RunResources,
) {
    let resolved_model = Arc::clone(&audit.resolved_model);
    let context_shape = audit.context_shape;
    let initial_occupancy_basis = context_occupancy_basis(
        context_shape.digest,
        audit.static_prefix,
        audit.weight.input_bytes(),
    );
    let mut runtime_failed = inner.failed.subscribe();
    if *runtime_failed.borrow() {
        tool_cancellation.store(true, Ordering::Release);
        if resources.stop(&mut events).await.is_err() {
            inner.failed.send_replace(true);
            return;
        }
        finish_run(
            &inner,
            &claimed,
            internal_failure("session runtime failed before provider work"),
        )
        .await;
        return;
    }
    let internal = claimed.kind == RunKind::Compaction;
    let mut accounting =
        RunAccountingAccumulator::new(resolved_model.pricing.clone(), initial_occupancy_basis);
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
    let mut current_turn: u16 = 1;
    let mut current_message: Option<MessageId> = None;
    let mut current_occupancy_basis = Some(initial_occupancy_basis);
    // Live tool output batches on the same timer as model text. Text and tool
    // output never accumulate at the same time: a turn's text is fully
    // flushed when the turn completes, before any of its calls execute.
    let mut pending_tool_call: Option<ToolCallId> = None;
    let mut pending_tool_output = String::new();
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
            if resources.stop(&mut events).await.is_err() {
                inner.failed.send_replace(true);
                return;
            }
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist model output", &error),
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
            if resources.stop(&mut events).await.is_err() {
                inner.failed.send_replace(true);
                return;
            }
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist tool output", &error),
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
            if resources.stop(&mut events).await.is_err() {
                inner.failed.send_replace(true);
                return;
            }
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist reasoning", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                    )
                    .await;
                    return;
                }
                flush_at = None;
            }
            stopped @ (RunInput::Cancelled | RunInput::Interrupted) => {
                tool_cancellation.store(true, Ordering::Release);
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
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
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
                    return;
                }
                finish_run_accounted(&inner, &claimed, outcome, Some(accounting.snapshot())).await;
                return;
            }
            RunInput::RuntimeFailed => {
                tool_cancellation.store(true, Ordering::Release);
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    internal_failure("session runtime failed during provider work"),
                    Some(accounting.snapshot()),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist the run prompt identity", &error),
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
                    // auto-compaction; later turns and compaction runs must
                    // fail closed without polling the provider.
                    compaction: if internal {
                        context::CompactionDisposition::AlreadyAttempted
                    } else {
                        context::CompactionDisposition::BetweenRunsOnly
                    },
                });
                match plan {
                    context::ContextPlan::Send { .. } => {}
                    plan => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(&inner, &claimed, planned_context_failure(plan)).await;
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
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist run activity", &error),
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
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
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
                        Ok(event) => inner.notify(event.cursor),
                        Err(error) => {
                            if resources.stop(&mut events).await.is_err() {
                                inner.failed.send_replace(true);
                                return;
                            }
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure("failed to persist reasoning", &error),
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
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
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
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
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
                        Ok(events) => {
                            for event in events {
                                inner.notify(event.cursor);
                            }
                        }
                        Err(error) => {
                            if resources.stop(&mut events).await.is_err() {
                                inner.failed.send_replace(true);
                                return;
                            }
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure(
                                    "failed to persist the completed compaction turn",
                                    &error,
                                ),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
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
                    Ok(events) => {
                        for event in events {
                            inner.notify(event.cursor);
                        }
                    }
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure(
                                "failed to persist the completed model turn",
                                &error,
                            ),
                        )
                        .await;
                        return;
                    }
                }
            }
            // Approval transitions (including denials) are persisted and
            // published by the tool gate before this event is emitted.
            RunInput::Event(Some(RuntimeEvent::ToolCallDenied { .. })) => {}
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
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the audit record", &error),
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
            RunInput::Event(Some(RuntimeEvent::ToolCallStarted { id })) => {
                if internal {
                    continue;
                }
                match inner.store.start_tool_call(&claimed, id).await {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the started tool call", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
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
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist tool output", &error),
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
                file_state,
                display,
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                    )
                    .await;
                    return;
                }
                match inner
                    .store
                    .finish_tool_call(&claimed, id, result, is_error, file_state, display)
                    .await
                {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the tool result", &error),
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
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist model output", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
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
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist model output", &error),
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
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist applied steering", &error),
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
                    Ok(events) => {
                        for event in events {
                            inner.notify(event.cursor);
                        }
                    }
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the interrupted turn", &error),
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
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the truncated turn", &error),
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::Completed)) => {
                if internal {
                    let summary = std::mem::take(&mut summary_text);
                    if summary.trim().is_empty() {
                        if resources.stop(&mut events).await.is_err() {
                            inner.failed.send_replace(true);
                            return;
                        }
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
                        )
                        .await;
                        return;
                    }
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    match inner
                        .store
                        .finish_compaction_run(&claimed, summary, Some(accounting.snapshot()))
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
                        cancellations.remove(&claimed.run_id);
                    }
                    inner.clear_run_approvals(claimed.run_id);
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Completed,
                    Some(accounting.snapshot()),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::BudgetExhausted {
                        exhaustion: Box::new(exhaustion),
                    },
                    Some(accounting.snapshot()),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
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
                    if resources.stop(&mut events).await.is_err() {
                        inner.failed.send_replace(true);
                        return;
                    }
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                if resources.stop(&mut events).await.is_err() {
                    inner.failed.send_replace(true);
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    internal_failure("model stream ended without a terminal event"),
                    Some(accounting.snapshot()),
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
    let event = inner
        .store
        .append_reasoning(claimed, ReasoningEvent::Delta { kind, text })
        .await?;
    inner.notify(event.cursor);
    Ok(())
}

async fn flush_pending_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    current_turn: u16,
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
    let event = inner
        .store
        .append_tool_output(claimed, tool_call_id, chunk)
        .await?;
    inner.notify(event.cursor);
    Ok(())
}

/// Persists model text into the current turn's assistant message, creating
/// that message on the turn's first chunk.
async fn persist_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    current_turn: u16,
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
                let event = inner
                    .store
                    .append_text(claimed, message_id, channel, chunk)
                    .await?;
                inner.notify(event.cursor);
            }
            None => {
                let message_id =
                    MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                let events = inner
                    .store
                    .begin_assistant_message(claimed, message_id, current_turn, channel, chunk)
                    .await?;
                for event in events {
                    inner.notify(event.cursor);
                }
                *current_message = Some(message_id);
            }
        }
        remaining = &remaining[end..];
    }
    Ok(())
}

pub(super) async fn finish_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) {
    finish_run_accounted(inner, claimed, outcome, None).await;
}

async fn finish_run_accounted(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) {
    loop {
        match inner
            .store
            .finish_run(claimed, outcome.clone(), accounting.clone())
            .await
        {
            Ok(events) => {
                for event in events {
                    inner.notify(event.cursor);
                }
                inner
                    .settlements
                    .send_modify(|generation| *generation = generation.wrapping_add(1));
                clear_run_registration(inner, claimed.run_id);
                return;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(_) => {
                inner.failed.send_replace(true);
                return;
            }
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
}

pub(super) struct RunAccountingAccumulator {
    usage: Option<TokenUsage>,
    context_tokens: Option<u64>,
    estimated_cost_usd_nanos: Option<u64>,
    pricing: Option<ModelPricing>,
    saw_turn: bool,
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
            request_basis,
        }
    }

    /// Adds a reviewer's provider spend to the run's totals. It is not a turn
    /// of this run (context occupancy is untouched) but the run is
    /// accountable for it; unknown spend makes the totals unknown.
    pub(super) fn record_review(&mut self, usage: Option<TokenUsage>, cost_usd_nanos: Option<u64>) {
        self.saw_turn = true;
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
            usage: self.saw_turn.then_some(self.usage).flatten(),
            context_tokens: self.context_tokens,
            estimated_cost_usd_nanos: self
                .saw_turn
                .then_some(self.estimated_cost_usd_nanos)
                .flatten(),
            saw_turn: self.saw_turn,
            request_basis: self.request_basis,
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
    pub(super) turn_ordinal: u16,
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
