//! Shared MCP client management for configuration-declared servers.
//!
//! One [`McpManager`] holds one client connection per configured MCP server
//! for the whole process. Connections start lazily on first use (or eagerly
//! at construction when a server sets its `eager` flag), tool schemas are
//! fetched once and cached (refreshed on `list_changed` notifications), and
//! each server carries a small concurrency bound so a slow server
//! backpressures instead of queueing unboundedly. Calls to distinct servers
//! proceed in parallel.
//!
//! Every failure — connect error, timeout, cancellation, server error — is
//! reported as an [`McpCallOutcome`] with `is_error` set, never as a crash of
//! the shared client: callers turn outcomes into tool errors for the model.
//!
//! Each listing is reduced to an [`McpToolSetDigest`]. A server declared with
//! a `pin` whose live listing digests differently is quarantined (ADR-0042):
//! its tools leave the catalog and every call on it fails closed with
//! [`McpCallFailure::Quarantined`] until the listing matches the pin again.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    pin::Pin,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use qq_provider::ToolSpec;
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ClientInfo, ContentBlock},
    service::{NotificationContext, RoleClient, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};

/// Namespace prefix carried by every declared MCP tool: `mcp__<server>__<tool>`.
pub const MCP_TOOL_PREFIX: &str = "mcp__";
/// Default per-call deadline covering queueing, (re)connection, and the call.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Default per-server bound on concurrently executing calls.
pub const DEFAULT_MAX_CONCURRENT_CALLS: usize = 4;

const MAX_SERVER_NAME_BYTES: usize = 64;
/// Deadline for establishing a connection and completing MCP initialization.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Deadline for one `tools/list` fetch on an established connection.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(20);
/// A failed connection is not retried until this much time has passed; a use
/// arriving earlier waits out the remainder, then retries.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
/// How often an in-flight call re-checks its cancellation flag.
/// A future that resolves when the caller's run is cancelled. The manager
/// awaits it alongside the call so cancellation is observed as soon as it
/// happens; the caller (`qq-core`'s host adapter) builds it from its own
/// token, which keeps this crate free of that type.
pub type CancellationSignal = Pin<Box<dyn Future<Output = ()> + Send>>;
/// Provider APIs bound tool names; namespaced names above this are skipped.
const MAX_NAMESPACED_NAME_BYTES: usize = 128;
/// Environment variables always passed through to stdio server processes so
/// the configured command resolves and behaves like a normal child process.
const DEFAULT_ENV_PASSTHROUGH: [&str; 2] = ["PATH", "HOME"];

/// One configured MCP server.
#[derive(Debug, Clone)]
pub struct McpServerSettings {
    /// Unique server name; becomes the middle segment of `mcp__<name>__<tool>`.
    pub name: String,
    pub transport: McpTransportSettings,
    /// Connect at manager construction instead of on first use.
    pub eager: bool,
    /// Bare (un-namespaced) tool names granted by configuration.
    pub allow: Vec<String>,
    pub call_timeout: Duration,
    pub max_concurrent_calls: usize,
    /// The tool-set digest the live listing must reproduce. `None` accepts
    /// whatever the server declares; `Some` quarantines the server while its
    /// listing digests differently.
    pub pin: Option<McpToolSetDigest>,
}

impl McpServerSettings {
    #[must_use]
    pub fn new(name: impl Into<String>, transport: McpTransportSettings) -> Self {
        Self {
            name: name.into(),
            transport,
            eager: false,
            allow: Vec::new(),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            max_concurrent_calls: DEFAULT_MAX_CONCURRENT_CALLS,
            pin: None,
        }
    }
}

/// Domain separator for [`McpToolSetDigest`]; bump with any change to what
/// the digest covers so old pins fail loudly instead of matching by accident.
const TOOL_SET_DIGEST_DOMAIN: &[u8] = b"qq-mcp-tool-set-v1\0";

