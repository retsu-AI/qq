//! The seam between the run loop and external tool hosts.
//!
//! `qq-core` never speaks MCP or any embedder's callback protocol itself: the
//! composition root wires a concrete host (the `qq-mcp` manager, or an
//! application's [`EmbeddedToolHost`](crate::EmbeddedToolHost)) through this
//! trait, exactly as providers arrive through [`qq_provider::Provider`].
//!
//! A host contributes an immutable catalog snapshot at plan compile time and
//! executes one selected call at a time under its own bounds. Hosts perform
//! no implicit retry: an ambiguous outcome is returned as such.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, atomic::AtomicBool},
};

use qq_provider::ToolSpec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Namespace prefix for MCP tool names: `mcp__<server>__<tool>`.
pub const MCP_TOOL_PREFIX: &str = "mcp__";
/// Namespace prefix for embedded host tool names: `ext__<host>__<tool>`.
pub const EMBEDDED_TOOL_PREFIX: &str = "ext__";

/// Advisory hints a host attaches to a tool, in MCP's vocabulary. Hints
/// never grant authority: approval policy treats every external tool alike
/// regardless of what its host claims, and the hints are recorded for
/// diagnosis and advertised to clients only.
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

/// One tool an external host declares.
#[derive(Debug, Clone, PartialEq)]
pub struct HostTool {
    pub spec: ToolSpec,
    pub hints: ToolHints,
}

/// Why a host is not currently serving calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostReadiness {
    Ready,
    /// Some or all backends are unavailable; the message names them.
    Degraded {
        message: String,
    },
    Unavailable {
        message: String,
    },
    ShutDown,
}

/// A host's tool declarations at one instant, with the generation the
/// snapshot was taken under. The plan compiles the tools in; the generation
/// lets the plan cache notice a later change without refetching.
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

/// A typed failure of one host call. Every variant becomes an ordinary
/// `is_error` tool result for the model, never a run failure; the type exists
/// so the runtime, traces, and conformance tests can tell them apart.
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

/// A successful host call. `is_error` carries the tool's own error
/// signalling (the server ran the tool and it reported failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolResult {
    pub content: String,
    pub is_error: bool,
}

pub type HostCallFuture =
    Pin<Box<dyn Future<Output = Result<HostToolResult, HostCallError>> + Send + 'static>>;
pub type HostShutdownFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// An external source of tools as the plan compiler and run loop see them.
///
/// Implementations own connection management and must keep every call
/// bounded in time. The run loop awaits `call` once per dispatched call; the
/// plan compiler calls `catalog_blocking` once per compile from a blocking
/// context; the plan cache calls `catalog_is_current` on every lookup and it
/// must be cheap and synchronous.
pub trait ExternalToolHost: Send + Sync {
    /// Stable host identity, used in diagnostics and capabilities. Tool names
    /// carry their own namespace prefix independent of this.
    fn name(&self) -> &str;

    /// Fetches the current declarations, connecting if necessary. May block
    /// the calling thread for a bounded time; never called on an async
    /// worker. An unavailable backend contributes no tools and is reported
    /// through the catalog's readiness rather than an error.
    fn catalog_blocking(&self) -> HostCatalog;

    /// Whether a catalog taken under `generation` still describes this host.
    /// Returning `false` makes the next plan lookup recompile; a host whose
    /// backends failed may return `false` after a bounded retry interval so
    /// a plan does not stay degraded forever.
    fn catalog_is_current(&self, generation: u64) -> bool;

    /// Exact namespaced tool names granted by configuration allowlists,
    /// merged into policy evaluation as session-style grants.
    fn config_grants(&self) -> Vec<String>;

    /// Executes one namespaced call. `cancelled` is the run's cancellation
    /// flag; a cancelled call must return promptly without wedging shared
    /// state. Dropping the future must be safe.
    fn call(&self, name: String, arguments: String, cancelled: Arc<AtomicBool>) -> HostCallFuture;

    fn readiness(&self) -> HostReadiness;

    /// Stops serving calls and releases backends. Calls in flight settle as
    /// [`HostCallError::ShutDown`] or their own outcome; new calls are
    /// refused. Bounded.
    fn shutdown(&self) -> HostShutdownFuture;
}

/// Renders a host failure as the bounded tool error the model sees.
pub(crate) fn host_error_result(error: &HostCallError) -> crate::tools::ToolOutput {
    crate::tools::bounded_result(error.to_string(), true)
}

pub mod embedded;
pub use embedded::{
    DEFAULT_EMBEDDED_CALL_TIMEOUT, DEFAULT_EMBEDDED_MAX_CONCURRENT_CALLS, EmbeddedHostError,
    EmbeddedToolFuture, EmbeddedToolHandler, EmbeddedToolHost, EmbeddedToolHostBuilder,
    MAX_EMBEDDED_ARGUMENT_BYTES, MAX_EMBEDDED_RESULT_BYTES,
};

#[doc(hidden)]
pub mod conformance;
