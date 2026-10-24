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
    shell: Arc<crate::ShellPolicy>,
    delegate: approval::ApprovalDelegate,
    task_scoped: bool,
}

impl SessionToolGate {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: ClaimedRun,
        cancellation: watch::Receiver<bool>,
        network: Arc<crate::tools::network::NetworkPolicy>,
        shell: Arc<crate::ShellPolicy>,
        delegate: approval::ApprovalDelegate,
        task_scoped: bool,
    ) -> Self {
        Self {
            inner,
            claimed,
            cancellation,
            network,
            shell,
            delegate,
            task_scoped,
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
        let delegate = self.delegate;
        let task_scoped = self.task_scoped;
        Box::pin(async move {
            let policy = match inner
                .store
                .tool_approval_policy(claimed.identity.session_id)
                .await
            {
                Ok(policy) => policy,
                Err(error) => return approval_persistence_failure(error),
            };
            let ToolApprovalPolicy {
                mode,
                grants,
                delegate: session_delegate,
                ancestors,
                supervised,
            } = policy.clone();
            // The session's own choice (`set_approval_delegate`) wins over the
            // configured one for the rest of the session; read here, at the
            // hold, so it applies to the next held call without a restart.
            let delegate = session_delegate.unwrap_or(delegate);
            let class = approval::classify(call.effect, &call.name, &call.arguments, &network);
            if let approval::ToolClass::Shell { command, .. } = &class
                && approval::managed_shell_denied(command, &shell.deny_prefixes)
            {
                let message = "permission_denied: managed_shell_deny: this command matches an explicitly configured deny_shell_prefixes entry; continue with an allowed operation".to_owned();
                return match inner
                    .store
                    .deny_tool_call(&claimed, call.id, message.clone())
                    .await
                {
                    Ok(_) => GateDecision::Deny { message },
                    Err(error) => approval_persistence_failure(error),
                };
            }
            // Auto is an unattended ceiling for the entire descendant tree,
            // even if a child session later selects a different mode.
            let auto_ceiling = mode == ApprovalMode::Auto
                || ancestors
                    .iter()
                    .any(|(mode, _)| *mode == ApprovalMode::Auto);
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
            // Existing explicit internal supervision remains available outside
            // Auto. Authorized Auto child actions use their parent's policy,
            // without introducing another per-action reviewer.
            let internal_supervision = supervised && !auto_ceiling && mode == ApprovalMode::Full;
            let no_human_approval = auto_ceiling || internal_supervision;
            let mut mode = mode;
            for (parent_mode, parent_grants) in ancestors {
                let message = match approval::evaluate_in_task(parent_mode, &call.name, &class, &parent_grants, task_scoped) {
                    approval::PolicyDecision::Execute | approval::PolicyDecision::AskUser { .. } => continue,
                    approval::PolicyDecision::RequireApproval if parent_mode == ApprovalMode::Ask && !no_human_approval => {
                        // Ask applies to each child action too. Auto children
                        // deny instead of inheriting a human approval wait.
                        if mode != ApprovalMode::ReadOnly {
                            mode = ApprovalMode::Ask;
                        }
                        continue;
                    }
                    approval::PolicyDecision::RequireApproval => "permission_denied: parent_permission_required: this child action is outside its parent's grants; use an operation within the parent's authority".to_owned(),
                    approval::PolicyDecision::Deny { reason } => approval::deny_result(&reason),
                    approval::PolicyDecision::Forbidden { rules } => approval::forbidden_result(&rules),
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
            let decision =
                approval::evaluate_in_task(mode, &call.name, &class, &grants, task_scoped);
            // A reviewer may supervise existing authority, never create it.
            let decision = if no_human_approval
                && matches!(decision, approval::PolicyDecision::RequireApproval)
            {
                approval::PolicyDecision::Deny {
                    reason: approval::DenyReason::OutOfScope,
                }
            } else {
                decision
            };
            // Parent ceilings were checked before any internal supervision.
            let decision = if internal_supervision
                && matches!(decision, approval::PolicyDecision::Execute)
                && !matches!(
                    class,
                    approval::ToolClass::ReadOnly
                        | approval::ToolClass::Interactive { .. }
                        | approval::ToolClass::Network { host: None, .. }
                ) {
                approval::PolicyDecision::RequireApproval
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
                    conclude(&inner, &claimed, call.id, timed_out, None).await
                }
                approval::PolicyDecision::RequireApproval => {
                    let consult = delegate.consults_reviewer(if no_human_approval {
                        ApprovalMode::Auto
                    } else {
                        mode
                    }) && inner.approval_reviewer.is_some();
                    if no_human_approval && !consult {
                        let message = "permission_denied: internal supervision is unavailable; use an action that does not require supervision.".to_owned();
                        return match inner
                            .store
                            .deny_tool_call(&claimed, call.id, message.clone())
                            .await
                        {
                            Ok(_) => GateDecision::Deny { message },
                            Err(error) => approval_persistence_failure(error),
                        };
                    }
                    #[cfg(not(feature = "tool-fetch"))]
                    let fetch = None;
                    #[cfg(feature = "tool-fetch")]
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
                        approval::ToolClass::Shell { command, cwd, .. } => {
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
                    // Decided before publishing so the request says who is
                    // asked first; see the comment on the review below.
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
                                delegated: consult,
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
                    // Only existing internal supervision reaches a reviewer.
                    // Missing authority was denied above; a reviewer cannot grant it.
                    // Ask requires a human and never consults the reviewer.
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
                    // Internal supervision has a bounded deadline. Auto denies on
                    // timeout; it never starts a human wait. Ask uses the configured
                    // human deadline, also bounded by the goal's fixed deadline.
                    let delegate_deadline = tokio::time::Instant::now() + inner.delegate_timeout;
                    let mut deadline = if review.is_some() {
                        goal_deadline
                    } else {
                        human_deadline(inner.approval_timeout)
                            .into_iter()
                            .chain(goal_deadline)
                            .min()
                    };
                    let timed_out = loop {
                        if let Some(pending_review) = review.as_mut() {
                            tokio::select! {
                                biased;
                                changed = cancellation.changed() => {
                                    let _ = changed;
                                    return cancelled_hold(&inner, &claimed, call.id,
                                        "The run stopped before this approval was resolved.").await;
                                }
                                result = &mut resolved => break result.is_err(),
                                () = sleep_until_or_never(goal_deadline) => break true,
                                verdict = pending_review => {
                                    review = None;
                                    review_spend = Some(verdict.spend);
                                    let decision = match verdict.decision {
                                        ReviewDecision::Escalate { reason } if no_human_approval => ReviewDecision::Deny { reason },
                                        decision => decision,
                                    };
                                    match decision {
                                        ReviewDecision::Approve => {
                                            match inner
                                                .store
                                                .resolve_approval_by_reviewer(
                                                    &claimed,
                                                    call.id,
                                                    delegate_grant.take(),
                                                    policy.clone(),
                                                )
                                                .await
                                            {
                                                Ok(Some(_)) => {
                                                    inner.remove_approval(call.id);
                                                    return conclude(&inner, &claimed, call.id, false, review_spend).await;
                                                }
                                                // A client resolution won the
                                                // race or the write failed:
                                                // fall through to conclude,
                                                // which reads the durable state.
                                                Ok(None) | Err(_) => break false,
                                            }
                                        }
                                        // A reviewer denial is final. Ask never
                                        // reaches the reviewer.
                                        ReviewDecision::Deny { reason } =>
                                        {
                                            let message = format!(
                                                "{} {}",
                                                approval::reviewer_denied_result(),
                                                truncate_utf8(reason, MAX_REVIEW_REASON_BYTES)
                                            );
                                            match inner
                                                .store
                                                .deny_approval_by_reviewer(
                                                    &claimed,
                                                    call.id,
                                                    message.clone(),
                                                    DelegateIdentity::Reviewer,
                                                )
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
                                        // remaining select arms. The
                                        // escalation is published so the
                                        // prompt can say why the delegate did
                                        // not decide; a failed write changes
                                        // nothing about the hold.
                                        ReviewDecision::Escalate { reason } => {
                                            if let Err(error) = inner
                                                .store
                                                .escalate_tool_approval(
                                                    &claimed,
                                                    call.id,
                                                    Some(DelegateIdentity::Reviewer),
                                                    truncate_utf8(reason, MAX_REVIEW_REASON_BYTES),
                                                )
                                                .await
                                            {
                                                inner.remove_approval(call.id);
                                                return reviewed(
                                                    approval_persistence_failure(error),
                                                    review_spend,
                                                );
                                            }
                                            deadline = human_deadline(inner.approval_timeout).into_iter().chain(goal_deadline).min();
                                        }
                                    }
                                }
                                // Auto settles unavailable supervision as a denial.
                                // Other approval paths retain their human deadline.
                                () = tokio::time::sleep_until(delegate_deadline) => {
                                    review = None;
                                    if no_human_approval {
                                        let message = format!("{} {}", approval::reviewer_denied_result(), approval::DELEGATE_TIMED_OUT_REASON);
                                        match inner.store.deny_approval_by_reviewer(
                                            &claimed, call.id, message.clone(), DelegateIdentity::Reviewer,
                                        ).await {
                                            Ok(Some(_)) => {
                                                inner.remove_approval(call.id);
                                                return reviewed(GateDecision::Deny { message }, review_spend);
                                            }
                                            Ok(None) | Err(_) => break false,
                                        }
                                    }
                                    if let Err(error) = inner
                                        .store
                                        .escalate_tool_approval(
                                            &claimed,
                                            call.id,
                                            None,
                                            approval::DELEGATE_TIMED_OUT_REASON.to_owned(),
                                        )
                                        .await
                                    {
                                        inner.remove_approval(call.id);
                                        return approval_persistence_failure(error);
                                    }
                                    deadline = human_deadline(inner.approval_timeout).into_iter().chain(goal_deadline).min();
                                }
                            }
                        } else {
                            tokio::select! {
                                biased;
                                changed = cancellation.changed() => {
                                    // The gate can observe cancellation before the
                                    // session loop; retain an expired goal hold first.
                                    let _ = changed;
                                    return cancelled_hold(&inner, &claimed, call.id,
                                        "The run stopped before this approval was resolved.").await;
                                }
                                result = &mut resolved => break result.is_err(),
                                () = sleep_until_or_never(deadline) => break true,
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

// The gate and the session loop race on the same cancellation signal. Settle
// an expired goal's unanswered hold before yielding a denied/interrupted call:
// the outer loop can no longer find the awaiting row once it persists that call.
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

/// When the human's wait ends, counted from now. `None` is no server
/// deadline: the hold waits for the client, the run deadline, or cancellation.
fn human_deadline(timeout: Option<Duration>) -> Option<tokio::time::Instant> {
    timeout.map(|timeout| tokio::time::Instant::now() + timeout)
}

/// Resolves at `deadline`, or never when there is none. Used as a select arm
/// so an unbounded wait is expressed by the arm simply not firing.
async fn sleep_until_or_never(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
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
        Ok(ConcludedApproval::GoalStopped { run_ids }) => {
            for run in run_ids {
                inner.cancel(run);
            }
            reviewed(
                GateDecision::Deny {
                    message: approval::TIMEOUT_DENIED_RESULT.to_owned(),
                },
                review_spend,
            )
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
    GoalStopped {
        run_ids: Vec<RunId>,
    },
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

/// The run-duration clock may win the select against a goal hold's deadline.
/// Settle that still-unanswered hold before terminal goal exhaustion is applied.
pub(super) async fn expire_goal_hold(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    expired_only: bool,
) -> Result<(), SessionRuntimeError> {
    if claimed.goal.is_none() && claimed.goal_spend_owner.is_none() {
        return Ok(());
    }
    // Capture on the executor: Tokio's simulated clock is runtime-local.
    let expired_owners = inner
        .goal_deadlines
        .lock()
        .map_err(|_| SessionRuntimeError::Unavailable)?
        .iter()
        .filter_map(|(run, deadline)| deadline.expired().then_some(*run))
        .collect::<Vec<_>>();
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