/// SHA-256 over one server's normalized tool set: the namespaced tool name
/// (which carries the server name), description, compact sorted-key input
/// schema, and hints of every listed tool, in name order. Listing order does
/// not matter; any change to a name, description, schema, or hint does.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct McpToolSetDigest([u8; 32]);

impl McpToolSetDigest {
    /// The digest of `tools` as one server listed them.
    #[must_use]
    pub fn of_tools(tools: &[McpTool]) -> Self {
        let mut sorted = tools.iter().collect::<Vec<_>>();
        sorted.sort_by(|left, right| left.spec.name().cmp(right.spec.name()));
        let mut hasher = Sha256::new();
        hasher.update(TOOL_SET_DIGEST_DOMAIN);
        for tool in sorted {
            hasher.update(tool.spec.name().as_bytes());
            hasher.update(b"\0");
            hasher.update(tool.spec.description().as_bytes());
            hasher.update(b"\0");
            hasher.update(tool.spec.input_schema().get().as_bytes());
            hasher.update(b"\0");
            hasher.update([u8::from(tool.hints.read_only)
                | u8::from(tool.hints.destructive) << 1
                | u8::from(tool.hints.idempotent) << 2
                | u8::from(tool.hints.open_world) << 3]);
            hasher.update(b"\n");
        }
        Self(hasher.finalize().into())
    }
}

impl fmt::Display for McpToolSetDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for McpToolSetDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "McpToolSetDigest({self})")
    }
}

/// The text of a pin was not a tool-set digest.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("an MCP tool-set digest is 64 lowercase hex digits")]
pub struct McpToolSetDigestParseError;

impl FromStr for McpToolSetDigest {
    type Err = McpToolSetDigestParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return Err(McpToolSetDigestParseError);
        }
        let mut digest = [0_u8; 32];
        for (slot, pair) in digest.iter_mut().zip(bytes.chunks_exact(2)) {
            let nibble = |byte: u8| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(McpToolSetDigestParseError),
            };
            *slot = nibble(pair[0])? << 4 | nibble(pair[1])?;
        }
        Ok(Self(digest))
    }
}

/// How a configured server is reached.
#[derive(Debug, Clone)]
pub enum McpTransportSettings {
    /// Spawn `command args...` and speak MCP over its stdio. The child starts
    /// from a cleared environment plus `PATH`/`HOME` and the listed
    /// passthrough variables copied from this process.
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<String>,
    },
    /// Streamable-HTTP endpoint with an optional bearer token.
    Http { url: String, bearer: McpBearer },
}

/// The bearer an HTTP server is reached with. The composition root resolves
/// configured secrets; this crate only carries the outcome.
#[derive(Debug, Clone)]
pub enum McpBearer {
    None,
    Token(String),
    /// The declared credential could not be resolved on this machine; the
    /// server stays declared (grants, tool names) but never connects. The
    /// message is shown to the user as the unavailability reason.
    Unavailable {
        reason: String,
    },
}

/// The outcome of one MCP tool call; failures are `is_error` outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutcome {
    pub content: String,
    pub is_error: bool,
    /// Which failure class produced an `is_error` outcome that the server
    /// did not itself report. `None` for successes and for server-reported
    /// tool errors.
    pub failure: Option<McpCallFailure>,
}

/// Why a call failed before or instead of the server answering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCallFailure {
    Timeout,
    Cancelled,
    Unavailable,
    InvalidArguments,
    UnknownTool,
    ShutDown,
    /// The server is pinned and its live listing digests differently; no
    /// tool on it runs until the pin matches again.
    Quarantined,
}

impl McpCallOutcome {
    fn error(content: impl Into<String>, failure: McpCallFailure) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            failure: Some(failure),
        }
    }
}

/// Advisory annotations one server attached to a tool. Hints only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpToolHints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

/// One namespaced declaration with its server's hints.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub spec: ToolSpec,
    pub hints: McpToolHints,
}

