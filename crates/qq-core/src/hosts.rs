use std::{future::Future, pin::Pin};

use qq_provider::ToolSpec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::RunCancellation;

pub const MCP_TOOL_PREFIX: &str = "mcp__";
pub const EMBEDDED_TOOL_PREFIX: &str = "ext__";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolHints {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destructive: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_world: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostTool {
    pub spec: ToolSpec,
    pub hints: ToolHints,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostReadiness {
    Ready,
    Degraded { message: String },
    Unavailable { message: String },
    ShutDown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostCatalog {
    pub generation: u64,
    pub tools: Vec<HostTool>,
    pub readiness: HostReadiness,
}

impl HostCatalog {
    #[must_use]
    pub fn empty(generation: u64) -> Self {
        Self {
            generation,
            tools: Vec::new(),
            readiness: HostReadiness::Ready,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HostCallError {
    #[error("the tool call exceeded its deadline")]
    Timeout,
    #[error("the tool call was cancelled")]
    Cancelled,
    #[error("the host is unavailable: {0}")]
    Unavailable(String),
    #[error("the host is at its concurrency bound; retry later")]
    Overloaded,
    #[error("the host returned an invalid result: {0}")]
    InvalidResult(String),
    #[error("the host refused the call: {0}")]
    Refused(String),
    #[error("no tool named {0:?} is served by this host")]
    UnknownTool(String),
    #[error("the host has been shut down")]
    ShutDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolResult {
    pub content: String,
    pub is_error: bool,
}

pub type HostCallFuture =
    Pin<Box<dyn Future<Output = Result<HostToolResult, HostCallError>> + Send + 'static>>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedHostToolResult {
    pub result: HostToolResult,
    pub result_digest: Option<[u8; 32]>,
}

pub type ObservedHostCallFuture =
    Pin<Box<dyn Future<Output = Result<ObservedHostToolResult, HostCallError>> + Send + 'static>>;

pub enum HostCallObservation {
    Rendered(HostCallFuture),
    Observed(ObservedHostCallFuture),
}

pub type HostShutdownFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub trait ExternalToolHost: Send + Sync {
    fn name(&self) -> &str;

    fn catalog_blocking(&self) -> HostCatalog;

    fn catalog_is_current(&self, generation: u64) -> bool;

    fn config_grants(&self) -> Vec<String>;

    fn call(&self, name: String, arguments: String, cancelled: RunCancellation) -> HostCallFuture;

    fn call_observed(
        &self,
        name: String,
        arguments: String,
        cancelled: RunCancellation,
    ) -> HostCallObservation {
        HostCallObservation::Rendered(self.call(name, arguments, cancelled))
    }

    fn readiness(&self) -> HostReadiness;

    fn shutdown(&self) -> HostShutdownFuture;
}

pub(crate) fn host_error_result(error: &HostCallError) -> crate::tools::ToolOutput {
    let mut result = crate::tools::bounded_result(error.to_string(), true);
    result.disposition = match error {
        HostCallError::Cancelled => crate::tools::ResultDisposition::Interrupted,
        HostCallError::Refused(_) | HostCallError::UnknownTool(_) | HostCallError::Overloaded => {
            crate::tools::ResultDisposition::Refused
        }
        HostCallError::Timeout
        | HostCallError::Unavailable(_)
        | HostCallError::InvalidResult(_)
        | HostCallError::ShutDown => crate::tools::ResultDisposition::Unobserved,
    };
    result
}

pub mod embedded;
pub use embedded::{
    DEFAULT_EMBEDDED_CALL_TIMEOUT, DEFAULT_EMBEDDED_MAX_CONCURRENT_CALLS, EmbeddedHostError,
    EmbeddedToolFuture, EmbeddedToolHandler, EmbeddedToolHost, EmbeddedToolHostBuilder,
    MAX_EMBEDDED_ARGUMENT_BYTES, MAX_EMBEDDED_RESULT_BYTES,
};

#[doc(hidden)]
pub mod conformance;
