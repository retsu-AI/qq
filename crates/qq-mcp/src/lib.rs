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
    model::{
        CallToolRequestParams, CallToolResult, ClientInfo, ContentBlock, PaginatedRequestParams,
    },
    service::{NotificationContext, RoleClient, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};

pub const MCP_TOOL_PREFIX: &str = "mcp__";
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_CONCURRENT_CALLS: usize = 4;

const MAX_SERVER_NAME_BYTES: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(20);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
pub type CancellationSignal = Pin<Box<dyn Future<Output = ()> + Send>>;
const MAX_NAMESPACED_NAME_BYTES: usize = 128;
const MAX_LIST_PAGES: usize = 32;
const MAX_LIST_TOOLS: usize = 512;
const MAX_LIST_BYTES: usize = 1024 * 1024;
struct ByteBudget {
    remaining: usize,
}

impl std::io::Write for ByteBudget {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.remaining.checked_sub(buf.len()) {
            Some(remaining) => {
                self.remaining = remaining;
                Ok(buf.len())
            }
            None => Err(std::io::Error::other("descriptor byte budget exceeded")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const DEFAULT_ENV_PASSTHROUGH: [&str; 2] = ["PATH", "HOME"];

#[derive(Debug, Clone)]
pub struct McpServerSettings {
    pub name: String,
    pub transport: McpTransportSettings,
    pub eager: bool,
    pub allow: Vec<String>,
    pub call_timeout: Duration,
    pub max_concurrent_calls: usize,
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

const TOOL_SET_DIGEST_DOMAIN: &[u8] = b"qq-mcp-tool-set-v2\0";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct McpToolSetDigest([u8; 32]);

impl McpToolSetDigest {
    #[must_use]
    pub fn of_tools(tools: &[McpTool]) -> Self {
        let mut sorted = tools.iter().collect::<Vec<_>>();
        sorted.sort_by(|left, right| left.spec.name().cmp(right.spec.name()));
        let mut hasher = Sha256::new();
        hasher.update(TOOL_SET_DIGEST_DOMAIN);
        for tool in sorted {
            for field in [
                tool.spec.name(),
                tool.spec.description(),
                tool.spec.input_schema().get(),
            ] {
                hasher.update((field.len() as u64).to_be_bytes());
                hasher.update(field.as_bytes());
            }
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

#[derive(Debug, Clone)]
pub enum McpTransportSettings {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<String>,
    },
    Http {
        url: String,
        bearer: McpBearer,
    },
}

#[derive(Debug, Clone)]
pub enum McpBearer {
    None,
    Token(String),
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutcome {
    pub result_digest: Option<[u8; 32]>,
    pub content: String,
    pub is_error: bool,
    pub failure: Option<McpCallFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCallFailure {
    Timeout,
    Cancelled,
    Unavailable,
    InvalidArguments,
    InvalidResult,
    UnknownTool,
    ShutDown,
    Quarantined,
}

impl McpCallOutcome {
    fn error(content: impl Into<String>, failure: McpCallFailure) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            failure: Some(failure),
            result_digest: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpToolHints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub spec: ToolSpec,
    pub hints: McpToolHints,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpCatalog {
    pub generation: u64,
    pub tools: Vec<McpTool>,
    pub servers: Vec<McpServerListing>,
    pub unavailable: Vec<McpUnavailable>,
    pub quarantined: Vec<McpQuarantine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerListing {
    pub server: String,
    pub digest: McpToolSetDigest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpInspection {
    pub server: String,
    pub configured_pin: Option<McpToolSetDigest>,
    pub digest: McpToolSetDigest,
    pub tools: Vec<McpTool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUnavailable {
    pub server: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpQuarantine {
    pub server: String,
    pub expected: McpToolSetDigest,
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

#[must_use]
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVER_NAME_BYTES
        && !name.contains("__")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[derive(Clone)]
struct QqClientHandler {
    tools_dirty: Arc<AtomicBool>,
    catalog_generation: Arc<AtomicU64>,
    dispatch_gate: Arc<std::sync::Mutex<()>>,
}

impl ClientHandler for QqClientHandler {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _gate = self.dispatch_gate.lock().expect("dispatch gate poisoned");
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
    catalog_generation: Arc<AtomicU64>,
    dispatch_gate: Arc<std::sync::Mutex<()>>,
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

struct Listing {
    tools: Vec<McpTool>,
    digest: McpToolSetDigest,
    client: Arc<Client>,
    generation: u64,
}

enum ListingFault {
    Unavailable(String),
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
            dispatch_gate: Arc::new(std::sync::Mutex::new(())),
            shut_down: AtomicBool::new(false),
            #[cfg(test)]
            connector: None,
        }
    }

    fn bump_generation(&self) {
        let _gate = self.dispatch_gate.lock().expect("dispatch gate poisoned");
        self.catalog_generation.fetch_add(1, Ordering::AcqRel);
    }

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
            dispatch_gate: Arc::clone(&self.dispatch_gate),
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

    async fn listing(&self) -> Result<Arc<Listing>, ListingFault> {
        let listing = self.fetch_listing().await?;
        match self.settings.pin {
            Some(expected) if expected != listing.digest => Err(ListingFault::Quarantined {
                expected,
                actual: listing.digest,
            }),
            Some(_) | None => Ok(listing),
        }
    }

    async fn fetch_listing(&self) -> Result<Arc<Listing>, ListingFault> {
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
                let fetch = async {
                    let mut tools = Vec::new();
                    let mut cursor = None;
                    let mut budget = ByteBudget {
                        remaining: MAX_LIST_BYTES,
                    };
                    for _ in 0..MAX_LIST_PAGES {
                        let mut request = PaginatedRequestParams::default();
                        request.cursor = cursor;
                        let page = match client.list_tools(Some(request)).await {
                            Ok(page) => page,
                            Err(error) => return Err(format!("tool listing failed: {error}")),
                        };
                        if page.tools.len() > MAX_LIST_TOOLS - tools.len() {
                            return Err("tool listing exceeds the 512-tool limit".to_owned());
                        }
                        for tool in &page.tools {
                            if let Err(error) = serde_json::to_writer(&mut budget, tool) {
                                if error.is_io() {
                                    return Err("tool listing exceeds the 1 MiB descriptor limit"
                                        .to_owned());
                                }
                                return Err(format!("invalid tool descriptor: {error}"));
                            }
                        }
                        tools.extend(page.tools);
                        cursor = page.next_cursor;
                        if cursor.is_none() {
                            return self.namespaced_tools(tools);
                        }
                    }
                    Err("tool listing exceeds the 32-page limit".to_owned())
                };
                match tokio::time::timeout(LIST_TOOLS_TIMEOUT, fetch).await {
                    Ok(Ok(tools)) => {
                        let _gate = self.dispatch_gate.lock().expect("dispatch gate poisoned");
                        if self.tools_dirty.load(Ordering::Acquire) {
                            return Err(ListingFault::Unavailable(
                                "tool listing changed during discovery; retry inspection"
                                    .to_owned(),
                            ));
                        }
                        let generation = self.catalog_generation.fetch_add(1, Ordering::AcqRel) + 1;
                        let listing = Arc::new(Listing {
                            digest: McpToolSetDigest::of_tools(&tools),
                            tools,
                            client,
                            generation,
                        });
                        state.listing = Some(Arc::clone(&listing));
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
        Ok(listing)
    }

    fn namespaced_tools(&self, tools: Vec<rmcp::model::Tool>) -> Result<Vec<McpTool>, String> {
        let mut namespaced = Vec::<McpTool>::with_capacity(tools.len());
        for tool in tools {
            let name = format!("{MCP_TOOL_PREFIX}{}__{}", self.settings.name, tool.name);
            if name.len() > MAX_NAMESPACED_NAME_BYTES
                || tool.name.is_empty()
                || !tool
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
                || namespaced
                    .iter()
                    .any(|existing| existing.spec.name() == name)
            {
                return Err("tool listing contains an invalid or duplicate name".to_owned());
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
        namespaced.sort_by(|left, right| left.spec.name().cmp(right.spec.name()));
        Ok(namespaced)
    }

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
            outcome = execution => outcome,
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
        let (client, _permit, generation) = loop {
            let listing = if self.settings.pin.is_some() {
                match self.listing().await {
                    Ok(listing) => Some(listing),
                    Err(ListingFault::Unavailable(reason)) => {
                        return McpCallOutcome::error(reason, McpCallFailure::Unavailable);
                    }
                    Err(ListingFault::Quarantined { expected, actual }) => {
                        return McpCallOutcome::error(
                            format!(
                                "MCP server {:?} is quarantined: its tool set digests to {actual} but the configured pin is {expected}",
                                self.settings.name
                            ),
                            McpCallFailure::Quarantined,
                        );
                    }
                }
            } else {
                None
            };
            let client = match &listing {
                Some(listing) => Arc::clone(&listing.client),
                None => match self.client().await {
                    Ok(client) => client,
                    Err(error) => return McpCallOutcome::error(error, McpCallFailure::Unavailable),
                },
            };
            let Ok(permit) = self.permits.acquire().await else {
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
            if let Some(listing) = &listing {
                if self.tools_dirty.load(Ordering::Acquire)
                    || self.catalog_generation.load(Ordering::Acquire) != listing.generation
                {
                    drop(permit);
                    continue;
                }
                let name = format!("{MCP_TOOL_PREFIX}{}__{tool}", self.settings.name);
                if listing
                    .tools
                    .binary_search_by(|entry| entry.spec.name().cmp(&name))
                    .is_err()
                {
                    return McpCallOutcome::error(
                        "tool is absent from the pinned listing",
                        McpCallFailure::UnknownTool,
                    );
                }
            }
            break (client, permit, listing.map(|listing| listing.generation));
        };
        let mut params = CallToolRequestParams::new(tool.to_owned());
        params.arguments = arguments;
        let result = if let Some(generation) = generation {
            use rmcp::{
                model::{CallToolRequest, ClientRequest, ServerResult},
                service::PeerRequestOptions,
            };
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
            let mut send = Box::pin(
                client
                    .peer()
                    .send_request_with_option(request, PeerRequestOptions::no_options()),
            );
            let dispatched = futures_util::future::poll_fn(|context| {
                let _gate = self.dispatch_gate.lock().expect("dispatch gate poisoned");
                if self.shut_down.load(Ordering::Acquire)
                    || self.tools_dirty.load(Ordering::Acquire)
                    || self.catalog_generation.load(Ordering::Acquire) != generation
                {
                    return std::task::Poll::Ready(None);
                }
                send.as_mut().poll(context).map(Some)
            })
            .await;
            match dispatched {
                None => {
                    return McpCallOutcome::error(
                        "MCP listing changed before dispatch; retry after inspection",
                        McpCallFailure::Quarantined,
                    );
                }
                Some(Err(error)) => Err(error),
                Some(Ok(handle)) => match handle.await_response().await {
                    Ok(ServerResult::CallToolResult(result)) => Ok(result),
                    Ok(_) => {
                        return McpCallOutcome::error(
                            "pinned MCP calls do not allow automatic continuation requests",
                            McpCallFailure::InvalidResult,
                        );
                    }
                    Err(error) => Err(error),
                },
            }
        } else {
            client.call_tool(params).await
        };
        match result {
            Ok(result) => render_result(result),
            Err(ServiceError::McpError(error)) => McpCallOutcome {
                result_digest: None,
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

struct ResponseIdentity(Sha256);
impl std::io::Write for ResponseIdentity {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn render_result(result: CallToolResult) -> McpCallOutcome {
    let mut identity = ResponseIdentity(Sha256::new());
    identity.0.update(b"qq.mcp-observed-result.v1\0");
    if let Err(error) = serde_json::to_writer(
        &mut identity,
        &(
            &result.content,
            &result.structured_content,
            result.is_error.unwrap_or(false),
        ),
    ) {
        return McpCallOutcome::error(
            format!("MCP response could not be represented: {error}"),
            McpCallFailure::InvalidResult,
        );
    }
    let result_digest = Some(identity.0.finalize().into());

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
        result_digest,
    }
}

pub struct McpManager {
    servers: BTreeMap<String, Arc<ServerHandle>>,
    grants: Vec<String>,
}

impl McpManager {
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
                    let _warm = handle.listing().await;
                });
            }
        }
    }

    pub async fn inspect(&self, server: &str) -> Result<McpInspection, McpUnavailable> {
        let Some(handle) = self.servers.get(server) else {
            return Err(McpUnavailable {
                server: server.to_owned(),
                reason: "server is not configured".to_owned(),
            });
        };
        match handle.fetch_listing().await {
            Ok(listing) => Ok(McpInspection {
                server: server.to_owned(),
                configured_pin: handle.settings.pin,
                digest: listing.digest,
                tools: listing.tools.clone(),
            }),
            Err(ListingFault::Unavailable(reason)) => Err(McpUnavailable {
                server: server.to_owned(),
                reason,
            }),
            Err(ListingFault::Quarantined { .. }) => {
                unreachable!("raw listing inspection does not enforce pins")
            }
        }
    }

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
            generation: self.generation(),
            tools,
            servers,
            unavailable,
            quarantined,
        }
    }

    pub async fn tool_specs(&self) -> Vec<ToolSpec> {
        self.catalog()
            .await
            .tools
            .into_iter()
            .map(|tool| tool.spec)
            .collect()
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.servers
            .values()
            .map(|handle| handle.catalog_generation.load(Ordering::Acquire))
            .fold(0_u64, u64::wrapping_add)
    }

    #[must_use]
    pub fn catalog_is_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }

    pub async fn shutdown(&self) {
        for handle in self.servers.values() {
            {
                let _gate = handle.dispatch_gate.lock().expect("dispatch gate poisoned");
                handle.shut_down.store(true, Ordering::Release);
            }
            let mut state = handle.state.lock().await;
            state.client = None;
            state.listing = None;
            handle.bump_generation();
        }
    }

    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.servers
            .values()
            .any(|handle| handle.shut_down.load(Ordering::Acquire))
    }

    #[must_use]
    pub fn config_grants(&self) -> Vec<String> {
        self.grants.clone()
    }

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