/// Every configured server's declarations at one instant. `generation`
/// changes whenever any server's cached tool set changes (connect, loss,
/// `list_changed`), so a holder can ask whether its snapshot is stale
/// without refetching. `servers` carries the current digest of every server
/// that answered, pinned or not, so an operator can copy it into a `pin`.
/// `unavailable` names servers that contributed nothing because they could
/// not be reached, each with the reason; `quarantined` names pinned servers
/// whose listing did not match, whose tools are likewise absent from `tools`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCatalog {
    pub generation: u64,
    pub tools: Vec<McpTool>,
    pub servers: Vec<McpServerListing>,
    pub unavailable: Vec<McpUnavailable>,
    pub quarantined: Vec<McpQuarantine>,
}

/// One server that answered `tools/list`, and what its listing digests to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerListing {
    pub server: String,
    pub digest: McpToolSetDigest,
}

/// One server that contributed no tools to a catalog, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUnavailable {
    pub server: String,
    pub reason: String,
}

/// One pinned server whose live listing does not reproduce its pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpQuarantine {
    pub server: String,
    /// The configured pin.
    pub expected: McpToolSetDigest,
    /// What the server's current listing digests to.
    pub actual: McpToolSetDigest,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum McpConfigError {
    #[error(
        "MCP server name {0:?} is invalid; use 1-{MAX_SERVER_NAME_BYTES} ASCII letters, digits, hyphens, or single underscores (no `__`)"
    )]
    InvalidServerName(String),
    #[error("MCP server {0:?} is declared more than once")]
    DuplicateServerName(String),
    #[error("MCP server {0:?} has an empty command")]
    EmptyCommand(String),
    #[error("MCP server {0:?} has an invalid URL; it must start with http:// or https://")]
    InvalidUrl(String),
    #[error("MCP server {0:?} has a zero call timeout")]
    ZeroCallTimeout(String),
    #[error("MCP server {0:?} has a zero concurrency bound")]
    ZeroConcurrencyBound(String),
    #[error("MCP server {0:?} allowlists an empty tool name")]
    EmptyAllowedTool(String),
}

/// Returns whether `name` keeps the `mcp__<server>__<tool>` grammar
/// unambiguous: non-empty ASCII letters, digits, `-`, or `_`, without any
/// `__` sequence.
#[must_use]
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVER_NAME_BYTES
        && !name.contains("__")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Client handler shared by every connection: it records `list_changed`
/// notifications so the next schema use refreshes the cache, and advances
/// the catalog generation so plans compiled from the old listing recompile.
#[derive(Clone)]
struct QqClientHandler {
    tools_dirty: Arc<AtomicBool>,
    catalog_generation: Arc<AtomicU64>,
}

impl ClientHandler for QqClientHandler {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.tools_dirty.store(true, Ordering::Release);
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info.name = "qq".to_owned();
        info.client_info.version = env!("CARGO_PKG_VERSION").to_owned();
        info
    }
}

type Client = RunningService<RoleClient, QqClientHandler>;

#[cfg(test)]
type TestConnectFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Client, String>> + Send + 'static>>;
#[cfg(test)]
type TestConnector = Arc<dyn Fn(QqClientHandler) -> TestConnectFuture + Send + Sync>;

struct ServerHandle {
    settings: McpServerSettings,
    permits: Arc<Semaphore>,
    state: Mutex<ServerState>,
    tools_dirty: Arc<AtomicBool>,
    /// Advances whenever the cached tool set changes or a `list_changed`
    /// notification arrives. Read without the state lock by
    /// [`McpManager::catalog_is_current`].
    catalog_generation: Arc<AtomicU64>,
    shut_down: AtomicBool,
    #[cfg(test)]
    connector: Option<TestConnector>,
}

#[derive(Default)]
struct ServerState {
    client: Option<Arc<Client>>,
    listing: Option<Arc<Listing>>,
    last_failure: Option<Instant>,
}

/// One server's namespaced declarations and their digest, cached together
/// so a pin is re-checked against exactly the tools that were listed.
struct Listing {
    tools: Vec<McpTool>,
    digest: McpToolSetDigest,
}

