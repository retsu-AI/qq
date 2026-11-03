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
                    let deadline = human_deadline(inner.approval_timeout);
                    let timed_out = tokio::select! {
                        biased;
                        changed = cancellation.changed() => {
                            let _ = changed;
                            return cancelled_hold(&inner,call.id,
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
                    let deadline = human_deadline(inner.approval_timeout);
                    let timed_out = tokio::select! {
                        biased;
                        changed = cancellation.changed() => {
                            let _ = changed;
                            return cancelled_hold(&inner,call.id,
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
    call: ToolCallId,
    message: &str,
) -> GateDecision {
    inner.remove_approval(call);
    GateDecision::Deny {
        message: message.to_owned(),
    }
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

async fn conclude(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    call_id: ToolCallId,
    timed_out: bool,
) -> GateDecision {
    match inner
        .store
        .conclude_tool_approval(claimed, call_id, timed_out)
        .await
    {
        Ok(ConcludedApproval::Approved) => GateDecision::Execute,
        Ok(ConcludedApproval::Denied { message }) => GateDecision::Deny { message },
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
    Denied { message: String },
    Answered { result: String },
    StillWaiting,
}
