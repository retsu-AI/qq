use super::runtime::SessionRuntimeInner;
use super::*;

pub(super) struct SessionToolGate {
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    cancellation: watch::Receiver<bool>,
    network: Arc<crate::tools::network::NetworkPolicy>,
    shell: Arc<crate::ShellPolicy>,
}

impl SessionToolGate {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: ClaimedRun,
        cancellation: watch::Receiver<bool>,
        network: Arc<crate::tools::network::NetworkPolicy>,
        shell: Arc<crate::ShellPolicy>,
    ) -> Self {
        Self {
            inner,
            claimed,
            cancellation,
            network,
            shell,
        }
    }
}

impl ToolGate for SessionToolGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let inner = Arc::clone(&self.inner);
        let claimed = self.claimed.clone();
        let call = call.clone();
        let mut cancellation = self.cancellation.clone();
        let network = Arc::clone(&self.network);
        let shell = Arc::clone(&self.shell);
        Box::pin(async move {
            let policy = match inner
                .store
                .tool_approval_policy(claimed.identity.session_id)
                .await
            {
                Ok(policy) => policy,
                Err(error) => return approval_persistence_failure(error),
            };
            let ToolApprovalPolicy { mode, ancestors } = policy;
            let class = approval::classify(call.effect, &call.name, &call.arguments, &network);
            if let approval::ToolClass::Shell { command, .. } = &class
                && approval::shell_prefix_denied(command, &shell.deny_prefixes)
            {
                let message = approval::SHELL_PREFIX_DENIED_RESULT.to_owned();
                return match inner
                    .store
                    .deny_tool_call(&claimed, call.id, message.clone())
                    .await
                {
                    Ok(_) => GateDecision::Deny { message },
                    Err(error) => approval_persistence_failure(error),
                };
            }
            let auto_ceiling =
                mode == ApprovalMode::Auto || ancestors.contains(&ApprovalMode::Auto);
            if auto_ceiling
                && matches!(
                    class,
                    approval::ToolClass::Interactive { question: Some(_) }
                )
            {
                let message = approval::UNATTENDED_QUESTION_RESULT.to_owned();
                return match inner
                    .store
                    .deny_tool_call(&claimed, call.id, message.clone())
                    .await
                {
                    Ok(_) => GateDecision::Deny { message },
                    Err(error) => approval_persistence_failure(error),
                };
            }
            let mut mode = mode;
            for parent_mode in ancestors {
                let message = match approval::evaluate(parent_mode, &class) {
                    approval::PolicyDecision::Execute
                    | approval::PolicyDecision::AskUser { .. } => continue,
                    approval::PolicyDecision::RequireApproval
                        if parent_mode == ApprovalMode::Ask && !auto_ceiling =>
                    {
                        if mode != ApprovalMode::ReadOnly {
                            mode = ApprovalMode::Ask;
                        }
                        continue;
                    }
                    approval::PolicyDecision::RequireApproval => {
                        approval::PARENT_DENIED_RESULT.to_owned()
                    }
                    approval::PolicyDecision::Deny { reason } => approval::deny_result(&reason),
                };
                return match inner
                    .store
                    .deny_tool_call(&claimed, call.id, message.clone())
                    .await
                {
                    Ok(_) => GateDecision::Deny { message },
                    Err(error) => approval_persistence_failure(error),
                };
            }
            let decision = approval::evaluate(mode, &class);
            let decision =
                if auto_ceiling && matches!(decision, approval::PolicyDecision::RequireApproval) {
                    approval::PolicyDecision::Deny {
                        reason: approval::DenyReason::OutOfScope,
                    }
                } else {
                    decision
                };
            match decision {
                approval::PolicyDecision::Execute => GateDecision::Execute,
                approval::PolicyDecision::Deny { reason } => {
                    let message = approval::deny_result(&reason);
                    match inner
                        .store
                        .deny_tool_call(&claimed, call.id, message.clone())
                        .await
                    {
                        Ok(_) => GateDecision::Deny { message },
                        Err(error) => approval_persistence_failure(error),
                    }
                }
                approval::PolicyDecision::AskUser { question } => {
                    let mut resolved = inner.register_approval(call.id, claimed.identity.run_id);
                    match inner
                        .store
                        .request_tool_approval(
                            &claimed,
                            call.id,
                            ApprovalPreviews {
                                question: Some(question),
                                ..ApprovalPreviews::default()
                            },
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(error) => {
                            inner.remove_approval(call.id);
                            return approval_persistence_failure(error);
                        }
                    }
                    let goal_deadline = match goal_approval_deadline(&inner, &claimed).await {
                        Ok(deadline) => deadline,
                        Err(error) => {
                            inner.remove_approval(call.id);
                            return approval_persistence_failure(error);
                        }
                    };
                    let deadline = human_deadline(inner.approval_timeout)
                        .into_iter()
                        .chain(goal_deadline)
                        .min();
                    let timed_out = tokio::select! {
                        biased;
                        changed = cancellation.changed() => {
                            let _ = changed;
                            return cancelled_hold(&inner, &claimed, call.id,
                                "The run stopped before this question was answered.").await;
                        }
                        result = &mut resolved => result.is_err(),
                        () = sleep_until_or_never(deadline) => true,
                    };
                    inner.remove_approval(call.id);
                    conclude(&inner, &claimed, call.id, timed_out).await
                }
                approval::PolicyDecision::RequireApproval => {
                    #[cfg(not(feature = "tool-fetch"))]
                    let fetch = None;
                    #[cfg(feature = "tool-fetch")]
                    let fetch = match &class {
                        approval::ToolClass::Network {
                            host: Some(host), ..
                        } => crate::tools::fetch::preview(&call.arguments, host),
                        _ => None,
                    };
                    let shell = match class {
                        approval::ToolClass::Shell { command, cwd, .. } => {
                            Some(ShellCommandPreview {
                                command,
                                cwd,
                                verdict: None,
                                reasons: Vec::new(),
                            })
                        }
                        _ => None,
                    };
                    let edit = approval::edit_preview(&call.name, &call.arguments);
                    let mut resolved = inner.register_approval(call.id, claimed.identity.run_id);
                    match inner
                        .store
                        .request_tool_approval(
                            &claimed,
                            call.id,
                            ApprovalPreviews {
                                shell,
                                edit,
                                question: None,
                                fetch,
                            },
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(error) => {
                            inner.remove_approval(call.id);
                            return approval_persistence_failure(error);
                        }
                    }
                    let goal_deadline = match goal_approval_deadline(&inner, &claimed).await {
                        Ok(deadline) => deadline,
                        Err(error) => {
                            inner.remove_approval(call.id);
                            return approval_persistence_failure(error);
                        }
                    };
                    let deadline = human_deadline(inner.approval_timeout)
                        .into_iter()
                        .chain(goal_deadline)
                        .min();
                    let timed_out = tokio::select! {
                        biased;
                        changed = cancellation.changed() => {
                            let _ = changed;
                            return cancelled_hold(&inner, &claimed, call.id,
                                "The run stopped before this approval was resolved.").await;
                        }
                        result = &mut resolved => result.is_err(),
                        () = sleep_until_or_never(deadline) => true,
                    };
                    inner.remove_approval(call.id);
                    conclude(&inner, &claimed, call.id, timed_out).await
                }
            }
        })
    }
}