/// Why a server's listing is not usable right now.
enum ListingFault {
    /// The server could not be reached or did not answer `tools/list`.
    Unavailable(String),
    /// The server answered, but its listing does not reproduce the pin.
    Quarantined {
        expected: McpToolSetDigest,
        actual: McpToolSetDigest,
    },
}

impl ServerHandle {
    fn new(settings: McpServerSettings) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(settings.max_concurrent_calls)),
            settings,
            state: Mutex::new(ServerState::default()),
            tools_dirty: Arc::new(AtomicBool::new(false)),
            catalog_generation: Arc::new(AtomicU64::new(0)),
            shut_down: AtomicBool::new(false),
            #[cfg(test)]
            connector: None,
        }
    }

    fn bump_generation(&self) {
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Returns the shared client, connecting first when necessary. A recent
    /// failure waits out the remaining backoff before one reconnect attempt.
    /// The state lock is held across the attempt so concurrent uses cannot
    /// stampede the server with parallel handshakes.
    async fn client(&self) -> Result<Arc<Client>, String> {
        let mut state = self.state.lock().await;
        self.client_locked(&mut state).await
    }

    async fn client_locked(&self, state: &mut ServerState) -> Result<Arc<Client>, String> {
        if self.shut_down.load(Ordering::Acquire) {
            return Err(format!(
                "MCP server {:?} has been shut down",
                self.settings.name
            ));
        }
        if let Some(client) = &state.client {
            return Ok(Arc::clone(client));
        }
        if let Some(failed_at) = state.last_failure {
            let elapsed = failed_at.elapsed();
            if elapsed < RECONNECT_BACKOFF {
                tokio::time::sleep(RECONNECT_BACKOFF - elapsed).await;
            }
        }
        let handler = QqClientHandler {
            tools_dirty: Arc::clone(&self.tools_dirty),
            catalog_generation: Arc::clone(&self.catalog_generation),
        };
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, self.connect(handler)).await;
        match connected {
            Ok(Ok(client)) => {
                let client = Arc::new(client);
                state.client = Some(Arc::clone(&client));
                if state.listing.take().is_some() {
                    self.bump_generation();
                }
                state.last_failure = None;
                Ok(client)
            }
            Ok(Err(error)) => {
                state.last_failure = Some(Instant::now());
                Err(error)
            }
            Err(_) => {
                state.last_failure = Some(Instant::now());
                Err(format!(
                    "connecting to MCP server {:?} timed out after {} s",
                    self.settings.name,
                    CONNECT_TIMEOUT.as_secs()
                ))
            }
        }
    }

    async fn connect(&self, handler: QqClientHandler) -> Result<Client, String> {
        #[cfg(test)]
        if let Some(connector) = &self.connector {
            return connector(handler).await;
        }
        match &self.settings.transport {
            McpTransportSettings::Stdio { command, args, env } => {
                let mut spawn = tokio::process::Command::new(command);
                spawn.args(args);
                spawn.env_clear();
                for name in DEFAULT_ENV_PASSTHROUGH
                    .into_iter()
                    .chain(env.iter().map(String::as_str))
                {
                    if let Ok(value) = std::env::var(name) {
                        spawn.env(name, value);
                    }
                }
                let (transport, _stderr) = TokioChildProcess::builder(spawn)
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|error| {
                        format!(
                            "could not start MCP server {:?}: {error}",
                            self.settings.name
                        )
                    })?;
                handler.serve(transport).await.map_err(|error| {
                    format!(
                        "MCP server {:?} failed to initialize: {error}",
                        self.settings.name
                    )
                })
            }
            McpTransportSettings::Http { url, bearer } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                match bearer {
                    McpBearer::None => {}
                    McpBearer::Token(token) => config = config.auth_header(token.clone()),
                    // Nothing to send: the connection is never attempted and
                    // the reason is the failure text callers see. The
                    // resulting `last_failure` only delays the next probe by
                    // the ordinary backoff.
                    McpBearer::Unavailable { reason } => return Err(reason.clone()),
                }
                let transport =
                    StreamableHttpClientTransport::with_client(reqwest::Client::default(), config);
                handler.serve(transport).await.map_err(|error| {
                    format!(
                        "MCP server {:?} failed to initialize: {error}",
                        self.settings.name
                    )
                })
            }
        }
    }

    /// Marks the shared client dead so the next use reconnects after backoff.
    /// Only resets when `client` is still the current one, so a call failing
    /// on a stale client cannot tear down a newer healthy connection.
    async fn invalidate(&self, client: &Arc<Client>) {
        let mut state = self.state.lock().await;
        if state
            .client
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client))
        {
            state.client = None;
            if state.listing.take().is_some() {
                self.bump_generation();
            }
            state.last_failure = Some(Instant::now());
        }
    }

    /// The cached listing checked against the pin: connects and fetches on
    /// first use, refetches after a `list_changed` notification, and
    /// compares the digest on every use (a `Copy` compare; the digest is
    /// computed once per fetch). `Unavailable` names the reason and the next
    /// use retries; `Quarantined` keeps the listing cached so the fault is
    /// reported, not refetched, until the server or the pin changes.
    async fn listing(&self) -> Result<Arc<Listing>, ListingFault> {
        let mut state = self.state.lock().await;
        if self.tools_dirty.swap(false, Ordering::AcqRel) {
            state.listing = None;
        }
        let listing = match &state.listing {
            Some(listing) => Arc::clone(listing),
            None => {
                let client = match self.client_locked(&mut state).await {
                    Ok(client) => client,
                    Err(reason) => return Err(ListingFault::Unavailable(reason)),
                };
                match tokio::time::timeout(LIST_TOOLS_TIMEOUT, client.list_all_tools()).await {
                    Ok(Ok(tools)) => {
                        let tools = self.namespaced_tools(tools);
                        let listing = Arc::new(Listing {
                            digest: McpToolSetDigest::of_tools(&tools),
                            tools,
                        });
                        state.listing = Some(Arc::clone(&listing));
                        self.bump_generation();
                        listing
                    }
                    Ok(Err(error)) => {
                        state.client = None;
                        state.listing = None;
                        state.last_failure = Some(Instant::now());
                        return Err(ListingFault::Unavailable(format!(
                            "listing tools on MCP server {:?} failed: {error}",
                            self.settings.name
                        )));
                    }
                    Err(_) => {
                        state.client = None;
                        state.listing = None;
                        state.last_failure = Some(Instant::now());
                        return Err(ListingFault::Unavailable(format!(
                            "listing tools on MCP server {:?} timed out after {} s",
                            self.settings.name,
                            LIST_TOOLS_TIMEOUT.as_secs()
                        )));
                    }
                }
            }
        };
        match self.settings.pin {
            Some(expected) if expected != listing.digest => Err(ListingFault::Quarantined {
                expected,
                actual: listing.digest,
            }),
            Some(_) | None => Ok(listing),
        }
    }

    fn namespaced_tools(&self, tools: Vec<rmcp::model::Tool>) -> Vec<McpTool> {
        let mut namespaced = Vec::<McpTool>::with_capacity(tools.len());
        for tool in tools {
            let name = format!("{MCP_TOOL_PREFIX}{}__{}", self.settings.name, tool.name);
            // Names the provider layer would reject, and duplicates within
            // one server, are skipped rather than failing the whole listing.
            if name.len() > MAX_NAMESPACED_NAME_BYTES
                || tool.name.is_empty()
                || namespaced
                    .iter()
                    .any(|existing| existing.spec.name() == name)
            {
                continue;
            }
            let description = tool
                .description
                .as_deref()
                .map_or_else(String::new, str::to_owned);
            let schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());
            let hints =
                tool.annotations
                    .as_ref()
                    .map_or_else(McpToolHints::default, |annotations| McpToolHints {
                        read_only: annotations.read_only_hint.unwrap_or(false),
                        destructive: annotations.destructive_hint.unwrap_or(false),
                        idempotent: annotations.idempotent_hint.unwrap_or(false),
                        open_world: annotations.open_world_hint.unwrap_or(false),
                    });
            namespaced.push(McpTool {
                spec: ToolSpec::new(name, description, schema),
                hints,
            });
        }
        namespaced
    }

    /// Executes one call under this server's concurrency bound and deadline.
    async fn call(
        &self,
        tool: &str,
        arguments: &str,
        cancelled: CancellationSignal,
    ) -> McpCallOutcome {
        if self.shut_down.load(Ordering::Acquire) {
            return McpCallOutcome::error(
                "the MCP manager has been shut down",
                McpCallFailure::ShutDown,
            );
        }
        let arguments = match parse_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return McpCallOutcome::error(error, McpCallFailure::InvalidArguments),
        };
        let deadline = tokio::time::Instant::now() + self.settings.call_timeout;
        let execution = self.execute(tool, arguments);
        tokio::select! {
            biased;
            outcome = execution => outcome,
            // Dropping the in-flight request future is safe for the shared
            // client: rmcp requests are independent, so a timed out or
            // cancelled call never wedges other users.
            () = cancelled => McpCallOutcome::error(
                "tool execution was cancelled",
                McpCallFailure::Cancelled,
            ),
            () = tokio::time::sleep_until(deadline) => McpCallOutcome::error(
                format!(
                    "MCP call timed out after {} s",
                    self.settings.call_timeout.as_secs()
                ),
                McpCallFailure::Timeout,
            ),
        }
    }

    async fn execute(
        &self,
        tool: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> McpCallOutcome {
        if self.shut_down.load(Ordering::Acquire) {
            return McpCallOutcome::error(
                "the MCP manager has been shut down",
                McpCallFailure::ShutDown,
            );
        }
        // A pinned server is re-verified before every call so a drift
        // announced by `list_changed` (or a listing never fetched by a
        // catalog) quarantines the server before this call reaches it. The
        // check reads the cached listing unless a notification dirtied it.
        if self.settings.pin.is_some() {
            match self.listing().await {
                Ok(_) => {}
                Err(ListingFault::Unavailable(reason)) => {
                    return McpCallOutcome::error(reason, McpCallFailure::Unavailable);
                }
                Err(ListingFault::Quarantined { expected, actual }) => {
                    return McpCallOutcome::error(
                        format!(
                            "MCP server {:?} is quarantined: its tool set digests to {actual} \
                             but the configured pin is {expected}",
                            self.settings.name
                        ),
                        McpCallFailure::Quarantined,
                    );
                }
            }
        }
        // Connect before taking a call permit. The permit bounds concurrent
        // in-flight calls on one server; it must not be held across the
        // state mutex, reconnect backoff, or the connect timeout, or a single
        // slow connect would stall every caller behind it.
        let client = match self.client().await {
            Ok(client) => client,
            Err(error) => return McpCallOutcome::error(error, McpCallFailure::Unavailable),
        };
        let Ok(_permit) = self.permits.acquire().await else {
            return McpCallOutcome::error(
                "MCP server executor is unavailable",
                McpCallFailure::Unavailable,
            );
        };
        if self.shut_down.load(Ordering::Acquire) {
            return McpCallOutcome::error(
                "the MCP manager has been shut down",
                McpCallFailure::ShutDown,
            );
        }
        let mut params = CallToolRequestParams::new(tool.to_owned());
        params.arguments = arguments;
        match client.call_tool(params).await {
            Ok(result) => render_result(result),
            // A JSON-RPC error is the server answering; keep the connection.
            Err(ServiceError::McpError(error)) => McpCallOutcome {
                content: format!("MCP server returned an error: {error}"),
                is_error: true,
                failure: None,
            },
            Err(error) => {
                self.invalidate(&client).await;
                McpCallOutcome::error(
                    format!("MCP call failed: {error}"),
                    McpCallFailure::Unavailable,
                )
            }
        }
    }
}

