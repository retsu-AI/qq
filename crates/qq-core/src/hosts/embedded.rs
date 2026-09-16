//! An in-process tool host for embedding applications.
//!
//! An embedder registers typed handlers once, before compiling plans, and the
//! host serves them under the same contract as MCP: an immutable catalog
//! snapshot with a generation, a per-host concurrency bound, a per-call
//! deadline, argument and result byte bounds, typed failures, and no implicit
//! retry. Tool names are `ext__<host>__<tool>`.
//!
//! The registry is frozen at construction. A host whose tool set must change
//! is a new host with a new generation; plans compiled from the old one keep
//! it until they are replaced.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use qq_provider::ToolSpec;
use tokio::sync::Semaphore;

use super::{
    EMBEDDED_TOOL_PREFIX, ExternalToolHost, HostCallError, HostCallFuture, HostCatalog,
    HostReadiness, HostShutdownFuture, HostTool, HostToolResult, ToolHints,
};
use crate::RunCancellation;

/// Default per-call deadline.
pub const DEFAULT_EMBEDDED_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Default per-host bound on concurrently executing calls.
pub const DEFAULT_EMBEDDED_MAX_CONCURRENT_CALLS: usize = 4;
/// Largest argument document a handler receives.
pub const MAX_EMBEDDED_ARGUMENT_BYTES: usize = 64 * 1024;
/// Largest result a handler may return before it is refused as invalid; the
/// run loop truncates to its own bound after this check.
pub const MAX_EMBEDDED_RESULT_BYTES: usize = 1024 * 1024;
const MAX_HOST_NAME_BYTES: usize = 64;
const MAX_TOOL_NAME_BYTES: usize = 64;

/// What a handler produces. `Err` is the tool reporting failure to the
/// model (an ordinary `is_error` result); the host itself decides
/// [`HostCallError`]s.
pub type EmbeddedToolFuture =
    Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>>;

/// One registered handler: receives the raw JSON argument document.
pub type EmbeddedToolHandler = Arc<dyn Fn(String) -> EmbeddedToolFuture + Send + Sync>;

/// Why registration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddedHostError {
    #[error(
        "embedded host name {0:?} is invalid; use 1-{MAX_HOST_NAME_BYTES} ASCII letters, digits, hyphens, or single underscores (no `__`)"
    )]
    InvalidHostName(String),
    #[error(
        "embedded tool name {0:?} is invalid; use 1-{MAX_TOOL_NAME_BYTES} ASCII letters, digits, hyphens, or underscores"
    )]
    InvalidToolName(String),
    #[error("embedded tool {0:?} is registered more than once")]
    DuplicateToolName(String),
    #[error("embedded tool schema must be a JSON object")]
    SchemaNotAnObject(String),
    #[error("embedded host call timeout must be non-zero")]
    ZeroCallTimeout,
    #[error("embedded host concurrency bound must be non-zero")]
    ZeroConcurrencyBound,
}

struct Registered {
    spec: ToolSpec,
    hints: ToolHints,
    handler: EmbeddedToolHandler,
}

/// Builds an [`EmbeddedToolHost`]. Registration is validated eagerly so a
/// misdeclared tool fails the embedder at startup, not the model at run time.
pub struct EmbeddedToolHostBuilder {
    name: String,
    tools: BTreeMap<String, Registered>,
    grants: Vec<String>,
    call_timeout: Duration,
    max_concurrent_calls: usize,
    error: Option<EmbeddedHostError>,
}

