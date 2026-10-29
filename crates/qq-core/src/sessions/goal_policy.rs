use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPolicy {
    pub max_duration_ms: u64,
    pub max_goal_runs: u32,
    pub per_run: RunLimits,
    pub max_backoff_ms: u64,
    pub max_check_timeout_ms: u64,
    pub approval_wait_ms: u64,
}

impl Default for GoalPolicy {
    fn default() -> Self {
        Self {
            max_duration_ms: 604_800_000,
            max_goal_runs: 2000,
            per_run: RunLimits::default(),
            max_backoff_ms: 1_800_000,
            max_check_timeout_ms: 3_600_000,
            approval_wait_ms: 600_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GoalPolicyError {
    #[error("goal policy could not be loaded: {0}")]
    Configuration(#[source] RuntimeLoadError),
    #[error("goal policy resolution stopped unexpectedly")]
    Unavailable,
    #[error("too many workspace policy resolutions are active; retry the request")]
    Overloaded,
    #[error("goal policy contains unsupported bounds")]
    Invalid,
}

pub type GoalPolicyFuture =
    Pin<Box<dyn Future<Output = Result<GoalPolicy, GoalPolicyError>> + Send + 'static>>;

impl GoalPolicy {
    pub(super) fn validate(self) -> Result<Self, GoalPolicyError> {
        if self.max_duration_ms == 0
            || self.max_duration_ms > 604_800_000
            || self.max_goal_runs == 0
            || self.max_goal_runs > 2000
            || self.max_backoff_ms == 0
            || self.max_backoff_ms > 86_400_000
            || self.max_check_timeout_ms == 0
            || self.max_check_timeout_ms > 3_600_000
            || self.approval_wait_ms == 0
            || self.approval_wait_ms > 86_400_000
            || claim::validate_run_limits(&self.per_run).is_err()
        {
            return Err(GoalPolicyError::Invalid);
        }
        Ok(self)
    }

    pub(super) fn validate_specification(
        self,
        budget: &qq_protocol::GoalBudget,
        check: Option<&qq_protocol::GoalCheck>,
        specification_at_ms: Option<u64>,
    ) -> Result<(), SessionRuntimeError> {
        let duration_exceeded = match specification_at_ms {
            Some(at) => budget.deadline_ms > at.saturating_add(self.max_duration_ms),
            None => self.max_duration_ms < Self::default().max_duration_ms,
        };
        if budget.max_goal_runs > self.max_goal_runs || duration_exceeded {
            return Err(SessionRuntimeError::InvalidGoal(
                GoalValidationError::Budget,
            ));
        }
        if check.is_some_and(|check| check.timeout_ms > self.max_check_timeout_ms) {
            return Err(SessionRuntimeError::InvalidGoal(GoalValidationError::Check));
        }
        if self.per_run.max_duration_ms.is_some_and(|cap| {
            budget
                .per_run
                .max_duration_ms
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_model_turns.is_some_and(|cap| {
            budget
                .per_run
                .max_model_turns
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_tool_calls.is_some_and(|cap| {
            budget
                .per_run
                .max_tool_calls
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_total_tokens.is_some_and(|cap| {
            budget
                .per_run
                .max_total_tokens
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_cost_usd_nanos.is_some_and(|cap| {
            budget
                .per_run
                .max_cost_usd_nanos
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_input_tokens.is_some_and(|cap| {
            budget
                .per_run
                .max_input_tokens
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_output_tokens.is_some_and(|cap| {
            budget
                .per_run
                .max_output_tokens
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_tool_output_bytes.is_some_and(|cap| {
            budget
                .per_run
                .max_tool_output_bytes
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self
            .per_run
            .max_children
            .is_some_and(|cap| budget.per_run.max_children.is_none_or(|limit| limit > cap))
        {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        if self.per_run.max_concurrent_children.is_some_and(|cap| {
            budget
                .per_run
                .max_concurrent_children
                .is_none_or(|limit| limit > cap)
        }) {
            return Err(SessionRuntimeError::InvalidRunLimits);
        }
        Ok(())
    }
}

pub(super) fn load(
    connection: &Connection,
    goal: qq_protocol::GoalId,
) -> Result<(GoalPolicy, Option<u64>), SessionRuntimeError> {
    let (encoded, at): (Option<String>, Option<u64>) = connection.query_row(
        "SELECT policy_json,specification_at_ms FROM session_goals WHERE goal_id=?1",
        [goal.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    match (encoded, at) {
        (None, None) => Ok((GoalPolicy::default(), None)),
        (Some(encoded), at) if encoded.len() <= 8192 => {
            let policy: GoalPolicy = serde_json::from_str(&encoded)?;
            Ok((policy.validate()?, at))
        }
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

pub(super) fn save(
    connection: &Connection,
    goal: qq_protocol::GoalId,
    policy: GoalPolicy,
    at: Option<u64>,
) -> Result<(), SessionRuntimeError> {
    policy.validate()?;
    let encoded = serde_json::to_string(&policy)?;
    connection.execute(
        "UPDATE session_goals SET policy_json=?2,specification_at_ms=?3 WHERE goal_id=?1",
        params![goal.to_string(), encoded, at],
    )?;
    Ok(())
}

fn bound_goal_owners(
    connection: &Connection,
    identity: RunIdentity,
) -> Result<Vec<(RunId, SessionId, Option<qq_protocol::GoalId>)>, SessionRuntimeError> {
    if identity.kind == RunKind::GoalCheck {
        let id: Option<String> = connection.query_row(
            "SELECT goal_id FROM runs WHERE id=?1",
            [identity.run_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(vec![(
            identity.run_id,
            identity.session_id,
            id.as_deref().map(parse_id).transpose()?,
        )])
    } else {
        let base: Option<String> = connection.query_row(
            "SELECT CASE WHEN kind='prompt' THEN id ELSE auto_compaction_for_run_id END FROM runs WHERE id=?1",
            [identity.run_id.to_string()], |row| row.get(0))?;
        let Some(base) = base else {
            return Ok(Vec::new());
        };
        goals::paid_goal_owners(connection, parse_id(&base)?)
    }
}

fn bound_goals(
    connection: &Connection,
    identity: RunIdentity,
) -> Result<Vec<(qq_protocol::GoalSnapshot, bool)>, SessionRuntimeError> {
    let mut result: Vec<(qq_protocol::GoalSnapshot, bool)> = Vec::new();
    for (_, session, id) in bound_goal_owners(connection, identity)? {
        let Some(id) = id else { continue };
        if result.iter().any(|(goal, _)| goal.goal_id == id) {
            continue;
        }
        let (payload, live): (String, bool) = connection.query_row(
            "SELECT payload_json,live FROM session_goals WHERE goal_id=?1 AND session_id=?2",
            params![id.to_string(), session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        result.push((serde_json::from_str(&payload)?, live));
    }
    Ok(result)
}

fn stop_goal(
    connection: &Connection,
    store_id: StoreId,
    workspace: WorkspaceId,
    goal: &mut qq_protocol::GoalSnapshot,
    cause: qq_protocol::GoalNeedsUser,
    now: u64,
) -> Result<Vec<RunId>, SessionRuntimeError> {
    goal.status = if matches!(cause, qq_protocol::GoalNeedsUser::Approval { .. }) {
        qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::NeedsUser { cause },
        }
    } else {
        match goals::goal_exhaustion(goal, now) {
            Some(reason) => qq_protocol::GoalStatus::BudgetExhausted { reason },
            None => qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::NeedsUser { cause },
            },
        }
    };
    goals::save(connection, goal)?;
    append_event(
        connection,
        EventContext::for_session(store_id, workspace, goal.session_id, None, now),
        SessionEvent::GoalUpdated {
            goal: Box::new(goal.clone()),
        },
    )?;
    commands::cancel_goal_work(
        connection,
        store_id,
        goal.session_id,
        goal.goal_id,
        None,
        now,
        true,
    )
}

fn apply_refresh(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    policy: Result<GoalPolicy, GoalPolicyError>,
) -> Result<(bool, Vec<RunId>), SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let cancelled: bool = transaction.query_row(
        "SELECT cancel_requested OR outcome_json IS NOT NULL FROM runs WHERE id=?1",
        [identity.run_id.to_string()],
        |row| row.get(0),
    )?;
    if cancelled {
        return Ok((false, Vec::new()));
    }
    let mut allowed = true;
    let mut signals = Vec::new();
    let now = now_ms();
    for (mut goal, live) in bound_goals(&transaction, identity)? {
        if !live || goal.status != qq_protocol::GoalStatus::Active {
            allowed = false;
            continue;
        }
        let (previous, at) = load(&transaction, goal.goal_id)?;
        let valid = policy.as_ref().is_ok_and(|policy| {
            policy
                .validate_specification(&goal.budget, goal.check.as_ref(), at)
                .is_ok()
        });
        if !valid {
            allowed = false;
            signals.extend(stop_goal(&transaction, store_id, identity.workspace_id, &mut goal,
                qq_protocol::GoalNeedsUser::Configuration { message:
                    "Goal settings cannot be validated against current workspace ceilings; inspect configuration and edit or resume the goal.".to_owned()
                }, now)?);
        } else if let Ok(policy) = policy
            && policy != previous
        {
            save(&transaction, goal.goal_id, policy, at)?;
        }
    }
    transaction.commit()?;
    Ok((allowed, signals))
}

pub(super) async fn refresh(
    inner: &runtime::SessionRuntimeInner,
    claimed: &ClaimedRun,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<bool, SessionRuntimeError> {
    if claimed.goal.is_none() && claimed.goal_spend_owner.is_none() {
        return Ok(true);
    }
    if *cancellation.borrow() {
        return Ok(false);
    }
    let deadline = claimed.check_deadline.map(|(_, at)| at).or_else(|| {
        claimed.goal.as_ref().map(|goal| {
            tokio::time::Instant::now()
                + Duration::from_millis(goal.budget.deadline_ms.saturating_sub(now_ms()))
        })
    });
    let policy = tokio::select! {
        biased;
        _ = cancellation.changed() => return Ok(false),
        _ = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        } => return Ok(true),
        result = inner.resolve_goal_policy(Path::new(&claimed.workspace)) => result,
    };
    let identity = claimed.identity;
    let store_id = inner.store.store_id();
    let (allowed, signals) = inner
        .store
        .call(store::Priority::AwaitControl, move |connection| {
            apply_refresh(connection, store_id, identity, policy)
        })
        .await?;
    for run in signals {
        inner.cancel(run);
    }
    Ok(allowed)
}

pub(super) fn approval_timeout(
    connection: &Connection,
    identity: RunIdentity,
) -> Result<Option<Duration>, SessionRuntimeError> {
    let now = now_ms();
    let mut timeout = None;
    for (goal, live) in bound_goals(connection, identity)? {
        if live && goal.status == qq_protocol::GoalStatus::Active {
            let (policy, _) = load(connection, goal.goal_id)?;
            let bound = Duration::from_millis(
                policy
                    .approval_wait_ms
                    .min(goal.budget.deadline_ms.saturating_sub(now)),
            );
            timeout = Some(timeout.map_or(bound, |previous: Duration| previous.min(bound)));
        }
    }
    Ok(timeout)
}

pub(super) fn expired_approval_owner(
    connection: &Connection,
    identity: RunIdentity,
    expired_owners: &[RunId],
) -> Result<bool, SessionRuntimeError> {
    for (owner, session, id) in bound_goal_owners(connection, identity)? {
        let Some(id) = id else { continue };
        let Some(goal) = goals::load_live(connection, session)? else {
            continue;
        };
        if goal.goal_id != id
            || goal.status != qq_protocol::GoalStatus::Active
            || (goal.budget.deadline_ms > now_ms() && !expired_owners.contains(&owner))
        {
            continue;
        }
        let cancelled: bool = connection.query_row(
            "SELECT cancel_requested FROM runs WHERE id=?1",
            [owner.to_string()],
            |row| row.get(0),
        )?;
        if !cancelled {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn pause_for_approval(
    connection: &Connection,
    store_id: StoreId,
    identity: RunIdentity,
    call: ToolCallId,
    now: u64,
    expired_owners: &[RunId],
) -> Result<Vec<RunId>, SessionRuntimeError> {
    let cancelled: bool = connection.query_row(
        "SELECT cancel_requested OR outcome_json IS NOT NULL FROM runs WHERE id=?1",
        [identity.run_id.to_string()],
        |row| row.get(0),
    )?;
    if cancelled && !expired_approval_owner(connection, identity, expired_owners)? {
        return Ok(Vec::new());
    }
    let mut signals = Vec::new();
    for (mut goal, live) in bound_goals(connection, identity)? {
        if live && goal.status == qq_protocol::GoalStatus::Active {
            signals.extend(stop_goal(
                connection,
                store_id,
                identity.workspace_id,
                &mut goal,
                qq_protocol::GoalNeedsUser::Approval { tool_call_id: call },
                now,
            )?);
        }
    }
    Ok(signals)
}