fn parse_arguments(
    arguments: &str,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    if arguments.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(serde_json::Value::Object(map)) if map.is_empty() => Ok(None),
        Ok(serde_json::Value::Object(map)) => Ok(Some(map)),
        Ok(serde_json::Value::Null) => Ok(None),
        Ok(_) => Err("tool arguments must be a JSON object".to_owned()),
        Err(error) => Err(format!("tool arguments were not valid JSON: {error}")),
    }
}

fn render_result(result: CallToolResult) -> McpCallOutcome {
    let mut content = String::new();
    for block in &result.content {
        let rendered = match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(_) => "[image content omitted]".to_owned(),
            ContentBlock::Audio(_) => "[audio content omitted]".to_owned(),
            ContentBlock::Resource(resource) => {
                serde_json::to_string(resource).unwrap_or_else(|_| "[resource]".to_owned())
            }
            ContentBlock::ResourceLink(link) => {
                serde_json::to_string(link).unwrap_or_else(|_| "[resource link]".to_owned())
            }
            _ => "[unsupported content omitted]".to_owned(),
        };
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&rendered);
    }
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content = serde_json::to_string(structured).unwrap_or_default();
    }
    McpCallOutcome {
        content,
        is_error: result.is_error.unwrap_or(false),
        failure: None,
    }
}