impl EmbeddedToolHostBuilder {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let error =
            (!valid_host_name(&name)).then(|| EmbeddedHostError::InvalidHostName(name.clone()));
        Self {
            name,
            tools: BTreeMap::new(),
            grants: Vec::new(),
            call_timeout: DEFAULT_EMBEDDED_CALL_TIMEOUT,
            max_concurrent_calls: DEFAULT_EMBEDDED_MAX_CONCURRENT_CALLS,
            error,
        }
    }

    /// Registers `tool` (un-namespaced) with its schema, hints, and handler.
    #[must_use]
    pub fn tool(
        mut self,
        tool: &str,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        hints: ToolHints,
        handler: EmbeddedToolHandler,
    ) -> Self {
        if self.error.is_some() {
            return self;
        }
        if !valid_tool_name(tool) {
            self.error = Some(EmbeddedHostError::InvalidToolName(tool.to_owned()));
            return self;
        }
        if !input_schema.is_object() {
            self.error = Some(EmbeddedHostError::SchemaNotAnObject(tool.to_owned()));
            return self;
        }
        let name = format!("{EMBEDDED_TOOL_PREFIX}{}__{tool}", self.name);
        if self.tools.contains_key(&name) {
            self.error = Some(EmbeddedHostError::DuplicateToolName(tool.to_owned()));
            return self;
        }
        self.tools.insert(
            name.clone(),
            Registered {
                spec: ToolSpec::new(name, description, input_schema),
                hints,
                handler,
            },
        );
        self
    }

    /// Pre-approves `tool` (un-namespaced) for every run, like an MCP
    /// allowlist entry. Mode still wins: read-only sessions deny it.
    #[must_use]
    pub fn grant(mut self, tool: &str) -> Self {
        self.grants
            .push(format!("{EMBEDDED_TOOL_PREFIX}{}__{tool}", self.name));
        self
    }

    #[must_use]
    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    #[must_use]
    pub fn max_concurrent_calls(mut self, bound: usize) -> Self {
        self.max_concurrent_calls = bound;
        self
    }

    pub fn build(self) -> Result<Arc<EmbeddedToolHost>, EmbeddedHostError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.call_timeout.is_zero() {
            return Err(EmbeddedHostError::ZeroCallTimeout);
        }
        if self.max_concurrent_calls == 0 {
            return Err(EmbeddedHostError::ZeroConcurrencyBound);
        }
        let mut grants = self.grants;
        grants.retain(|grant| self.tools.contains_key(grant));
        grants.sort();
        grants.dedup();
        Ok(Arc::new(EmbeddedToolHost {
            name: self.name,
            tools: self.tools,
            grants,
            call_timeout: self.call_timeout,
            permits: Arc::new(Semaphore::new(self.max_concurrent_calls)),
            shut_down: AtomicBool::new(false),
        }))
    }
}

/// A frozen set of in-process tools served under the external-host contract.
pub struct EmbeddedToolHost {
    name: String,
    tools: BTreeMap<String, Registered>,
    grants: Vec<String>,
    call_timeout: Duration,
    permits: Arc<Semaphore>,
    shut_down: AtomicBool,
}

impl EmbeddedToolHost {
    #[must_use]
    pub fn builder(name: impl Into<String>) -> EmbeddedToolHostBuilder {
        EmbeddedToolHostBuilder::new(name)
    }

    /// The registry never changes, so its generation is constant.
    const GENERATION: u64 = 1;
}

impl ExternalToolHost for EmbeddedToolHost {
    fn name(&self) -> &str {
        &self.name
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: Self::GENERATION,
            tools: self
                .tools
                .values()
                .map(|registered| HostTool {
                    spec: registered.spec.clone(),
                    hints: registered.hints,
                })
                .collect(),
            readiness: self.readiness(),
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        generation == Self::GENERATION && !self.shut_down.load(Ordering::Acquire)
    }

    fn config_grants(&self) -> Vec<String> {
        self.grants.clone()
    }

    fn call(&self, name: String, arguments: String, cancelled: RunCancellation) -> HostCallFuture {
        if self.shut_down.load(Ordering::Acquire) {
            return Box::pin(std::future::ready(Err(HostCallError::ShutDown)));
        }
        let Some(registered) = self.tools.get(&name) else {
            return Box::pin(std::future::ready(Err(HostCallError::UnknownTool(name))));
        };
        if arguments.len() > MAX_EMBEDDED_ARGUMENT_BYTES {
            return Box::pin(std::future::ready(Err(HostCallError::Refused(format!(
                "arguments exceed the {MAX_EMBEDDED_ARGUMENT_BYTES}-byte limit"
            )))));
        }
        // The permit is taken without waiting: a saturated host is reported
        // as overloaded so the model can back off, rather than queueing calls
        // behind one another inside a turn's deadline. The owned permit
        // travels with the future and releases when it settles or is dropped.
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return Box::pin(std::future::ready(Err(HostCallError::Overloaded)));
        };
        let handler = Arc::clone(&registered.handler);
        let deadline = tokio::time::Instant::now() + self.call_timeout;
        Box::pin(async move {
            let _permit = permit;
            tokio::select! {
                biased;
                outcome = handler(arguments) => match outcome {
                    Ok(content) if content.len() > MAX_EMBEDDED_RESULT_BYTES => {
                        Err(HostCallError::InvalidResult(format!(
                            "result exceeds the {MAX_EMBEDDED_RESULT_BYTES}-byte limit"
                        )))
                    }
                    Ok(content) => Ok(HostToolResult { content, is_error: false }),
                    Err(content) => Ok(HostToolResult { content, is_error: true }),
                },
                () = cancelled.cancelled() => Err(HostCallError::Cancelled),
                () = tokio::time::sleep_until(deadline) => Err(HostCallError::Timeout),
            }
        })
    }

    fn readiness(&self) -> HostReadiness {
        if self.shut_down.load(Ordering::Acquire) {
            HostReadiness::ShutDown
        } else {
            HostReadiness::Ready
        }
    }

    fn shutdown(&self) -> HostShutdownFuture {
        self.shut_down.store(true, Ordering::Release);
        Box::pin(std::future::ready(()))
    }
}

