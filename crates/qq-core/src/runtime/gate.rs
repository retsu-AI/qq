use std::{future::Future, pin::Pin};

use qq_protocol::RunFailureKind;

use super::RuntimeToolCall;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateDecision {
    Execute,
    Deny {
        message: String,
    },
    Answered {
        result: String,
    },
    Fail {
        kind: RunFailureKind,
        message: String,
    },
}

pub(crate) type ToolGateFuture = Pin<Box<dyn Future<Output = GateDecision> + Send + 'static>>;

pub(crate) trait ToolGate: Send + Sync {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture;
}