/// One shared client per configured MCP server for the whole process.
pub struct McpManager {
    servers: BTreeMap<String, Arc<ServerHandle>>,
    /// Namespaced `mcp__<server>__<tool>` names granted by configuration.
    grants: Vec<String>,
}

impl McpManager {
    /// Validates the declarations and, for servers marked eager, starts
    /// connecting in the background when a Tokio runtime is available.
    pub fn new(settings: Vec<McpServerSettings>) -> Result<Self, McpConfigError> {
        let mut servers = BTreeMap::new();
        let mut grants = Vec::new();
        for server in settings {
            if !valid_server_name(&server.name) {
                return Err(McpConfigError::InvalidServerName(server.name));
            }
            match &server.transport {
                McpTransportSettings::Stdio { command, .. } if command.trim().is_empty() => {
                    return Err(McpConfigError::EmptyCommand(server.name));
                }
                McpTransportSettings::Http { url, .. } if !valid_http_url(url) => {
                    return Err(McpConfigError::InvalidUrl(server.name));
                }
                McpTransportSettings::Stdio { .. } | McpTransportSettings::Http { .. } => {}
            }
            if server.call_timeout.is_zero() {
                return Err(McpConfigError::ZeroCallTimeout(server.name));
            }
            if server.max_concurrent_calls == 0 {
                return Err(McpConfigError::ZeroConcurrencyBound(server.name));
            }
            if server.allow.iter().any(|tool| tool.trim().is_empty()) {
                return Err(McpConfigError::EmptyAllowedTool(server.name));
            }
            let name = server.name.clone();
            for tool in &server.allow {
                grants.push(format!("{MCP_TOOL_PREFIX}{name}__{tool}"));
            }
            if servers
                .insert(name.clone(), Arc::new(ServerHandle::new(server)))
                .is_some()
            {
                return Err(McpConfigError::DuplicateServerName(name));
            }
        }
        let manager = Self { servers, grants };
        manager.spawn_eager_connects();
        Ok(manager)
    }

