use crate::{GoalId, RunId, RunLimits, SessionId, TokenUsage, ToolCallId};

pub const DEFAULT_GOAL_RUN_MODEL_TURNS: u32 = 256;
use serde::{Deserialize, Serialize};

/// A client specification/control generation; model data writes retain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalIdentity {
    pub goal_id: GoalId,
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBudget {
    pub max_goal_runs: u32,
    pub deadline_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_nanos: Option<u64>,
    #[serde(default = "default_goal_run_limits")]
    pub per_run: RunLimits,
}

fn default_goal_run_limits() -> RunLimits {
    RunLimits {
        max_model_turns: Some(DEFAULT_GOAL_RUN_MODEL_TURNS),
        ..RunLimits::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCheck {
    pub command: String,
    #[serde(default = "default_check_timeout")]
    pub timeout_ms: u64,
}
const fn default_check_timeout() -> u64 {
    600_000
}

/// Authority belongs to this goal's exact check and workspace, never an
/// ordinary tool approval. The row retains consumed one-shot authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCheckAuthorization {
    Pending,
    Once,
    Consumed,
    ForGoal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCheckScope {
    Once,
    ForGoal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalControl {
    Pause,
    Resume,
    Clear,
    ApproveCheck {
        scope: GoalCheckScope,
        command: String,
    },
    DenyCheck,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalStatus {
    Active,
    Paused { reason: GoalPauseReason },
    Achieved,
    Blocked { reason: GoalBlockedReason },
    BudgetExhausted { reason: GoalExhaustion },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalPauseReason {
    User,
    NeedsUser { cause: GoalNeedsUser },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalNeedsUser {
    CheckApproval,
    CheckInterrupted,
    Approval { tool_call_id: ToolCallId },
    Authentication { message: String },
    Configuration { message: String },
    Credentials { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalBlockedReason {
    Model { reason: String },
    NoProgress,
    RepeatedFailure,
    CheckFailing,
    AuditExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalExhaustion {
    Runs,
    Tokens,
    Cost,
    Deadline,
    TokensUnknown,
    CostUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChecklistState {
    Pending,
    InProgress,
    Done,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalChecklistItem {
    pub id: u8,
    pub text: String,
    pub state: GoalChecklistState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalWait {
    pub until_ms: u64,
    pub reason: String,
}

/// The accepted proposal remains distinct from independently verified achievement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalAchievementClaim {
    pub run_id: RunId,
    pub revision: u32,
    pub evidence: String,
}

/// Usage and cost preserve uncertainty independently, as run accounting does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCounters {
    pub runs: u32,
    pub usage: Option<TokenUsage>,
    pub estimated_cost_usd_nanos: Option<u64>,
    pub audits: u8,
    pub check_failures: u8,
    pub no_progress_streak: u8,
    pub failure_streak: u8,
}
impl Default for GoalCounters {
    fn default() -> Self {
        Self {
            runs: 0,
            usage: Some(TokenUsage::default()),
            estimated_cost_usd_nanos: Some(0),
            audits: 0,
            check_failures: 0,
            no_progress_streak: 0,
            failure_streak: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSnapshot {
    pub session_id: SessionId,
    pub goal_id: GoalId,
    pub revision: u32,
    pub objective: String,
    pub check: Option<GoalCheck>,
    pub check_authorization: Option<GoalCheckAuthorization>,
    pub budget: GoalBudget,
    pub checklist: Vec<GoalChecklistItem>,
    pub notes: String,
    pub status: GoalStatus,
    pub counters: GoalCounters,
    pub next_run_at_ms: u64,
    pub wait: Option<GoalWait>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievement_claim: Option<GoalAchievementClaim>,
    pub audit_pending: bool,
    pub audit_revision: Option<u32>,
    pub pending_run_id: Option<RunId>,
    pub check_output: Option<String>,
}
