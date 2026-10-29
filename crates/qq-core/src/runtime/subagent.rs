use std::{future::Future, pin::Pin};

use qq_protocol::{ChildAuthority, SessionId, SessionPurpose, TokenUsage, ToolCallId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpawnAgentSpend {
    pub(crate) cost_usd_nanos: Option<u64>,
    pub(crate) usage: Option<TokenUsage>,
}

impl SpawnAgentSpend {
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

    pub(crate) const UNKNOWN: Self = Self {
        cost_usd_nanos: None,
        usage: None,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnAgentOutcome {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) spend: SpawnAgentSpend,
    pub(crate) session_id: Option<qq_protocol::SessionId>,
    pub(crate) detached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequest {
    pub(crate) call_id: ToolCallId,
    pub(crate) task: String,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub(crate) authority: ChildAuthority,
    pub(crate) budget: super::ChildBudget,
    pub(crate) purpose: SessionPurpose,
    pub(crate) detached: bool,
}

pub(crate) type SpawnAgentFuture =
    Pin<Box<dyn Future<Output = SpawnAgentOutcome> + Send + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveredChild {
    pub(crate) notice: String,
    pub(crate) answered: bool,
    pub(crate) interim: bool,
    pub(crate) spend: SpawnAgentSpend,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DeliveryError {
    #[error("a sub-agent answer could not be delivered durably: {0}")]
    Store(#[source] crate::sessions::SessionRuntimeError),
    #[error("the sub-agent owner registry is unavailable")]
    Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildStatus {
    Finished,
    Working,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitReport {
    pub(crate) children: Vec<(SessionId, ChildStatus)>,
    pub(crate) timed_out: bool,
}

impl WaitReport {
    pub(crate) fn render(&self, timeout_seconds: u64) -> String {
        use std::fmt::Write as _;
        if self.children.is_empty() {
            return "No background sub-agents are outstanding; there is nothing to wait for."
                .to_owned();
        }
        let mut text = String::new();
        if self.timed_out {
            let _ = writeln!(
                text,
                "Waited {timeout_seconds}s; the sub-agents still working keep working."
            );
        }
        for (id, status) in &self.children {
            let _ = write!(text, "Sub-agent {id}: ");
            text.push_str(match status {
                ChildStatus::Finished => "finished; its answer arrives as a runtime notice.",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    Cancelled,
    AlreadyFinished,
    Unknown,
}

impl CancelOutcome {
    pub(crate) fn render(self, id: SessionId) -> (String, bool) {
        match self {
            Self::Cancelled => (
                format!(
                    "Sub-agent {id} was cancelled. What it reported so far arrives as a runtime \
                     notice."
                ),
                false,
            ),
            Self::AlreadyFinished => (
                format!(
                    "Sub-agent {id} had already finished; its answer arrives as a runtime notice."
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportDelivery {
    Always,
    WithAnswers,
}

pub(crate) type DeliverFuture =
    Pin<Box<dyn Future<Output = Result<Vec<DeliveredChild>, DeliveryError>> + Send>>;
pub(crate) type ChildWaitFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("child execution cleanup is unavailable")]
pub(crate) struct ChildCleanupError;

pub(crate) type ChildDrainFuture =
    Pin<Box<dyn Future<Output = Result<Vec<SpawnAgentSpend>, ChildCleanupError>> + Send>>;

pub(crate) trait SubagentSpawner: Send + Sync {
    fn spawn(&self, request: SpawnRequest) -> SpawnAgentFuture;
    fn acknowledge(&self, call_id: ToolCallId);
    fn drain(&self) -> ChildDrainFuture;
    fn drain_attached(&self) -> ChildDrainFuture {
        self.drain()
    }
    fn outstanding_detached(&self) -> usize {
        0
    }
    fn settled_detached(&self) -> bool {
        false
    }
    fn child_settled(&self) -> ChildWaitFuture {
        Box::pin(std::future::pending())
    }
    fn deliver(&self, _turn_ordinal: u32, _reports: ReportDelivery) -> DeliverFuture {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
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
    fn cancel_child(&self, _id: SessionId) -> CancelFuture {
        Box::pin(std::future::ready(Ok(CancelOutcome::Unknown)))
    }
    #[cfg(test)]
    fn waiting_for_test(&self) {}
}

pub(crate) const SPAWN_UNAVAILABLE_RESULT: &str = "spawn_agent is not available in this \
     session: this run is at the deepest delegation level its configuration permits.";