    fn spawn_eager_connects(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for handle in self.servers.values() {
            if handle.settings.eager {
                let handle = Arc::clone(handle);
                runtime.spawn(async move {
                    // The listing is cached on the handle; the eager path
                    // only wants the connection warm.
                    let _warm = handle.listing().await;
                });
            }
        }
    }

    /// Cached namespaced declarations for every configured server, connecting
    /// lazily on first use. Servers are queried in parallel; an unavailable
    /// or quarantined server contributes nothing rather than failing the
    /// batch, and is named in the matching list instead.
    pub async fn catalog(&self) -> McpCatalog {
        let fetches = self
            .servers
            .iter()
            .map(|(name, handle)| async move { (name, handle.listing().await) })
            .collect::<Vec<_>>();
        let results = futures_util::future::join_all(fetches).await;
        let mut tools = Vec::new();
        let mut servers = Vec::new();
        let mut unavailable = Vec::new();
        let mut quarantined = Vec::new();
        for (name, result) in results {
            match result {
                Ok(listing) => {
                    tools.extend(listing.tools.iter().cloned());
                    servers.push(McpServerListing {
                        server: name.clone(),
                        digest: listing.digest,
                    });
                }
                Err(ListingFault::Unavailable(reason)) => unavailable.push(McpUnavailable {
                    server: name.clone(),
                    reason,
                }),
                Err(ListingFault::Quarantined { expected, actual }) => {
                    servers.push(McpServerListing {
                        server: name.clone(),
                        digest: actual,
                    });
                    quarantined.push(McpQuarantine {
                        server: name.clone(),
                        expected,
                        actual,
                    });
                }
            }
        }
        McpCatalog {
            // Read after the fetches: a listing that landed during them is
            // reflected in both the tools and the generation.
            generation: self.generation(),
            tools,
            servers,
            unavailable,
            quarantined,
        }
    }

