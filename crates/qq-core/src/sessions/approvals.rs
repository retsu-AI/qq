use super::runtime::SessionRuntimeInner;
use super::*;

/// Applies the session's approval policy to each requested tool call,
/// persisting approval state before publishing it and holding the run open
/// while a client decides.
pub(super) struct SessionToolGate {
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    cancellation: watch::Receiver<bool>,
    network: Arc<crate::tools::network::NetworkPolicy>,
    delegate: approval::ApprovalDelegate,
}

impl SessionToolGate {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: ClaimedRun,
        cancellation: watch::Receiver<bool>,
        network: Arc<crate::tools::network::NetworkPolicy>,
        delegate: approval::ApprovalDelegate,
    ) -> Self {
        Self {
            inner,
            claimed,
            cancellation,
            network,
            delegate,
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
        let delegate = self.delegate;
        Box::pin(async move {
            let (mode, grants) = match inner
                .store
                .approval_policy(claimed.identity.session_id)
                .await
            {
                Ok(policy) => policy,
                Err(error) => return approval_persistence_failure(error),
            };
            let class = approval::classify(call.effect, &call.name, &call.arguments, &network);
            match approval::evaluate(mode, &call.name, &class, &grants) {
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
                approval::PolicyDecision::Forbidden { rules } => {
                    let message = approval::forbidden_result(&rules);
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
                    // A question is a hold without a permission: same
                    // registration, persistence, and wait as an approval;
                    // no reviewer (there is nothing to adjudicate).
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
                    let deadline = tokio::time::Instant::now() + inner.approval_timeout;
                    let timed_out = tokio::select! {
                        biased;
                        changed = cancellation.changed() => {
                            let _ = changed;
                            inner.remove_approval(call.id);
                            return GateDecision::Deny {
                                message: "The run stopped before this question was answered."
                                    .to_owned(),
                            };
                        }
                        result = &mut resolved => result.is_err(),
                        () = tokio::time::sleep_until(deadline) => true,
                    };
                    inner.remove_approval(call.id);
                    conclude(&inner, &claimed, call.id, timed_out, None).await
                }
                approval::PolicyDecision::RequireApproval => {
                    let fetch = match &class {
                        approval::ToolClass::Network {
                            host: Some(host), ..
                        } => crate::tools::fetch::preview(&call.arguments, host),
                        _ => None,
                    };
                    // What a delegate's Approve may bless for the rest of the
                    // session: the exact command string or the exact host,
                    // nothing wider, and nothing for other tool classes. The
                    // human's own choices (prefix, tool name, workspace) stay
                    // on the client path; a delegate never gets them.
                    let mut delegate_grant = match &class {
                        approval::ToolClass::Shell { command, .. } => {
                            Some(DelegateGrant::Command(command.clone()))
                        }
                        approval::ToolClass::Network {
                            host: Some(host), ..
                        } => Some(DelegateGrant::Host(host.clone())),
                        _ => None,
                    };
                    let shell = match class {
                        approval::ToolClass::Shell { command, cwd } => {
                            // Why the gate is asking, so the client can say so.
                            let verdict = approval::classify_command(&command, None);
                            Some(ShellCommandPreview {
                                command,
                                cwd,
                                verdict: Some(match verdict.decision {
                                    approval::Decision::Allow => ShellVerdict::Allow,
                                    approval::Decision::Prompt => ShellVerdict::Prompt,
                                    approval::Decision::Forbidden => ShellVerdict::Forbidden,
                                }),
                                reasons: verdict
                                    .reasons
                                    .iter()
                                    .map(|rule| rule.name().to_owned())
                                    .collect(),
                            })
                        }
                        _ => None,
                    };
                    let edit = approval::edit_preview(&call.name, &call.arguments);
                    // Register before publishing the request so a client
                    // response can never race past the waiting run.
                    let mut resolved = inner.register_approval(call.id, claimed.identity.run_id);
                    match inner
                        .store
                        .request_tool_approval(
                            &claimed,
                            call.id,
                            ApprovalPreviews {
                                shell: shell.clone(),
                                edit: edit.clone(),
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
                    // Who the held call goes to first. The mode is the
                    // ceiling; the delegate setting chooses who decides inside
                    // it: by default the reviewer under Auto (the held bucket
                    // is "dangerous-shaped but possibly fine") and Supervised
                    // (every action of a write child), the human under Ask.
                    // `On` extends the reviewer to Ask; `Off` withdraws it.
                    // ReadOnly and Full never reach here. Where the reviewer
                    // is the delegate its verdict settles the call: Approve
                    // executes, Deny is final under Auto and Supervised, and
                    // Escalate (or a reviewer failure, which the reviewer must
                    // report as Escalate) reaches the human.
                    let consult = delegate.consults_reviewer(mode);
                    let mut review: Option<ReviewFuture> = match &inner.approval_reviewer {
                        Some(reviewer) if consult => {
                            // Context is advisory: a missing brief must not
                            // skip the review.
                            let (task_brief, recent_actions) = inner
                                .store
                                .review_context(&claimed)
                                .await
                                .unwrap_or_default();
                            Some(reviewer.review(ReviewRequest {
                                tool_name: call.name.clone(),
                                arguments: truncate_utf8(
                                    call.arguments.clone(),
                                    MAX_REVIEW_ARGUMENT_BYTES,
                                ),
                                shell,
                                edit,
                                workspace: claimed.workspace.clone(),
                                origin: if claimed.identity.child {
                                    ReviewOrigin::Child {
                                        depth: claimed.depth,
                                        parent_run: claimed.identity.run_id,
                                    }
                                } else {
                                    ReviewOrigin::Root
                                },
                                task_brief,
                                mode,
                                recent_actions,
                                granted_tools: grants.tools.iter().cloned().collect(),
                                granted_shell_prefixes: grants.shell_prefixes.clone(),
                            }))
                        }
                        _ => None,
                    };
                    // `Some` once a reviewer verdict arrived; its spend is
                    // charged to this run whatever the outcome.
                    let mut review_spend: Option<ReviewSpend> = None;
                    // The human's deadline. While a reviewer is deciding the
                    // human is not being asked, so the wait starts when the
                    // reviewer escalates, not when it was consulted: a slow
                    // reviewer must not eat into the human's time. Without a
                    // reviewer the deadline starts now.
                    let mut deadline = tokio::time::Instant::now() + inner.approval_timeout;
                    let timed_out = loop {
                        if let Some(pending_review) = review.as_mut() {
                            tokio::select! {
                                biased;
                                changed = cancellation.changed() => {
                                    let _ = changed;
                                    inner.remove_approval(call.id);
                                    return GateDecision::Deny {
                                        message: "The run stopped before this approval was resolved."
                                            .to_owned(),
                                    };
                                }
                                result = &mut resolved => break result.is_err(),
                                verdict = pending_review => {
                                    review = None;
                                    review_spend = Some(verdict.spend);
                                    match verdict.decision {
                                        ReviewDecision::Approve => {
                                            match inner
                                                .store
                                                .resolve_approval_by_reviewer(
                                                    &claimed,
                                                    call.id,
                                                    delegate_grant.take(),
                                                )
                                                .await
                                            {
                                                Ok(Some(_)) => {
                                                    inner.remove_approval(call.id);
                                                    return reviewed(GateDecision::Execute, review_spend);
                                                }
                                                // A client resolution won the
                                                // race or the write failed:
                                                // fall through to conclude,
                                                // which reads the durable state.
                                                Ok(None) | Err(_) => break false,
                                            }
                                        }
                                        // Final under Auto and Supervised: the
                                        // reviewer is the configured delegate
                                        // for the calls those modes hold. Under
                                        // Ask the operator asked to decide
                                        // everything, so a denial is advice: the
                                        // human is still asked and their wait
                                        // starts here. The client-wins race is
                                        // unchanged.
                                        ReviewDecision::Deny { reason }
                                            if approval::ApprovalDelegate::deny_is_final(mode) =>
                                        {
                                            let message = format!(
                                                "{} {}",
                                                approval::reviewer_denied_result(mode),
                                                truncate_utf8(reason, MAX_REVIEW_REASON_BYTES)
                                            );
                                            match inner
                                                .store
                                                .deny_approval_by_reviewer(&claimed, call.id, message.clone())
                                                .await
                                            {
                                                Ok(Some(_)) => {
                                                    inner.remove_approval(call.id);
                                                    return reviewed(
                                                        GateDecision::Deny { message },
                                                        review_spend,
                                                    );
                                                }
                                                Ok(None) | Err(_) => break false,
                                            }
                                        }
                                        // Escalate, or a non-final Deny: the
                                        // human's wait starts here, on the
                                        // remaining select arms.
                                        ReviewDecision::Escalate { .. } | ReviewDecision::Deny { .. } => {
                                            deadline = tokio::time::Instant::now()
                                                + inner.approval_timeout;
                                        }
                                    }
                                }
                                // Bounds the reviewer as well as the human: a
                                // reviewer that never answers cannot hold the
                                // run past the configured wait.
                                () = tokio::time::sleep_until(deadline) => break true,
                            }
                        } else {
                            tokio::select! {
                                biased;
                                changed = cancellation.changed() => {
                                    // Run cancellation or shutdown: leave the call
                                    // awaiting so run completion interrupts it.
                                    let _ = changed;
                                    inner.remove_approval(call.id);
                                    return GateDecision::Deny {
                                        message: "The run stopped before this approval was resolved."
                                            .to_owned(),
                                    };
                                }
                                result = &mut resolved => break result.is_err(),
                                () = tokio::time::sleep_until(deadline) => break true,
                            }
                        }
                    };
                    inner.remove_approval(call.id);
                    conclude(&inner, &claimed, call.id, timed_out, review_spend).await
                }
            }
        })
    }
}

/// Reads the durable outcome of a hold once the wait ended and turns it into
/// the gate's decision; a timeout is written here if nothing else resolved.
async fn conclude(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    call_id: ToolCallId,
    timed_out: bool,
    review_spend: Option<ReviewSpend>,
) -> GateDecision {
    match inner
        .store
        .conclude_tool_approval(claimed, call_id, timed_out)
        .await
    {
        Ok(ConcludedApproval::Approved) => reviewed(GateDecision::Execute, review_spend),
        Ok(ConcludedApproval::Denied { message }) => {
            reviewed(GateDecision::Deny { message }, review_spend)
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
    Denied {
        message: String,
    },
    /// The user answered an `ask_user` hold; `result` is the settled call's
    /// persisted result text.
    Answered {
        result: String,
    },
    StillWaiting,
}

/// Longest reviewer rationale carried into a denial result.
const MAX_REVIEW_REASON_BYTES: usize = 512;

/// Wraps a decision with the reviewer's spend when a reviewer answered, so
/// the run loop charges it; no reviewer, no wrapper.
fn reviewed(decision: GateDecision, spend: Option<ReviewSpend>) -> GateDecision {
    match spend {
        Some(spend) => GateDecision::Reviewed {
            decision: Box::new(decision),
            spend,
        },
        None => decision,
    }
}
