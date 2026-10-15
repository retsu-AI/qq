//! Configuration-neutral goal ceilings and durable operational settings.
use super::*;

/// Effective workspace goal bounds. Applications resolve this before admitting work.
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

/// A current workspace policy could not be safely resolved.
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
            // Legacy stores prove only the shipped seven-day ceiling.
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
    // Unknown legacy anchors remain unknown until a client specification edit.
    let encoded = serde_json::to_string(&policy)?;
    connection.execute(
        "UPDATE session_goals SET policy_json=?2,specification_at_ms=?3 WHERE goal_id=?1",
        params![goal.to_string(), encoded, at],
    )?;
    Ok(())
}
