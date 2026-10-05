use std::{future::Future, pin::Pin};

use qq_protocol::{ChildAuthority, SessionId, SessionPurpose, TokenUsage, ToolCallId};

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
    /// The child was admitted detached and is still running: `content` is
    /// the admission receipt, and the answer (with its spend) arrives later
    /// through [`SubagentSpawner::deliver`]. `spend` is `NONE` here.
    pub(crate) detached: bool,
}

/// One `spawn_agent` call as the run loop hands it to the spawner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequest {
    /// The parent's `spawn_agent` tool call; the child records it so clients
    /// can place the child under the call that created it.
    pub(crate) call_id: ToolCallId,
    pub(crate) task: String,
    pub(crate) model: Option<String>,
    /// The effort the child runs at. `None` inherits the parent's session
    /// pin (the legacy worker path and audits); a roster spawn derives one
    /// from the entry via `qq_protocol::child_reasoning_effort`.
    pub(crate) reasoning_effort: Option<qq_provider::ReasoningEffort>,
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
    /// Return on durable admission and deliver the answer at a later turn
    /// boundary (ADR-0054 § 4). Only unbounded read task spawns detach.
    pub(crate) detached: bool,
}

pub(crate) type SpawnAgentFuture =
    Pin<Box<dyn Future<Output = SpawnAgentOutcome> + Send + 'static>>;

/// One detached child's answer as it entered the parent's context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveredChild {
    /// The notice message the next request carries, already durable.
    pub(crate) notice: String,
    /// Whether the child answered (a failed or cancelled child did not, and
    /// an interim report from a child still working is not an answer).
    pub(crate) answered: bool,
    pub(crate) spend: SpawnAgentSpend,
}

/// A delivery the store could not commit. The run fails (a server failure,
/// as a failed persistence does): an answer the parent would act on must be
/// durable before it joins context.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DeliveryError {
    #[error("a sub-agent answer could not be delivered durably: {0}")]
    Store(#[source] crate::sessions::SessionRuntimeError),
    #[error("the sub-agent owner registry is unavailable")]
    Registry,
}

/// Where one background child stands, as `wait_agents` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildStatus {
    /// Settled; its answer enters the parent's context at the next boundary.
    Finished,
    Working,
    /// Not an outstanding background child of this run: never one, or its
    /// answer was already delivered.
    Unknown,
}

/// What one `wait_agents` call saw when it returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitReport {
    /// The children waited for, in request order (every outstanding child
    /// when no ids were named).
    pub(crate) children: Vec<(SessionId, ChildStatus)>,
    pub(crate) timed_out: bool,
}

impl WaitReport {
    /// The tool result text. Answers are not repeated here: each finished
    /// child's answer follows this result as a delivered notice, through the
    /// one delivery path that keeps it exactly-once (ADR-0054 § 4).
    pub(crate) fn render(&self, timeout_seconds: u64) -> String {
        if self.children.is_empty() {
            return "No background sub-agents are outstanding; there is nothing to wait for."
                .to_owned();
        }
        let mut text = String::new();
        if self.timed_out {
            text.push_str(&format!(
                "Waited {timeout_seconds}s; the sub-agents still working keep working.\n"
            ));
        }
        for (id, status) in &self.children {
            text.push_str(&format!("Sub-agent {id}: "));
            text.push_str(match status {
                ChildStatus::Finished => "finished; its answer follows as a runtime notice.",
                ChildStatus::Working => "still working.",
                ChildStatus::Unknown => {
                    "not a background sub-agent of this run that is still outstanding (its \
                     answer may already have reached you)."
                }
            });
            text.push('\n');
        }
        text.pop();
        text
    }
}

/// What `cancel_agent` did to one background child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    /// Stopped and settled; its answer (the reason and its latest report)
    /// enters the parent's context at the next boundary.
    Cancelled,
    /// It had already finished; its answer is on its way.
    AlreadyFinished,
    Unknown,
}

impl CancelOutcome {
    pub(crate) fn render(self, id: SessionId) -> (String, bool) {
        match self {
            Self::Cancelled => (
                format!(
                    "Sub-agent {id} was cancelled. What it reported so far follows as a runtime \
                     notice."
                ),
                false,
            ),
            Self::AlreadyFinished => (
                format!(
                    "Sub-agent {id} had already finished; its answer follows as a runtime notice."
                ),
                false,
            ),
            Self::Unknown => (
                format!(
                    "Sub-agent {id} is not a background sub-agent of this run that is still \
                     outstanding (its answer may already have reached you)."
                ),
                true,
            ),
        }
    }
}

pub(crate) type WaitFuture =
    Pin<Box<dyn Future<Output = Result<WaitReport, DeliveryError>> + Send>>;
pub(crate) type CancelFuture =
    Pin<Box<dyn Future<Output = Result<CancelOutcome, DeliveryError>> + Send>>;

pub(crate) type DeliverFuture =
    Pin<Box<dyn Future<Output = Result<Vec<DeliveredChild>, DeliveryError>> + Send>>;
pub(crate) type ChildWaitFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

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
    /// Detached children are stopped too; their spend is charged by the
    /// delivery that settlement commits, never here.
    fn drain(&self) -> ChildDrainFuture;
    /// Stops only the blocking children a tool call still awaits, leaving
    /// detached children running: an interrupt drops the turn's tool calls,
    /// not the parent's delegated work.
    /// The default drains everything: a spawner that never detaches has
    /// only blocking children.
    fn drain_attached(&self) -> ChildDrainFuture {
        self.drain()
    }
    /// Detached children whose answers are not yet delivered, running or
    /// settled.
    fn outstanding_detached(&self) -> usize {
        0
    }
    /// A detached child has settled and its answer awaits delivery.
    fn settled_detached(&self) -> bool {
        false
    }
    /// Resolves when a detached child settles, at once if one already has
    /// and is undelivered; pending forever when none is outstanding.
    fn child_settled(&self) -> ChildWaitFuture {
        Box::pin(std::future::pending())
    }
    /// Commits every settled detached child's answer into the parent's
    /// context for its turn `turn_ordinal`, in one transaction, before that
    /// request is built. Each answer is returned once, with its spend.
    fn deliver(&self, _turn_ordinal: u32) -> DeliverFuture {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
    /// `wait_agents`: resolves when every named background child has
    /// settled (or, with no ids, when any outstanding one has, or none is
    /// outstanding), or when `timeout` passes. Settled answers are delivered
    /// at the next boundary, not here.
    fn wait_children(
        &self,
        ids: Option<Vec<SessionId>>,
        _timeout: std::time::Duration,
    ) -> WaitFuture {
        let children = ids
            .unwrap_or_default()
            .into_iter()
            .map(|id| (id, ChildStatus::Unknown))
            .collect();
        Box::pin(std::future::ready(Ok(WaitReport {
            children,
            timed_out: false,
        })))
    }
    /// `cancel_agent`: stops one background child and resolves once it has
    /// settled, so its answer is delivered at the next boundary.
    fn cancel_child(&self, _id: SessionId) -> CancelFuture {
        Box::pin(std::future::ready(Ok(CancelOutcome::Unknown)))
    }
    /// Test hook: the parent entered its wait for answers.
    #[cfg(test)]
    fn waiting_for_test(&self) {}
}

/// The dispatcher's defensive answer when `spawn_agent` is called by a run
/// that has no spawner (a child session, or a run outside the session layer).
pub(crate) const SPAWN_UNAVAILABLE_RESULT: &str = "spawn_agent is not available in this \
     session: this run is at the deepest delegation level its configuration permits.";
