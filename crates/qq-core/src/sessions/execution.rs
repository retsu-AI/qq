use super::*;
use super::{
    approvals::SessionToolGate,
    runtime::SessionRuntimeInner,
    subagents::{ChildTasks, SessionAuditHook, SessionEvidenceReader, SessionSubagentSpawner},
};
use crate::runtime::RunDeadline;
use crate::tools;

struct CompactionRunGate;

#[cfg(all(test, unix))]
struct BufferedToolOutputHook {
    tool_call_id: ToolCallId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(all(test, unix))]
static BUFFERED_TOOL_OUTPUT_HOOKS: Mutex<Vec<BufferedToolOutputHook>> = Mutex::new(Vec::new());

#[cfg(all(test, unix))]
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

#[cfg(all(test, unix))]
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

fn session_prompt_prefix_key(
    claimed: &ClaimedRun,
    loaded: &LoadedRuntime,
) -> crate::plan::PromptPrefixKey {
    let delegation = &loaded.plan.descriptor().delegation;
    let user = claimed.depth == 0;
    crate::plan::PromptPrefixKey {
        tools: Some(crate::catalog::StaticFilter {
            spawn_agent: claimed.depth < delegation.max_depth.min(MAX_CHILD_DEPTH),
            search_history: true,
            update_goal: claimed.goal.is_some(),
            read_tool_result: true,
            load_skill: user,
            read_only: claimed.approval_mode == ApprovalMode::ReadOnly,
            unattended: false,
        }),
        guidance: user,
        subagent: (!user && claimed.purpose == SessionPurpose::Task).then_some(
            match claimed.approval_mode {
                ApprovalMode::ReadOnly => crate::runtime::SubagentAuthority::Read,
                ApprovalMode::Ask | ApprovalMode::Auto | ApprovalMode::Full => {
                    crate::runtime::SubagentAuthority::Write
                }
            },
        ),
        mode: claimed.approval_mode,
        snapshots: !claimed.identity.child,
    }
}

struct GoalDeadlineRegistration {
    inner: Arc<SessionRuntimeInner>,
    run: RunId,
}

impl Drop for GoalDeadlineRegistration {
    fn drop(&mut self) {
        match self.inner.goal_deadlines.lock() {
            Ok(mut deadlines) => {
                deadlines.remove(&self.run);
            }
            Err(_) => {
                self.inner.failed.send_replace(true);
            }
        }
    }
}