async fn cancelled_hold(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    call: ToolCallId,
    message: &str,
) -> GateDecision {
    let settled = expire_goal_hold(inner, claimed, true).await;
    inner.remove_approval(call);
    match settled {
        Ok(()) => GateDecision::Deny {
            message: message.to_owned(),
        },
        Err(error) => approval_persistence_failure(error),
    }
}

async fn goal_approval_deadline(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
) -> Result<Option<tokio::time::Instant>, SessionRuntimeError> {
    if claimed.goal.is_none() && claimed.goal_spend_owner.is_none() {
        return Ok(None);
    }
    let identity = claimed.identity;
    let at = tokio::time::Instant::now();
    let timeout = inner
        .store
        .call(store::Priority::Output, move |connection| {
            goal_policy::approval_timeout(connection, identity)
        })
        .await?;
    Ok(timeout.map(|timeout| at + timeout))
}

fn human_deadline(timeout: Option<Duration>) -> Option<tokio::time::Instant> {
    timeout.map(|timeout| tokio::time::Instant::now() + timeout)
}

async fn sleep_until_or_never(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn expired_goal_owners(inner: &SessionRuntimeInner) -> Result<Vec<RunId>, SessionRuntimeError> {
    Ok(inner
        .goal_deadlines
        .lock()
        .map_err(|_| SessionRuntimeError::Unavailable)?
        .iter()
        .filter_map(|(run, deadline)| deadline.expired().then_some(*run))
        .collect())
}

async fn conclude(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    call_id: ToolCallId,
    timed_out: bool,
) -> GateDecision {
    let expired_owners = if timed_out {
        match expired_goal_owners(inner) {
            Ok(owners) => owners,
            Err(error) => return approval_persistence_failure(error),
        }
    } else {
        Vec::new()
    };
    match inner
        .store
        .conclude_tool_approval(claimed, call_id, timed_out, expired_owners)
        .await
    {
        Ok(ConcludedApproval::Approved) => GateDecision::Execute,
        Ok(ConcludedApproval::Denied { message }) => GateDecision::Deny { message },
        Ok(ConcludedApproval::GoalStopped { run_ids }) => {
            for run in run_ids {
                inner.cancel(run);
            }
            GateDecision::Deny {
                message: approval::TIMEOUT_DENIED_RESULT.to_owned(),
            }
        }
        Ok(ConcludedApproval::Answered { result }) => GateDecision::Answered { result },
        Ok(ConcludedApproval::StillWaiting) => GateDecision::Fail {
            kind: RunFailureKind::Server,
            message: "tool approval resolution disappeared before it could be applied".to_owned(),
        },
        Err(error) => approval_persistence_failure(error),
    }
}

fn approval_persistence_failure(error: SessionRuntimeError) -> GateDecision {
    match error {
        SessionRuntimeError::OutputTooLarge => GateDecision::Fail {
            kind: RunFailureKind::Policy,
            message: "the tool result would exceed the run's context capacity".to_owned(),
        },
        error => GateDecision::Fail {
            kind: RunFailureKind::Server,
            message: format!("tool approval state could not be persisted: {error}"),
        },
    }
}

pub(super) enum ConcludedApproval {
    Approved,
    GoalStopped { run_ids: Vec<RunId> },
    Denied { message: String },
    Answered { result: String },
    StillWaiting,
}

pub(super) async fn expire_goal_hold(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    expired_only: bool,
) -> Result<(), SessionRuntimeError> {
    if claimed.goal.is_none() && claimed.goal_spend_owner.is_none() {
        return Ok(());
    }
    let expired_owners = expired_goal_owners(inner)?;
    let identity = claimed.identity;
    let store_id = inner.store.store_id();
    let concluded = inner.store.call(store::Priority::Output, move |connection| {
        if expired_only && !goal_policy::expired_approval_owner(connection, identity, &expired_owners)? {
            return Ok(None);
        }
        let call: Option<String> = connection.query_row(
            "SELECT id FROM tool_calls WHERE run_id=?1 AND state='awaiting_approval' ORDER BY turn_ordinal,call_ordinal LIMIT 1",
            [identity.run_id.to_string()], |row| row.get(0)).optional()?;
        match call {
            Some(call) => tool_calls::conclude_tool_approval(connection, store_id, identity, parse_id(&call)?, true, &expired_owners).map(Some),
            None => Ok(None),
        }
    }).await?;
    if let Some(ConcludedApproval::GoalStopped { run_ids }) = concluded {
        for run in run_ids {
            inner.cancel(run);
        }
    }
    Ok(())
}
