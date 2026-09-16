use std::{future::Future, pin::Pin};

use qq_protocol::RunFailureKind;

use super::RuntimeToolCall;
use crate::sessions::ReviewSpend;

/// The runtime's answer for one requested tool call after policy and, when
/// required, an approval round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateDecision {
    Execute,
    Deny {
        message: String,
    },
    /// The user answered an `ask_user` call: `result` is the call's complete,
    /// already-persisted result, so nothing dispatches.
    Answered {
        result: String,
    },
    Fail {
        kind: RunFailureKind,
        message: String,
    },
    /// A decision the configured reviewer model reached (or was consulted
    /// for): the wrapped decision stands and `spend` is charged to the run's
    /// budget, so reviewer cost is never hidden.
    Reviewed {
        decision: Box<GateDecision>,
        spend: ReviewSpend,
    },
}

pub(crate) type ToolGateFuture = Pin<Box<dyn Future<Output = GateDecision> + Send + 'static>>;

/// Resolves approval policy for tool calls before they execute. The session
/// runtime installs a gate that persists approval state and waits for clients;
/// gate-less runs fall back to a static policy that cannot prompt.
pub(crate) trait ToolGate: Send + Sync {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture;
}