struct PreparedExecution {
    goal_paid: Option<super::GoalPaidInput>,
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

#[derive(Clone, Debug)]
pub(super) struct TeardownComplete(Vec<(ToolCallId, crate::tools::ToolOutput)>);

impl TeardownComplete {
    #[cfg(test)]
    pub(super) const fn nothing_ran() -> Self {
        Self(Vec::new())
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
    pub(super) fn process_lease(&self) -> crate::tools::ToolTaskLease {
        self.tools.enter()
    }

    #[cfg(test)]
    pub(super) fn for_test_run(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }
    pub(super) async fn drain(&self) -> Result<TeardownComplete, ExecutionCleanupError> {
        let (tools, children) = tokio::join!(self.tools.drain(), self.children.drain());
        match (tools, children) {
            (Ok(()), Ok(_)) => Ok(TeardownComplete(self.tools.take_finished())),
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
    input_workspace: &crate::workspace::Workspace,
    cancellation: &mut watch::Receiver<bool>,
    resources: &RunResources,
    execution_started: tokio::time::Instant,
) -> Result<PreparedExecution, RunOutcome> {
    let deadline = RunDeadline::new(claimed.limits, execution_started);
    if let Some(deadline) = deadline
        && claimed.identity.kind == RunKind::Prompt
        && (claimed.goal.is_some() || claimed.goal_spend_owner.is_some())
    {
        match inner.goal_deadlines.lock() {
            Ok(mut deadlines) => {
                deadlines.insert(claimed.identity.run_id, deadline);
            }
            Err(_) => return Err(internal_failure("goal deadline registry is unavailable")),
        }
    }
    if let Some(deadline) = deadline.filter(|deadline| deadline.expired()) {
        return Err(RunOutcome::BudgetExhausted {
            exhaustion: Box::new(deadline.exhaustion()),
        });
    }
    let tool_cancellation = RunCancellation::new();
    let internal = claimed.identity.kind == RunKind::Compaction;
    let mut messages = std::mem::take(&mut claimed.messages);
    let inherited_effects = std::mem::take(&mut claimed.message_effects);
    let input = std::mem::take(&mut claimed.input);
    let gate: Arc<dyn ToolGate> = if internal {
        Arc::new(CompactionRunGate)
    } else {
        Arc::new(SessionToolGate::new(
            Arc::clone(inner),
            claimed.clone(),
            cancellation.clone(),
            loaded.plan.network_policy(),
            loaded.plan.shell_policy(),
        ))
    };
    if *cancellation.borrow() {
        tool_cancellation.cancel();
        return Err(RunOutcome::Cancelled);
    }
    let file_state = Arc::new(FileState::with_entries(std::mem::take(
        &mut claimed.file_state,
    )));
    if !internal
        && (claimed.resolved_input.is_some()
            || input
                .iter()
                .any(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. })))
    {
        let resolved = if let Some(resolved) = &claimed.resolved_input {
            Arc::clone(resolved)
        } else {
            let workspace = input_workspace.clone();
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
    let goal_paid = super::GoalPaidInput::new(
        &inner.store,
        claimed,
        loaded.resolved_model().pricing.as_ref(),
    );
    let mut capabilities = if internal {
        RunCapabilities::restricted()
            .with_goal_paid(goal_paid.clone())
            .with_goal_spend_tracking(claimed.goal_spend_owner.is_some())
            .with_limits(
                RunLimits {
                    max_duration_ms: claimed.limits.max_duration_ms,
                    ..RunLimits::default()
                },
                None,
            )
            .summarizer(session_prompt_prefix_key(claimed, loaded))
            .with_max_output_tokens(context::summarizer_output_tokens(
                loaded.resolved_model().max_output_tokens,
                loaded.resolved_model().context_window,
            ))
    } else {
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
        let delegation = &loaded.plan.descriptor().delegation;
        let effective_depth = delegation.max_depth.min(MAX_CHILD_DEPTH);
        let spawner = if claimed.depth < effective_depth {
            Some(Arc::new(
                SessionSubagentSpawner::new(
                    Arc::clone(inner),
                    claimed.clone(),
                    Arc::clone(&resources.children),
                    tools::spawn_model_routes(
                        &loaded.plan.descriptor().tools.spawn_model_routes,
                        delegation,
                    )
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
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
        let base = if claimed.depth > 0
            && !claimed.user_initiated
            && claimed.purpose == SessionPurpose::Task
        {
            base.for_subagent(match claimed.approval_mode {
                ApprovalMode::ReadOnly => crate::runtime::SubagentAuthority::Read,
                ApprovalMode::Ask | ApprovalMode::Auto | ApprovalMode::Full => {
                    crate::runtime::SubagentAuthority::Write
                }
            })
        } else if claimed.depth > 0
            && !claimed.user_initiated
            && claimed.purpose == SessionPurpose::Audit
        {
            base.stall_exempt()
        } else {
            base
        };
        let base = if claimed.approval_mode == ApprovalMode::ReadOnly {
            base.read_only()
        } else {
            base
        };
        let (sender, receiver) = crate::runtime::steering_channel();
        for message in std::mem::take(&mut claimed.pending_steering) {
            match sender.messages.try_send(message) {
                Ok(()) => {}
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
        let session_reader = Arc::new(SessionEvidenceReader::new(
            Arc::clone(inner),
            claimed.identity.session_id,
        ));
        let base = base
            .with_limits(claimed.limits, loaded.resolved_model().pricing.clone())
            .with_goal_paid(goal_paid.clone())
            .with_goal_spend_tracking(claimed.goal_spend_owner.is_some())
            .with_goal(super::goals::GoalRunInput::new(
                inner.store.clone(),
                claimed,
            ))
            .with_history(session_reader.clone())
            .with_spills(session_reader)
            .with_steering(receiver)
            .with_compactor(Arc::new(
                super::in_run_compaction::SessionInRunCompactor::new(
                    Arc::clone(inner),
                    claimed,
                    loaded,
                ),
            ));
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
    .with_cache_key(claimed.identity.session_id.to_string())
    .with_execution_started(execution_started)
    .with_tool_tasks(resources.tools.clone())
    .with_output(claimed.output.clone())
    .with_inherited_effects(inherited_effects);
    if !claimed.identity.child {
        capabilities = capabilities.in_session(claimed.approval_mode);
    } else {
        capabilities.mode = claimed.approval_mode;
    }
    if claimed
        .goal
        .as_ref()
        .is_some_and(|goal| goals::is_model_audit(goal, claimed.origin))
    {
        capabilities = capabilities.without_tools();
        capabilities.audit_hook = None;
        capabilities.spawner = None;
    }
    if !internal
        && (claimed.goal.is_some() || claimed.goal_spend_owner.is_some())
        && let Some(tokens) = [
            claimed.limits.max_total_tokens,
            claimed.limits.max_output_tokens,
        ]
        .into_iter()
        .flatten()
        .min()
    {
        capabilities =
            capabilities.with_max_output_tokens(u32::try_from(tokens).unwrap_or(u32::MAX));
    }
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
            Some(RuntimeEvent::GoalPaidCancelled) => return Err(RunOutcome::Cancelled),
            Some(RuntimeEvent::Started) => {}
            Some(RuntimeEvent::Prepared {
                turn_ordinal: 1,
                identity: Some(prompt_identity),
                static_prefix,
                mut weight,
            }) => {
                let mut resolved_model = loaded.resolved_model().as_ref().clone();
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
                    goal_paid,
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

fn per_run_duration_exhaustion() -> RunOutcome {
    RunOutcome::BudgetExhausted {
        exhaustion: Box::new(qq_protocol::BudgetExhaustion {
            limit: qq_protocol::BudgetLimitKind::Duration,
            final_response: false,
            message: "run duration expired before the next provider request started".to_owned(),
        }),
    }
}

fn goal_admission_exhaustion(reason: qq_protocol::GoalExhaustion) -> RunOutcome {
    use qq_protocol::{BudgetLimitKind, GoalExhaustion};
    let limit = match reason {
        GoalExhaustion::Deadline => BudgetLimitKind::Duration,
        GoalExhaustion::Runs => BudgetLimitKind::ModelTurns,
        GoalExhaustion::Tokens => BudgetLimitKind::TotalTokens,
        GoalExhaustion::TokensUnknown => BudgetLimitKind::TokensUnknown,
        GoalExhaustion::Cost => BudgetLimitKind::Cost,
        GoalExhaustion::CostUnknown => BudgetLimitKind::CostUnknown,
    };
    RunOutcome::BudgetExhausted {
        exhaustion: Box::new(qq_protocol::BudgetExhaustion {
            limit,
            final_response: false,
            message: format!(
                "goal admission refused: {}; no main model request started",
                limit.as_str()
            ),
        }),
    }
}

pub(super) async fn execute_run(
    inner: Arc<SessionRuntimeInner>,
    mut claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    resources: RunResources,
) {
    let process_store = inner.store.clone();
    let process_identity = claimed.identity;
    if let Err(error) = resources.tools.set_process_recorder(move |id, receipt| {
        let store = process_store.clone();
        async move {
            store
                .record_tool_process(process_identity, id, receipt)
                .await
        }
    }) {
        finish_reserved_run(&inner, &claimed, internal_failure(&error.to_string())).await;
        return;
    }
    match goal_policy::refresh(&inner, &claimed, &mut cancellation).await {
        Ok(true) => {}
        Ok(false) => {
            finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
            return;
        }
        Err(error) => {
            finish_reserved_run(
                &inner,
                &claimed,
                persistence_failure("failed to apply current workspace goal policy", &error),
            )
            .await;
            return;
        }
    }
    if claimed.identity.kind == RunKind::GoalCheck {
        super::goal_check::execute(inner, claimed, cancellation, resources).await;
        return;
    }

    let _goal_deadline =
        (claimed.goal.is_some() || claimed.goal_spend_owner.is_some()).then(|| {
            GoalDeadlineRegistration {
                inner: Arc::clone(&inner),
                run: claimed.identity.run_id,
            }
        });
    let execution_started = tokio::time::Instant::now();
    if let Some(admission) = claimed.goal_admission {
        let refusal = match admission {
            goals::GoalAdmission::Cancelled => Some(RunOutcome::Cancelled),
            goals::GoalAdmission::Exhausted(reason) => Some(goal_admission_exhaustion(reason)),
            goals::GoalAdmission::Allowed {
                deadline_ms,
                final_slot,
                ..
            } => {
                let anchor_ms = now_ms();
                claimed.goal_admission = Some(goals::GoalAdmission::Allowed {
                    final_slot,
                    deadline_ms,
                    anchor_ms,
                    execution_started: Some(execution_started),
                });
                let remaining = deadline_ms.saturating_sub(anchor_ms);
                if remaining == 0 {
                    Some(goal_admission_exhaustion(
                        qq_protocol::GoalExhaustion::Deadline,
                    ))
                } else {
                    claimed.limits.max_duration_ms = Some(
                        claimed
                            .limits
                            .max_duration_ms
                            .map_or(remaining, |cap| cap.min(remaining)),
                    );
                    None
                }
            }
        };
        if let Some(outcome) = refusal {
            finish_reserved_run(&inner, &claimed, outcome).await;
            return;
        }
    }
    let deadline = RunDeadline::new(claimed.limits, execution_started);
    if *cancellation.borrow() {
        finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }
    if let Some(failure) = claimed.history_admission_failure {
        let outcome = match failure {
            HistoryAdmissionFailure::ContextTooLarge => context_budget_failure(),
            HistoryAdmissionFailure::MissingToolProjection(run) => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Server,
                    message: SessionRuntimeError::ToolProjectionUnavailable {
                        run: run.to_string(),
                    }
                    .to_string(),
                },
            },
        };
        finish_reserved_run(&inner, &claimed, outcome).await;
        return;
    }
    let load_progress = RuntimeLoadProgress::default();
    let mut load = inner.loader.load_with_progress(
        RuntimeLoadRequest {
            reasoning_effort: claimed.reasoning_effort,
            workspace: claimed.workspace.clone(),
            model: claimed.model.clone(),
            profile: claimed.profile.clone(),
        },
        load_progress.clone(),
    );
    let loaded = tokio::select! {
        biased;
        () = RunDeadline::wait(deadline) => {
            let unfinished_stage = load_progress.stage();
            load_progress.cancel();
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
                load_progress.cancel();
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
    let input_workspace = loaded.plan.workspace_handle();
    match goal_policy::refresh(&inner, &claimed, &mut cancellation).await {
        Ok(true) => {}
        Ok(false) => {
            finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
            return;
        }
        Err(error) => {
            finish_reserved_run(
                &inner,
                &claimed,
                persistence_failure(
                    "failed to refresh goal policy before paid preparation",
                    &error,
                ),
            )
            .await;
            return;
        }
    }
    let mut bounded_manual_compaction = false;
    let mut summary_only_admission = false;
    let mut watermark_advanced = false;
    loop {
        let mut prepared = match prepare_execution(
            &inner,
            &mut claimed,
            &loaded,
            &input_workspace,
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
            && !summary_only_admission
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
                if !summary_only_admission {
                    summary_only_admission = true;
                    drop(prepared);
                    if !admit_with_summary_only_history(&inner, &mut claimed).await {
                        return;
                    }
                    continue;
                }
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
                    claimed.message_effects = Vec::new();
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
                match goal_policy::refresh(&inner, &claimed, &mut cancellation).await {
                    Ok(true) => {}
                    Ok(false) => {
                        finish_prepared_run(
                            &inner,
                            &claimed,
                            &prepared.audit,
                            RunOutcome::Cancelled,
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        finish_prepared_run(
                            &inner,
                            &claimed,
                            &prepared.audit,
                            persistence_failure(
                                "failed to apply current workspace goal policy",
                                &error,
                            ),
                        )
                        .await;
                        return;
                    }
                }
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
                    Ok(Ok(Some(_))) => {}
                    Ok(Ok(None)) => {
                        finish_reserved_run(
                            &inner,
                            &claimed,
                            internal_failure("run preparation lost its durable reservation"),
                        )
                        .await;
                        return;
                    }
                    Ok(Err(claim::GoalStartRefusal::Reprepare { limits, final_slot })) => {
                        drop(prepared);
                        claimed.limits = *limits;
                        if let Some(goals::GoalAdmission::Allowed {
                            final_slot: admitted,
                            ..
                        }) = &mut claimed.goal_admission
                        {
                            *admitted = final_slot;
                        }
                        if summary_only_admission {
                            if !admit_with_summary_only_history(&inner, &mut claimed).await {
                                return;
                            }
                        } else {
                            match inner.store.reload_reserved_messages(&claimed).await {
                                Ok(Some((messages, effects, _))) => {
                                    claimed.messages = messages;
                                    claimed.message_effects = effects;
                                }
                                Ok(None) => {
                                    finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled)
                                        .await;
                                    return;
                                }
                                Err(error) => {
                                    finish_reserved_run(&inner, &claimed, persistence_failure("failed to reload the reserved prompt after budget refresh", &error)).await;
                                    return;
                                }
                            }
                        }
                        continue;
                    }
                    Ok(Err(refusal)) => {
                        let outcome = match refusal {
                            claim::GoalStartRefusal::Exhausted(reason) => {
                                goal_admission_exhaustion(reason)
                            }
                            claim::GoalStartRefusal::Cancelled => RunOutcome::Cancelled,
                            claim::GoalStartRefusal::PerRunDuration => {
                                per_run_duration_exhaustion()
                            }
                            claim::GoalStartRefusal::Reprepare { .. } => {
                                unreachable!("reprepare handled before terminal refusal")
                            }
                        };
                        finish_reserved_run(&inner, &claimed, outcome).await;
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
                    model_is_fallback: claimed.session_model.model_is_fallback,
                    model: Some(prepared.audit.resolved_model.route.clone()),
                    max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
                    organization: prepared.audit.resolved_model.organization.clone(),
                };
                let goal_spend = claimed.goal.is_some() || claimed.goal_spend_owner.is_some();
                let initial_accounting = goal_spend.then(|| {
                    RunAccountingAccumulator::new(
                        prepared.audit.resolved_model.pricing.clone(),
                        context_occupancy_basis(
                            prepared.audit.context_shape.digest,
                            prepared.audit.static_prefix,
                            prepared.audit.weight.input_bytes(),
                        ),
                    )
                    .with_zero_spend()
                    .snapshot()
                });
                let cancelled = match cancellation_requested(&inner, claimed.identity.run_id).await
                {
                    Ok(cancelled) => cancelled || *cancellation.borrow(),
                    Err(error) => {
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
                execute_started_run(inner, claimed, cancellation, prepared, &resources).await;
                return;
            }
            context::ContextPlan::Compact { .. } if claimed.identity.kind == RunKind::Prompt => {
                let audit = prepared.audit.clone();
                drop(prepared);
                if !watermark_advanced {
                    watermark_advanced = true;
                    match inner
                        .store
                        .advance_prune_watermark_before_prompt(&claimed)
                        .await
                    {
                        Ok(false) => {}
                        Ok(true) => match inner.store.reload_reserved_messages(&claimed).await {
                            Ok(Some((messages, effects, _))) => {
                                claimed.messages = messages;
                                claimed.message_effects = effects;
                                continue;
                            }
                            Ok(None) => {
                                finish_prepared_run(
                                    &inner,
                                    &claimed,
                                    &audit,
                                    RunOutcome::Cancelled,
                                )
                                .await;
                                return;
                            }
                            Err(error) => {
                                finish_prepared_run(
                                    &inner,
                                    &claimed,
                                    &audit,
                                    persistence_failure(
                                        "failed to reload the reserved prompt after pruning",
                                        &error,
                                    ),
                                )
                                .await;
                                return;
                            }
                        },
                        Err(error) => {
                            finish_prepared_run(
                                &inner,
                                &claimed,
                                &audit,
                                persistence_failure(
                                    "failed to persist the prune watermark",
                                    &error,
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                }
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
            context::ContextPlan::Reject {
                reason:
                    context::ContextRejectReason::Exhausted(
                        _,
                        context::CompactionExhaustion::Attempted { .. },
                    ),
                ..
            } if claimed.identity.kind == RunKind::Prompt && !summary_only_admission => {
                summary_only_admission = true;
                drop(prepared);
                if !admit_with_summary_only_history(&inner, &mut claimed).await {
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

async fn admit_with_summary_only_history(
    inner: &Arc<SessionRuntimeInner>,
    claimed: &mut ClaimedRun,
) -> bool {
    let summary = match inner.store.summary_only_history(claimed.identity).await {
        Ok(summary) => summary,
        Err(error) => {
            finish_reserved_run(
                inner,
                claimed,
                persistence_failure("failed to load the latest compaction", &error),
            )
            .await;
            return false;
        }
    };
    let Ok(Some((messages, _, _))) = inner.store.reload_reserved_messages(claimed).await else {
        finish_reserved_run(
            inner,
            claimed,
            internal_failure("failed to reload the reserved prompt"),
        )
        .await;
        return false;
    };
    let Some(prompt) = messages.last().cloned() else {
        finish_reserved_run(
            inner,
            claimed,
            internal_failure("reloaded context did not end with the run's prompt"),
        )
        .await;
        return false;
    };
    let opening = match summary {
        Some(summary) => {
            format!("{SUMMARY_ONLY_NOTICE}\n\n{COMPACTION_SUMMARY_PREAMBLE}\n\n{summary}")
        }
        None => SUMMARY_ONLY_NOTICE.to_owned(),
    };
    claimed.messages = vec![Message::user(opening), prompt];
    claimed.message_effects = Vec::new();
    true
}

pub(super) fn compaction_disposition(claimed: &ClaimedRun) -> context::CompactionDisposition {
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
    let summarizer = inner
        .store
        .load_summarizer_input(
            original.identity.session_id,
            context::summarizer_message_byte_budget(
                loaded.resolved_model().context_window,
                context::summarizer_output_tokens(
                    loaded.resolved_model().max_output_tokens,
                    loaded.resolved_model().context_window,
                ),
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
    candidate.origin = Some(qq_protocol::RunOrigin::AutoCompaction {
        owner_run_id: original.identity.run_id,
    });
    candidate.user_initiated = false;
    candidate.literal_slash = false;
    candidate.messages = summarizer.messages;
    candidate.message_effects = Vec::new();
    candidate.context_compaction_attempted =
        original.context_compaction_attempted.saturating_add(1);
    candidate.compaction_cutoff_ordinal = summarizer.cutoff_ordinal;
    candidate.context_occupancy = None;
    let mut prepared = match prepare_execution(
        inner,
        &mut candidate,
        loaded,
        &loaded.plan.workspace_handle(),
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
    if let Some(paid) = prepared.goal_paid.as_ref()
        && let Err(error) = paid.bind_run(compaction.identity)
    {
        let Ok(teardown) = resources.stop(&mut prepared.events).await else {
            inner.failed.send_replace(true);
            return false;
        };
        let outcome = persistence_failure("failed to bind paid compaction identity", &error);
        finish_run(inner, &compaction, outcome.clone(), teardown).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    let (compaction_cancel, compaction_cancellation) = watch::channel(false);
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.insert(compaction.identity.run_id, compaction_cancel);
    } else {
        inner.failed.send_replace(true);
        let outcome = internal_failure("run cancellation registry is unavailable");
        let Ok(teardown) = resources.stop(&mut prepared.events).await else {
            return false;
        };
        finish_run(inner, &compaction, outcome.clone(), teardown).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    compaction.model = ModelSelection {
        model_is_fallback: false,
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
        Ok(Some((messages, effects, progress))) => {
            original.messages = messages;
            original.message_effects = effects;
            original.context_compaction_attempted = progress.steps;
            original.context_compaction_failed = progress.failed;
            original.context_compaction_remaining = progress.remaining;
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
            finish_prepared_run(inner, original, &original_audit, RunOutcome::Cancelled).await;
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

pub(super) async fn cancellation_requested(
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
    mut claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    prepared: PreparedExecution,
    resources: &RunResources,
) {
    let PreparedExecution {
        goal_paid: _,
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
    let goal_spend = claimed.goal.is_some() || claimed.goal_spend_owner.is_some();
    let mut accounting =
        RunAccountingAccumulator::new(resolved_model.pricing.clone(), initial_occupancy_basis);
    if goal_spend {
        accounting = accounting.with_zero_spend();
    }
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
            goal_spend.then(|| accounting.snapshot()),
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
    let mut current_turn: u32 = 1;
    let mut current_message: Option<MessageId> = None;
    let mut current_occupancy_basis = Some(initial_occupancy_basis);
    let mut pending_tool_call: Option<ToolCallId> = None;
    let mut pending_tool_output = String::new();
    let mut snapshot_taken = false;
    let mut summary_text = String::new();
    let mut summary_continues_truncated_turn = false;
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
                if matches!(stopped, RunInput::Cancelled)
                    && approvals::expire_goal_hold(&inner, &claimed, true)
                        .await
                        .is_err()
                {
                    inner.failed.send_replace(true);
                    return;
                }
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
                let outcome = if matches!(stopped, RunInput::Cancelled) {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
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
                accounting.goal_request_rejected = false;
                if let Some(identity) = identity
                    && let Err(error) = inner.store.record_prompt_identity(&claimed, identity).await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run_accounted(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist the run prompt identity", &error),
                        Some(accounting.snapshot()),
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
                        finish_run_accounted(
                            &inner,
                            &claimed,
                            planned_context_failure(plan),
                            Some(accounting.snapshot()),
                            teardown,
                        )
                        .await;
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
                claimed.model.max_output_tokens = Some(weight.max_output_tokens);
            }
            RunInput::Event(Some(RuntimeEvent::GoalRequestAdmitted { cursor })) => {
                if let Some(cursor) = cursor {
                    inner.notify(cursor);
                }
                accounting.goal_request_rejected = false;
            }
            RunInput::Event(Some(RuntimeEvent::GoalPaidCancelled)) => {
                let Ok(teardown) = resources.stop(&mut events).await else {
                    inner.failed.send_replace(true);
                    return;
                };
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Cancelled,
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::GoalRequestRejected)) => {
                accounting.goal_request_rejected = true;
            }
            RunInput::Event(Some(RuntimeEvent::ActivityChanged { activity })) => {
                if internal {
                    if claimed.goal_spend_owner.is_some()
                        && activity == qq_protocol::RunActivity::WaitingForProvider
                    {
                        if let Err(error) = inner.store.start_in_run_summary_request(&claimed).await
                        {
                            let Ok(teardown) = resources.stop(&mut events).await else {
                                inner.failed.send_replace(true);
                                return;
                            };
                            finish_run_accounted(
                                &inner,
                                &claimed,
                                persistence_failure(
                                    "failed to persist owned summary dispatch",
                                    &error,
                                ),
                                Some(accounting.snapshot()),
                                teardown,
                            )
                            .await;
                            return;
                        }
                        accounting.goal_request_rejected = false;
                    }
                    continue;
                }
                match inner
                    .store
                    .append_run_activity(&claimed, activity, accounting.goal_request_rejected)
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        let Ok(teardown) = resources.stop(&mut events).await else {
                            inner.failed.send_replace(true);
                            return;
                        };
                        finish_run_accounted(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist run activity", &error),
                            Some(accounting.snapshot()),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
                if activity == qq_protocol::RunActivity::WaitingForProvider {
                    accounting.goal_request_rejected = false;
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
                model_work,
                turn_ordinal,
                message,
                usage,
                calls,
                truncated,
                notice,
                goal_notices,
                reserved_goal_notices,
            })) => {
                if internal {
                    let turn_cost = usage.and_then(|usage| {
                        accounting
                            .pricing
                            .as_ref()
                            .and_then(|pricing| run_cost(usage, pricing))
                    });
                    if claimed.goal_spend_owner.is_none()
                        || model_work == crate::runtime::ModelWorkDisposition::Committed
                    {
                        accounting.record_turn(usage);
                    } else if model_work == crate::runtime::ModelWorkDisposition::Uncertain {
                        accounting.record_unattributed(None, None);
                    }
                    if calls.is_empty() {
                        if !summary_continues_truncated_turn {
                            summary_text.clear();
                        }
                        for block in message.content() {
                            if let ContentBlock::Text { text } = block {
                                summary_text.push_str(text);
                            }
                        }
                        summary_continues_truncated_turn = truncated;
                    } else {
                        summary_text.clear();
                        summary_continues_truncated_turn = false;
                    }
                    current_turn = turn_ordinal.saturating_add(1);
                    match inner
                        .store
                        .persist_model_turn(
                            &claimed,
                            ModelTurnCommit {
                                model_work,
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
                                notice,
                                goal_notices,
                                reserved_goal_notices,
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
                if (claimed.goal.is_none() && claimed.goal_spend_owner.is_none())
                    || model_work == crate::runtime::ModelWorkDisposition::Committed
                {
                    accounting.record_turn(usage);
                } else if model_work == crate::runtime::ModelWorkDisposition::Uncertain {
                    accounting.record_unattributed(None, None);
                }
                let turn_accounting = accounting.snapshot();
                let context_tokens = usage.map(turn_context_tokens);
                let occupancy_basis = usage
                    .and(current_occupancy_basis.take())
                    .filter(|_| context_shape.provider_identity);
                let turn_message = current_message.take();
                current_turn = turn_ordinal.saturating_add(1);
                match inner
                    .store
                    .persist_model_turn(
                        &claimed,
                        ModelTurnCommit {
                            model_work,
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
                            notice,
                            goal_notices,
                            reserved_goal_notices,
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
            RunInput::Event(Some(RuntimeEvent::ToolCallUnexecuted {
                id,
                message,
                interrupted,
            })) => {
                if internal {
                    continue;
                }
                let settled = if interrupted {
                    inner
                        .store
                        .interrupt_unstarted_tool_call(&claimed, id, message)
                        .await
                } else {
                    inner.store.deny_tool_call(&claimed, id, message).await
                };
                if let Err(error) = settled {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist the refused tool call", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
                if interrupted {
                    inner.remove_approval(id);
                }
            }
            RunInput::Event(Some(
                RuntimeEvent::ToolCallDenied { .. } | RuntimeEvent::ToolCallAnswered { .. },
            )) => {}
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
            RunInput::Event(Some(RuntimeEvent::ToolCallStarted {
                id,
                may_change_files,
            })) => {
                if internal {
                    continue;
                }
                if may_change_files && !snapshot_taken && !claimed.identity.child {
                    snapshot_taken = true;
                    let mut stop = cancellation.clone();
                    let cancelled = tokio::select! {
                        biased;
                        _ = stop.wait_for(|cancelled| *cancelled) => true,
                        () = tool_cancellation.cancelled() => true,
                        () = snapshot_workspace(&claimed) => false,
                    };
                    if cancelled {
                        continue;
                    }
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
                #[cfg(all(test, unix))]
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
            RunInput::Event(Some(RuntimeEvent::ToolResultsProjected {
                turn_ordinal,
                projection,
            })) => {
                if internal {
                    continue;
                }
                if let Err(error) = inner
                    .store
                    .persist_tool_projection(&claimed, turn_ordinal, projection)
                    .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist projected tool results", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
            }
            RunInput::Event(Some(RuntimeEvent::ToolCallFinished {
                id,
                result,
                is_error,
                file_states,
                display,
                spill,
                execution_context,
            })) => {
                if internal {
                    continue;
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
                match inner
                    .store
                    .finish_tool_call(
                        &claimed,
                        id,
                        result,
                        is_error,
                        file_states,
                        display,
                        spill,
                        execution_context,
                    )
                    .await
                {
                    Ok(commit) => {
                        resources.tools.forget_finished(id);
                        if commit.context_exhausted {
                            let Ok(teardown) = resources.stop(&mut events).await else {
                                inner.failed.send_replace(true);
                                return;
                            };
                            finish_run(&inner, &claimed, context_budget_failure(), teardown).await;
                            return;
                        }
                    }
                    Err(error) => {
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
                if internal || text.is_empty() {
                    continue;
                }
                if current_message.is_none() {
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
            RunInput::Event(Some(RuntimeEvent::ContextPruned { turn_ordinal })) => {
                if internal {
                    continue;
                }
                if let Err(error) = inner
                    .store
                    .advance_prune_watermark(&claimed, turn_ordinal.saturating_sub(1))
                    .await
                {
                    let Ok(teardown) = resources.stop(&mut events).await else {
                        inner.failed.send_replace(true);
                        return;
                    };
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist the prune watermark", &error),
                        teardown,
                    )
                    .await;
                    return;
                }
            }
            RunInput::Event(Some(RuntimeEvent::InRunCompacted { .. })) => {
                current_occupancy_basis = None;
            }
            RunInput::Event(Some(RuntimeEvent::ProviderOverflow { .. })) => {
                current_occupancy_basis = None;
                if internal {
                    summary_text.clear();
                    summary_continues_truncated_turn = false;
                }
            }
            RunInput::Event(Some(RuntimeEvent::SteeringApplied {
                message_id,
                turn_ordinal,
                attachments,
            })) => {
                match inner
                    .store
                    .apply_steering(&claimed, message_id, turn_ordinal, attachments)
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
            RunInput::Event(Some(RuntimeEvent::TurnRetrying {
                turn_ordinal,
                attempt,
                delay,
                kind,
                message,
            })) => {
                if internal {
                    summary_continues_truncated_turn = true;
                    continue;
                }
                match inner
                    .store
                    .record_turn_retrying(&claimed, turn_ordinal, attempt, delay, kind, message)
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
                            persistence_failure("failed to persist the turn retry", &error),
                            teardown,
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::Paused { pause })) => {
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
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Paused { pause },
                    Some(accounting.snapshot()),
                    teardown,
                )
                .await;
                return;
            }
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
                            record_budget(resolved_model.context_window),
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

async fn snapshot_workspace(claimed: &ClaimedRun) {
    let workspace = Path::new(&claimed.workspace);
    let run = claimed.identity.run_id;
    match crate::undo::snapshot(workspace, crate::undo::SnapshotKind::Run(run)).await {
        Ok(Some(snapshot)) => {
            tracing::debug!(%run, commit = %snapshot.commit, "workspace snapshot recorded");
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%run, %error, "could not snapshot the workspace; qq undo cannot restore this run");
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

pub(super) async fn finish_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    teardown: TeardownComplete,
) {
    finish_run_accounted(inner, claimed, outcome, None, teardown).await;
}

pub(super) async fn persist_finished_results(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    teardown: &mut TeardownComplete,
) -> Result<(), SessionRuntimeError> {
    for (id, result) in std::mem::take(&mut teardown.0) {
        let result = if result.spill.is_some() {
            let run_id = claimed.identity.run_id;
            let tool: String = inner
                .store
                .call(super::store::Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT name FROM tool_calls WHERE id=?1 AND run_id=?2",
                        params![id.to_string(), run_id.to_string()],
                        |row| row.get(0),
                    )?)
                })
                .await?;
            crate::cite_spill(result, &tool, id, true)
        } else {
            result
        };
        match inner
            .store
            .finish_tool_call(
                claimed,
                id,
                result.model_text,
                result.is_error,
                result.file_states,
                result.ui_payload,
                result.spill,
                result.execution_context,
            )
            .await
        {
            Ok(_) | Err(SessionRuntimeError::ToolCallNotFound) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn finish_run_accounted(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
    mut teardown: TeardownComplete,
) {
    if persist_finished_results(inner, claimed, &mut teardown)
        .await
        .is_err()
    {
        inner.failed.send_replace(true);
        return;
    }
    if matches!(&outcome, RunOutcome::BudgetExhausted { exhaustion }
        if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
        && approvals::expire_goal_hold(inner, claimed, false)
            .await
            .is_err()
    {
        inner.failed.send_replace(true);
        return;
    }
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
    pub(super) goal_request_rejected: bool,
    pub(super) usage: Option<TokenUsage>,
    pub(super) context_tokens: Option<u64>,
    pub(super) estimated_cost_usd_nanos: Option<u64>,
    pub(super) saw_turn: bool,
    pub(super) request_basis: ContextOccupancyBasis,
    pub(super) final_output: Option<Box<FinalOutput>>,
}

pub(super) struct RunAccountingAccumulator {
    goal_request_rejected: bool,
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
            goal_request_rejected: false,
            usage: Some(TokenUsage::default()),
            context_tokens: None,
            estimated_cost_usd_nanos: pricing.as_ref().map(|_| 0),
            pricing,
            saw_turn: false,
            saw_spend: false,
            request_basis,
        }
    }

    pub(super) fn with_zero_spend(mut self) -> Self {
        self.usage = Some(TokenUsage::default());
        self.estimated_cost_usd_nanos = Some(0);
        self.saw_spend = true;
        self
    }

    pub(super) fn record_unattributed(
        &mut self,
        usage: Option<TokenUsage>,
        cost_usd_nanos: Option<u64>,
    ) {
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
            self.context_tokens = None;
            self.estimated_cost_usd_nanos = None;
            return;
        };
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
            goal_request_rejected: self.goal_request_rejected,
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

const fn turn_context_tokens(usage: TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_write_input_tokens)
}

pub(crate) fn add_usage(left: TokenUsage, right: TokenUsage) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: left.input_tokens.checked_add(right.input_tokens)?,
        cache_read_input_tokens: left
            .cache_read_input_tokens
            .checked_add(right.cache_read_input_tokens)?,
        cache_write_input_tokens: left
            .cache_write_input_tokens
            .checked_add(right.cache_write_input_tokens)?,
        output_tokens: left.output_tokens.checked_add(right.output_tokens)?,
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
    pub(super) model_work: crate::runtime::ModelWorkDisposition,
    pub(super) turn_ordinal: u32,
    pub(super) message: Message,
    pub(super) calls: Vec<RuntimeToolCall>,
    pub(super) turn_message: Option<MessageId>,
    pub(super) context_tokens: Option<u64>,
    pub(super) occupancy_basis: Option<ContextOccupancyBasis>,
    pub(super) usage: Option<TokenUsage>,
    pub(super) estimated_cost_usd_nanos: Option<u64>,
    pub(super) accounting: Option<RunAccounting>,
    pub(super) truncated: bool,
    pub(super) notice: Option<crate::runtime::TurnNotice>,
    pub(super) goal_notices: Vec<crate::runtime::OrderedTurnNotice>,
    pub(super) reserved_goal_notices: usize,
}