fn valid_host_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HOST_NAME_BYTES
        && !name.contains("__")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::hosts::conformance::{ConformanceFixture, check};

    fn handler<F>(f: F) -> EmbeddedToolHandler
    where
        F: Fn(String) -> Result<String, String> + Send + Sync + 'static,
    {
        Arc::new(move |arguments| {
            let outcome = f(arguments);
            Box::pin(std::future::ready(outcome)) as EmbeddedToolFuture
        })
    }

    fn fixture_host(bound: usize) -> Arc<EmbeddedToolHost> {
        EmbeddedToolHost::builder("app")
            .call_timeout(Duration::from_millis(80))
            .max_concurrent_calls(bound)
            .tool(
                "echo",
                "Echo the arguments",
                serde_json::json!({"type": "object"}),
                ToolHints {
                    read_only: true,
                    ..ToolHints::default()
                },
                handler(|arguments| Ok(format!("echo:{arguments}"))),
            )
            .tool(
                "fail",
                "Always fails",
                serde_json::json!({"type": "object"}),
                ToolHints::default(),
                handler(|_| Err("the tool failed".to_owned())),
            )
            .tool(
                "hang",
                "Never returns",
                serde_json::json!({"type": "object"}),
                ToolHints::default(),
                Arc::new(|_| Box::pin(std::future::pending()) as EmbeddedToolFuture),
            )
            .tool(
                "huge",
                "Returns too much",
                serde_json::json!({"type": "object"}),
                ToolHints::default(),
                handler(|_| Ok("x".repeat(MAX_EMBEDDED_RESULT_BYTES + 1))),
            )
            .grant("echo")
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn embedded_host_passes_the_shared_conformance_suite() {
        let host = fixture_host(2);
        assert_eq!(host.config_grants(), ["ext__app__echo"]);
        let catalog = host.catalog_blocking();
        assert_eq!(catalog.tools.len(), 4);
        assert!(
            catalog
                .tools
                .iter()
                .any(|tool| tool.spec.name() == "ext__app__echo" && tool.hints.read_only)
        );
        check(
            host,
            ConformanceFixture {
                succeeds: Some(("ext__app__echo".to_owned(), "echo:{}".to_owned())),
                tool_error: Some("ext__app__fail".to_owned()),
                hangs: Some("ext__app__hang".to_owned()),
                oversized: Some("ext__app__huge".to_owned()),
                unknown: "ext__app__nope".to_owned(),
                concurrency: Some(2),
                backend_unavailable: false,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn embedded_host_bounds_arguments_and_validates_registration() {
        let host = fixture_host(1);
        let big = "x".repeat(MAX_EMBEDDED_ARGUMENT_BYTES + 1);
        assert!(matches!(
            host.call("ext__app__echo".to_owned(), big, RunCancellation::new())
                .await,
            Err(HostCallError::Refused(_))
        ));

        let noop = handler(|_| Ok(String::new()));
        assert_eq!(
            EmbeddedToolHost::builder("bad__name").build().err(),
            Some(EmbeddedHostError::InvalidHostName("bad__name".to_owned()))
        );
        assert_eq!(
            EmbeddedToolHost::builder("app")
                .tool(
                    "bad name",
                    "",
                    serde_json::json!({}),
                    ToolHints::default(),
                    Arc::clone(&noop)
                )
                .build()
                .err(),
            Some(EmbeddedHostError::InvalidToolName("bad name".to_owned()))
        );
        assert_eq!(
            EmbeddedToolHost::builder("app")
                .tool(
                    "t",
                    "",
                    serde_json::json!([]),
                    ToolHints::default(),
                    Arc::clone(&noop)
                )
                .build()
                .err(),
            Some(EmbeddedHostError::SchemaNotAnObject("t".to_owned()))
        );
        assert_eq!(
            EmbeddedToolHost::builder("app")
                .tool(
                    "t",
                    "",
                    serde_json::json!({}),
                    ToolHints::default(),
                    Arc::clone(&noop)
                )
                .tool(
                    "t",
                    "",
                    serde_json::json!({}),
                    ToolHints::default(),
                    Arc::clone(&noop)
                )
                .build()
                .err(),
            Some(EmbeddedHostError::DuplicateToolName("t".to_owned()))
        );
        assert_eq!(
            EmbeddedToolHost::builder("app")
                .call_timeout(Duration::ZERO)
                .build()
                .err(),
            Some(EmbeddedHostError::ZeroCallTimeout)
        );
        assert_eq!(
            EmbeddedToolHost::builder("app")
                .max_concurrent_calls(0)
                .build()
                .err(),
            Some(EmbeddedHostError::ZeroConcurrencyBound)
        );
        // Grants for unregistered tools are dropped, not invented.
        let host = EmbeddedToolHost::builder("app")
            .grant("ghost")
            .build()
            .unwrap();
        assert!(host.config_grants().is_empty());
    }
}