    /// The cached declarations for every server, flattened.
    pub async fn tool_specs(&self) -> Vec<ToolSpec> {
        self.catalog()
            .await
            .tools
            .into_iter()
            .map(|tool| tool.spec)
            .collect()
    }

    /// The catalog generation right now. Cheap and synchronous: a sum of
    /// per-server counters that advance on connect, loss, and
    /// `list_changed`.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.servers
            .values()
            .map(|handle| handle.catalog_generation.load(Ordering::Acquire))
            .fold(0_u64, u64::wrapping_add)
    }

    /// Whether a catalog taken under `generation` still describes every
    /// server's cached tool set.
    #[must_use]
    pub fn catalog_is_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }

    /// Refuses new calls and drops every connection. In-flight calls settle
    /// through their own deadlines; no call is retried.
    pub async fn shutdown(&self) {
        for handle in self.servers.values() {
            handle.shut_down.store(true, Ordering::Release);
            let mut state = handle.state.lock().await;
            state.client = None;
            state.listing = None;
            // Always advance: a plan compiled against this manager must not
            // consider its snapshot current once the backends are gone.
            handle.bump_generation();
        }
    }

    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.servers
            .values()
            .any(|handle| handle.shut_down.load(Ordering::Acquire))
    }

    /// Exact namespaced tool names granted by configuration allowlists.
    #[must_use]
    pub fn config_grants(&self) -> Vec<String> {
        self.grants.clone()
    }

    /// Dispatches one namespaced `mcp__<server>__<tool>` call. `cancelled`
    /// resolves when the caller's run is cancelled; the call then returns
    /// [`McpCallFailure::Cancelled`] at once rather than on a poll tick.
    pub async fn call(
        &self,
        name: &str,
        arguments: &str,
        cancelled: CancellationSignal,
    ) -> McpCallOutcome {
        let Some((server, tool)) = name
            .strip_prefix(MCP_TOOL_PREFIX)
            .and_then(|rest| rest.split_once("__"))
        else {
            return McpCallOutcome::error(
                format!("unknown MCP tool {name:?}"),
                McpCallFailure::UnknownTool,
            );
        };
        let Some(handle) = self.servers.get(server) else {
            return McpCallOutcome::error(
                format!("no MCP server named {server:?} is configured"),
                McpCallFailure::UnknownTool,
            );
        };
        if tool.is_empty() {
            return McpCallOutcome::error(
                format!("unknown MCP tool {name:?}"),
                McpCallFailure::UnknownTool,
            );
        }
        handle.call(tool, arguments, cancelled).await
    }
}

fn valid_http_url(url: &str) -> bool {
    ["https://", "http://"].into_iter().any(|scheme| {
        url.len() > scheme.len()
            && url
                .get(..scheme.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
    })
}

#[cfg(test)]
mod tests;
