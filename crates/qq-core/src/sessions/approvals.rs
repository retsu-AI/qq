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
}

impl SessionToolGate {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: ClaimedRun,
        cancellation: watch::Receiver<bool>,
        network: Arc<crate::tools::network::NetworkPolicy>,
    ) -> Self {
        Self {
            inner,
            claimed,
            cancellation,
            network,
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
                    // The reviewer adjudicates under Auto (the held bucket is
                    // "dangerous-shaped but possibly fine") and under
                    // Supervised (every action of a write child). Ask means
                    // the human asked to decide everything; ReadOnly never
                    // reaches here. Under Auto only a clear Approve changes
                    // anything; under Supervised a Deny is final too.
                    let mut review: Option<ReviewFuture> = match (&inner.approval_reviewer, mode) {
                        (Some(reviewer), ApprovalMode::Auto | ApprovalMode::Supervised) => {
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
                    // One deadline for the whole wait: a reviewer escalation
                    // must not restart the human approval timeout.
                    let deadline = tokio::time::Instant::now() + inner.approval_timeout;
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
                                                .resolve_approval_by_reviewer(&claimed, call.id)
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
                                        ReviewDecision::Deny { reason }
                                            if mode == ApprovalMode::Supervised =>
                                        {
                                            let message = format!(
                                                "{} {}",
                                                approval::REVIEWER_DENIED_RESULT,
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
                                        // Escalate, or Deny under Auto: keep
                                        // waiting for the human on the
                                        // remaining select arms.
                                        ReviewDecision::Escalate { .. }
                                        | ReviewDecision::Deny { .. } => {}
                                    }
                                }
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
