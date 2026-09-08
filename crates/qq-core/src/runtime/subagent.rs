use std::{future::Future, pin::Pin};

use qq_protocol::{ChildAuthority, SessionPurpose, TokenUsage, ToolCallId};

/// The spend one spawned sub-agent reports back to its parent. Every field is
/// `None` when unknown, never zero: the parent's meter turns an unknown into
/// the matching `*_unknown` exhaustion rather than a silent pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpawnAgentSpend {
    /// The child run's estimated cost, charged against the parent's cost
    /// budget.
    pub(crate) cost_usd_nanos: Option<u64>,
    /// The child run's total token usage, charged against the parent's token
    /// budgets exactly like the parent's own turns.
    pub(crate) usage: Option<TokenUsage>,
}

impl SpawnAgentSpend {
    /// A child that never ran spent nothing.
    pub(crate) const NONE: Self = Self {
        cost_usd_nanos: Some(0),
        usage: Some(TokenUsage {
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: Some(0),
        }),
    };

    /// A child whose spend could not be read.
    pub(crate) const UNKNOWN: Self = Self {
        cost_usd_nanos: None,
        usage: None,
    };
}

/// The outcome one spawned sub-agent call returns to its parent. The content
/// flows through the same bounded-result truncation as built-in tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnAgentOutcome {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) spend: SpawnAgentSpend,
    /// The child session that ran, when one was created.
    pub(crate) session_id: Option<qq_protocol::SessionId>,
}

/// One `spawn_agent` call as the run loop hands it to the spawner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequest {
    /// The parent's `spawn_agent` tool call; the child records it so clients
    /// can place the child under the call that created it.
    pub(crate) call_id: ToolCallId,
    pub(crate) task: String,
    pub(crate) model: Option<String>,
    /// The authority the parent asked for. `Write` is admitted only when the
    /// roster allows write children and a reviewer is installed; the child
    /// then runs `Supervised`, never above.
    pub(crate) authority: ChildAuthority,
    /// The parent's remaining budget at spawn time. The child is admitted
    /// with these bounds, never with the parent's original caps.
    pub(crate) budget: super::ChildBudget,
    /// Why the child exists: an ordinary delegated task, or the parent's
    /// final-answer audit.
    pub(crate) purpose: SessionPurpose,
}

pub(crate) type SpawnAgentFuture =
    Pin<Box<dyn Future<Output = SpawnAgentOutcome> + Send + 'static>>;

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("child execution cleanup is unavailable")]
pub(crate) struct ChildCleanupError;

pub(crate) type ChildDrainFuture =
    Pin<Box<dyn Future<Output = Result<Vec<SpawnAgentSpend>, ChildCleanupError>> + Send>>;

/// Runs one sub-agent task to completion on behalf of a `spawn_agent` call.
/// The session runtime installs a spawner for eligible runs only: child
/// sessions (and session-less runs) get none, so the tool is neither declared
/// nor dispatchable there. Dropping the returned future must cancel the
/// in-flight child work.
pub(crate) trait SubagentSpawner: Send + Sync {
    fn spawn(&self, request: SpawnRequest) -> SpawnAgentFuture;
    /// Called synchronously after the parent charges a returned child's spend.
    fn acknowledge(&self, call_id: ToolCallId);
    /// Stops outstanding children and returns all spend not yet acknowledged.
    /// Dropping this future must preserve ownership and unconsumed receipts.
    fn drain(&self) -> ChildDrainFuture;
}

/// The dispatcher's defensive answer when `spawn_agent` is called by a run
/// that has no spawner (a child session, or a run outside the session layer).
pub(crate) const SPAWN_UNAVAILABLE_RESULT: &str = "spawn_agent is not available in this \
     session: this run is at the deepest delegation level its configuration permits.";
