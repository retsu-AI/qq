//! Authenticated, single-instance HTTP and SSE server adapter.

#![forbid(unsafe_code)]

use std::{
    convert::Infallible,
    fmt,
    fs::{self, File, OpenOptions, TryLockError},
    future::Future,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_stream::stream;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxumPath, Request, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response, sse::Event, sse::KeepAlive, sse::Sse},
    routing::{get, post},
};
use directories::ProjectDirs;
use futures_util::StreamExt;
use qq_core::PublishedEventStream;
use qq_protocol::{
    AgentProfileSummary, ApprovalMode, BudgetLimitKind, CAPABILITIES_VERSION, CapabilitiesRequest,
    CommandReceipt, CommandRequest, DelegationCapabilities, DelegationRoster, EventCapabilities,
    InputPartKind, LimitCapabilities, LocalConnectionError, LocalServerConnection,
    MAX_CORRELATION_ENTRIES, MAX_EVENT_BYTES, MAX_INPUT_FILE_BYTES, MAX_INPUT_FILE_PARTS,
    MAX_INPUT_PARTS, MAX_INPUT_TEXT_BYTES, MAX_MODEL_BYTES, MAX_ORGANIZATION_BYTES,
    MAX_REQUEST_BYTES, MAX_WORKSPACE_BYTES, ModelCatalogRequest, ModelDescriptor, PROTOCOL_VERSION,
    ServerCapabilities, ServerInfo, SessionCommand, SessionCommandKind, SnapshotRequest,
    SteeringCapabilities, SubscribeRequest, ToolCapabilities, WorkspaceId, WorkspaceSnapshot,
    WorkspaceToolCapabilities, validate_input,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};

const METADATA_FORMAT_VERSION: u16 = 1;
const METADATA_FILE_NAME: &str = "server.ron";
const LOCK_FILE_NAME: &str = "server.lock";
const DEFAULT_BIND_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
const MAX_METADATA_BYTES: usize = 16 * 1024;
pub(crate) const MAX_HEALTH_BYTES: usize = 16 * 1024;
const MAX_MODEL_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const STARTUP_RETRIES: usize = 8;
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(25);
const DISCOVERY_RETRIES: usize = 3;
const DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(50);
const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_BYTES: usize = TOKEN_BYTES * 2;
const MAX_CONCURRENT_SESSION_REQUESTS: usize = 64;
const MAX_CONCURRENT_SUBSCRIPTIONS: usize = 64;
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

pub type CommandFuture =
    Pin<Box<dyn Future<Output = Result<CommandReceipt, ServerHandlerError>> + Send + 'static>>;
pub type SnapshotFuture =
    Pin<Box<dyn Future<Output = Result<WorkspaceSnapshot, ServerHandlerError>> + Send + 'static>>;
pub type ModelsFuture = Pin<
    Box<dyn Future<Output = Result<Vec<ModelDescriptor>, ServerHandlerError>> + Send + 'static>,
>;
pub type ProfilesFuture = Pin<
    Box<dyn Future<Output = Result<Vec<AgentProfileSummary>, ServerHandlerError>> + Send + 'static>,
>;
pub type WorkspaceToolsFuture = Pin<
    Box<
        dyn Future<Output = Result<WorkspaceToolCapabilities, ServerHandlerError>> + Send + 'static,
    >,
>;
pub type DelegationFuture =
    Pin<Box<dyn Future<Output = Result<DelegationRoster, ServerHandlerError>> + Send + 'static>>;

/// Root-supplied application seam for durable session requests.
pub trait ServerHandler: Send + Sync + 'static {
    fn command(&self, _request: CommandRequest) -> CommandFuture {
        Box::pin(async { Err(ServerHandlerError::Unavailable) })
    }

    fn snapshot(&self, _request: SnapshotRequest) -> SnapshotFuture {
        Box::pin(async { Err(ServerHandlerError::Unavailable) })
    }

    fn models(&self, _request: ModelCatalogRequest) -> ModelsFuture {
        Box::pin(async { Err(ServerHandlerError::Unavailable) })
    }

    /// The agent profiles a workspace's configuration declares, for the
    /// capability document. Everything else in that document is owned by the
    /// transport and the protocol crate.
    fn profiles(&self, _workspace_id: WorkspaceId) -> ProfilesFuture {
        Box::pin(async { Err(ServerHandlerError::Unavailable) })
    }

    /// The external tool hosts and skill index of a workspace's default plan,
    /// for the capability document. The default returns nothing rather than
    /// failing: a handler without plans still serves the static sections.
    fn workspace_tools(&self, _workspace_id: WorkspaceId) -> WorkspaceToolsFuture {
        Box::pin(async { Err(ServerHandlerError::Unavailable) })
    }

    /// The delegation roster and bounds a workspace's configuration declares,
    /// for the capability document.
    fn delegation(&self, _workspace_id: WorkspaceId) -> DelegationFuture {
        Box::pin(async { Err(ServerHandlerError::Unavailable) })
    }

    /// Committed events after the request cursor, each with the JSON the
    /// store persisted. The server writes that JSON to the SSE frame as-is.
    fn subscribe(
        &self,
        _request: SubscribeRequest,
    ) -> Result<PublishedEventStream, ServerHandlerError> {
        Err(ServerHandlerError::Unavailable)
    }
}

/// Sanitized failures a root handler may return before streaming starts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ServerHandlerError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("request service is unavailable")]
    Unavailable,
    #[error("request failed")]
    Internal,
}

/// Stable filesystem locations used for instance coordination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPaths {
    directory: PathBuf,
    lock_file: PathBuf,
    metadata_file: PathBuf,
}

impl ServerPaths {
    /// Uses `directory` as an injectable private state directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        Self {
            lock_file: directory.join(LOCK_FILE_NAME),
            metadata_file: directory.join(METADATA_FILE_NAME),
            directory,
        }
    }

    /// Resolves the current user's runtime directory, with a data-local fallback.
    pub fn for_user() -> Result<Self, ServerError> {
        let project =
            ProjectDirs::from("dev", "qq", "qq").ok_or(ServerError::StateDirectoryUnavailable)?;
        let directory = project
            .runtime_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project.data_local_dir().join("runtime"));
        Ok(Self::new(directory))
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn lock_file(&self) -> &Path {
        &self.lock_file
    }

    #[must_use]
    pub fn metadata_file(&self) -> &Path {
        &self.metadata_file
    }
}

/// Server startup settings.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    paths: ServerPaths,
    bind_address: SocketAddr,
}

impl ServerOptions {
    /// Creates options using the default ephemeral IPv4 loopback address.
    #[must_use]
    pub fn new(paths: ServerPaths) -> Self {
        Self {
            paths,
            bind_address: DEFAULT_BIND_ADDRESS,
        }
    }

    /// Creates options for the current user's state directory.
    pub fn for_user() -> Result<Self, ServerError> {
        Ok(Self::new(ServerPaths::for_user()?))
    }

    /// Overrides the listener address. Only loopback addresses are accepted.
    #[must_use]
    pub fn with_bind_address(mut self, bind_address: SocketAddr) -> Self {
        self.bind_address = bind_address;
        self
    }

    #[must_use]
    pub fn paths(&self) -> &ServerPaths {
        &self.paths
    }

    #[must_use]
    pub fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }
}

pub type ServerConnection = LocalServerConnection;

fn generate_bearer_token() -> Result<String, ServerError> {
    let mut random = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut random).map_err(|_| ServerError::RandomnessUnavailable)?;

    let mut encoded = String::with_capacity(TOKEN_HEX_BYTES);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in random {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataFile {
    format_version: u16,
    address: String,
    pid: u32,
    protocol_version: u16,
    version: String,
    token: String,
}

impl MetadataFile {
    fn new(connection: &ServerConnection) -> Self {
        Self {
            format_version: METADATA_FORMAT_VERSION,
            address: connection.address().to_string(),
            pid: connection.server_info().pid,
            protocol_version: connection.server_info().protocol_version,
            version: connection.server_info().version.clone(),
            token: connection.expose_bearer_token().to_owned(),
        }
    }

    fn into_connection(self) -> Result<ServerConnection, ServerError> {
        if self.format_version != METADATA_FORMAT_VERSION {
            return Err(ServerError::MetadataVersionMismatch {
                expected: METADATA_FORMAT_VERSION,
                found: self.format_version,
            });
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ServerError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: self.protocol_version,
            });
        }
        if self.pid == 0 || !valid_process_version(&self.version) {
            return Err(ServerError::MetadataCorrupt);
        }
        let address = self
            .address
            .parse::<SocketAddr>()
            .map_err(|_| ServerError::MetadataCorrupt)?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(ServerError::MetadataCorrupt);
        }

        LocalServerConnection::new(
            address,
            self.token,
            ServerInfo {
                protocol_version: self.protocol_version,
                version: self.version,
                pid: self.pid,
            },
        )
        .map_err(map_connection_error)
    }

    fn belongs_to(&self, connection: &ServerConnection) -> bool {
        self.format_version == METADATA_FORMAT_VERSION
            && self.address == connection.address().to_string()
            && self.pid == connection.server_info().pid
            && self.protocol_version == connection.server_info().protocol_version
            && self.version == connection.server_info().version
            && constant_time_eq(
                self.token.as_bytes(),
                connection.expose_bearer_token().as_bytes(),
            )
    }
}

impl fmt::Debug for MetadataFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetadataFile")
            .field("format_version", &self.format_version)
            .field("address", &self.address)
            .field("pid", &self.pid)
            .field("protocol_version", &self.protocol_version)
            .field("version", &self.version)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for MetadataFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (pid {}, protocol {}, token [REDACTED])",
            self.address, self.pid, self.protocol_version
        )
    }
}

/// Result of attempting to become the user-scoped server.
#[derive(Debug)]
pub enum StartOutcome {
    Started(ServerHandle),
    Existing(ServerConnection),
}

/// Result of claiming the local-instance lock before constructing the runtime
/// that will serve it.
#[derive(Debug)]
pub enum ReserveOutcome {
    Reserved(Box<ServerReservation>),
    Existing(ServerConnection),
}

/// Exclusive local-server ownership. Keeping reservation separate from
/// handler construction prevents a losing startup race from opening a second
/// durable runtime against the same store.
pub struct ServerReservation {
    listener: TcpListener,
    connection: ServerConnection,
    guard: InstanceGuard,
}

impl fmt::Debug for ServerReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerReservation")
            .field("connection", &self.connection)
            .finish_non_exhaustive()
    }
}

impl ServerReservation {
    /// Starts serving with the handler constructed after ownership was won.
    #[must_use]
    pub fn start(self, handler: Arc<dyn ServerHandler>) -> ServerHandle {
        let Self {
            listener,
            connection,
            mut guard,
        } = self;
        let app = router(handler, connection.clone());
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let serve_result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_receiver.await;
                })
                .await
                .map_err(|source| ServerError::Serve { source });
            let cleanup_result = guard.cleanup();
            serve_result.and(cleanup_result)
        });
        ServerHandle {
            connection,
            shutdown: Some(shutdown_sender),
            task: Some(task),
        }
    }
}

/// Owns a running server task and its graceful shutdown signal.
pub struct ServerHandle {
    connection: ServerConnection,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServerError>>>,
}

impl ServerHandle {
    #[must_use]
    pub fn connection(&self) -> &ServerConnection {
        &self.connection
    }

    /// Stops accepting new connections without waiting for active responses.
    pub fn begin_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    /// Requests graceful shutdown and waits for bounded metadata and lock cleanup.
    pub async fn shutdown(self) -> Result<(), ServerError> {
        self.shutdown_with_grace(DEFAULT_SHUTDOWN_GRACE).await
    }

    /// Requests graceful shutdown, aborting active responses after `grace`.
    pub async fn shutdown_with_grace(mut self, grace: Duration) -> Result<(), ServerError> {
        self.begin_shutdown();
        let mut task = self.task.take().ok_or(ServerError::ServerTaskStopped)?;
        match tokio::time::timeout(grace, &mut task).await {
            Ok(result) => result.map_err(|_| ServerError::ServerTaskStopped)?,
            Err(_) => {
                task.abort();
                // Await the aborted task so its instance guard is dropped and
                // releases metadata and the ownership lock before returning.
                let _ = task.await;
                Err(ServerError::ShutdownTimedOut)
            }
        }
    }

    /// Runs in the foreground until the server stops or this future is cancelled.
    pub async fn wait(mut self) -> Result<(), ServerError> {
        // Keeping the sender alive prevents the receiver from treating this as shutdown.
        let shutdown = self.shutdown.take();
        let result = self.join().await;
        drop(shutdown);
        result
    }

    async fn join(&mut self) -> Result<(), ServerError> {
        let task = self.task.take().ok_or(ServerError::ServerTaskStopped)?;
        task.await.map_err(|_| ServerError::ServerTaskStopped)?
    }
}

impl fmt::Debug for ServerHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerHandle")
            .field("connection", &self.connection)
            .field("running", &self.task.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Starts the server or returns the authenticated connection for the existing instance.
pub async fn start(
    handler: Arc<dyn ServerHandler>,
    options: ServerOptions,
) -> Result<StartOutcome, ServerError> {
    match reserve(options).await? {
        ReserveOutcome::Reserved(reservation) => {
            Ok(StartOutcome::Started(reservation.start(handler)))
        }
        ReserveOutcome::Existing(connection) => Ok(StartOutcome::Existing(connection)),
    }
}

/// Claims the local-instance lock and listener before a caller constructs its
/// durable runtime. Dropping a reservation releases the lock and removes its
/// metadata.
pub async fn reserve(options: ServerOptions) -> Result<ReserveOutcome, ServerError> {
    if !options.bind_address.ip().is_loopback() {
        return Err(ServerError::NonLoopbackBind(options.bind_address));
    }

    ensure_private_directory(&options.paths.directory)?;
    let lock = open_private_lock_file(&options.paths.lock_file)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            drop(lock);
            return find_existing_server(&options.paths)
                .await
                .map(ReserveOutcome::Existing);
        }
        Err(TryLockError::Error(source)) => {
            return Err(ServerError::StateIo {
                action: "lock",
                source,
            });
        }
    }

    let listener = TcpListener::bind(options.bind_address)
        .await
        .map_err(|source| ServerError::Bind {
            address: options.bind_address,
            source,
        })?;
    let address = listener.local_addr().map_err(|source| ServerError::Bind {
        address: options.bind_address,
        source,
    })?;
    let connection = ServerConnection::new(
        address,
        generate_bearer_token()?,
        ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
        },
    )
    .map_err(map_connection_error)?;
    let metadata = MetadataFile::new(&connection);
    write_metadata_atomically(&options.paths, &metadata)?;

    let guard = InstanceGuard {
        _lock: lock,
        paths: options.paths,
        connection: connection.clone(),
        cleaned: false,
    };
    Ok(ReserveOutcome::Reserved(Box::new(ServerReservation {
        listener,
        connection,
        guard,
    })))
}

#[derive(Clone)]
struct AppState {
    handler: Arc<dyn ServerHandler>,
    connection: ServerConnection,
    session_requests: Arc<Semaphore>,
    subscriptions: Arc<Semaphore>,
}

fn router(handler: Arc<dyn ServerHandler>, connection: ServerConnection) -> Router {
    let state = AppState {
        handler,
        connection,
        session_requests: Arc::new(Semaphore::new(MAX_CONCURRENT_SESSION_REQUESTS)),
        subscriptions: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBSCRIPTIONS)),
    };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/capabilities", post(capabilities))
        .route("/v1/workspaces/resolve", post(resolve_workspace))
        .route("/v1/workspaces/snapshot", post(workspace_snapshot))
        .route("/v1/models", post(models))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/prompts", post(submit_prompt))
        .route("/v1/sessions/approval-mode", post(set_approval_mode))
        .route("/v1/sessions/model", post(set_session_model))
        .route("/v1/sessions/profile", post(set_session_profile))
        .route("/v1/sessions/delete", post(delete_session))
        .route("/v1/sessions/prune", post(prune_sessions))
        .route("/v1/sessions/compact", post(compact_session))
        .route("/v1/sessions/compact/rollback", post(rollback_compaction))
        .route("/v1/runs/cancel", post(cancel_run))
        .route("/v1/runs/steer", post(steer_run))
        .route("/v1/tools/approvals", post(respond_tool_approval))
        .route(
            "/v1/workspaces/{workspace_id}/events",
            get(workspace_events),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(state)
}

async fn authenticate(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !authorized(request.headers(), &state.connection) {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    next.run(request).await
}

fn authorized(headers: &HeaderMap, connection: &ServerConnection) -> bool {
    let candidate = headers
        .get(AUTHORIZATION)
        .map(|value| value.as_bytes())
        .and_then(|value| value.strip_prefix(b"Bearer "))
        .unwrap_or_default();
    connection.matches_bearer_token(candidate)
}

async fn health(State(state): State<AppState>) -> Json<ServerInfo> {
    Json(state.connection.server_info().clone())
}

async fn resolve_workspace(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::ResolveWorkspace { .. })
    })
    .await
}

async fn create_session(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::CreateSession { .. })
    })
    .await
}

async fn submit_prompt(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::SubmitPrompt { .. })
    })
    .await
}

async fn cancel_run(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::CancelRun { .. })
    })
    .await
}

async fn steer_run(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::SteerRun { .. })
    })
    .await
}

async fn set_session_profile(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::SetSessionProfile { .. })
    })
    .await
}

async fn respond_tool_approval(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::RespondToolApproval { .. })
    })
    .await
}

async fn set_approval_mode(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::SetApprovalMode { .. })
    })
    .await
}

async fn set_session_model(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::SetSessionModel { .. })
    })
    .await
}

async fn delete_session(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::DeleteSession { .. })
    })
    .await
}

async fn prune_sessions(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::PruneSessions { .. })
    })
    .await
}

async fn compact_session(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::CompactSession { .. })
    })
    .await
}

async fn rollback_compaction(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::RollbackCompaction { .. })
    })
    .await
}

async fn session_command(
    state: AppState,
    body: Result<Bytes, BytesRejection>,
    expected: impl FnOnce(&SessionCommand) -> bool,
) -> Response {
    let Ok(_permit) = Arc::clone(&state.session_requests).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are active",
        );
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = match serde_json::from_slice::<CommandRequest>(&body) {
        Ok(request) if expected(&request.command) => request,
        Ok(_) | Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
    };
    // Structured input is bounded at the transport, before the handler can
    // admit anything durably: an oversized or malformed part is a client
    // error, never a queued run.
    let input = match &request.command {
        SessionCommand::SubmitPrompt { input, .. } | SessionCommand::SteerRun { input, .. } => {
            Some(input.as_slice())
        }
        SessionCommand::ResolveWorkspace { .. }
        | SessionCommand::CreateSession { .. }
        | SessionCommand::CancelRun { .. }
        | SessionCommand::RespondToolApproval { .. }
        | SessionCommand::SetApprovalMode { .. }
        | SessionCommand::SetSessionModel { .. }
        | SessionCommand::SetSessionProfile { .. }
        | SessionCommand::DeleteSession { .. }
        | SessionCommand::PruneSessions { .. }
        | SessionCommand::CompactSession { .. }
        | SessionCommand::RollbackCompaction { .. } => None,
    };
    if let Some(input) = input
        && let Err(error) = validate_input(input)
    {
        return api_error(StatusCode::BAD_REQUEST, &format!("invalid input: {error}"));
    }
    match state.handler.command(request).await {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => handler_error_response(error),
    }
}

async fn capabilities(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Ok(_permit) = Arc::clone(&state.session_requests).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are active",
        );
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = if body.is_empty() {
        CapabilitiesRequest::default()
    } else {
        match serde_json::from_slice::<CapabilitiesRequest>(&body) {
            Ok(request) => request,
            Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
        }
    };
    let (profiles, workspace_tools, delegation) = match request.workspace_id {
        None => (None, None, None),
        Some(workspace_id) => {
            let profiles = match state.handler.profiles(workspace_id).await {
                Ok(profiles) => Some(profiles),
                Err(error) => return handler_error_response(error),
            };
            // A workspace whose plan cannot compile still reports its
            // profiles; the tool section is simply absent.
            let tools = match state.handler.workspace_tools(workspace_id).await {
                Ok(tools) => Some(tools),
                Err(ServerHandlerError::Unavailable | ServerHandlerError::Internal) => None,
                Err(ServerHandlerError::InvalidRequest(_)) => None,
            };
            // Delegation follows the same rule: absent when it cannot load.
            let delegation = state.handler.delegation(workspace_id).await.ok();
            (profiles, tools, delegation)
        }
    };
    Json(server_capabilities(
        &state.connection,
        profiles,
        workspace_tools,
        delegation,
    ))
    .into_response()
}

/// The capability document this server build advertises. Every bound is the
/// constant the transport or runtime actually enforces, so a client that
/// formats behavior from this document never trips a limit it was not told.
fn server_capabilities(
    connection: &ServerConnection,
    profiles: Option<Vec<AgentProfileSummary>>,
    workspace_tools: Option<WorkspaceToolCapabilities>,
    delegation: Option<DelegationRoster>,
) -> ServerCapabilities {
    ServerCapabilities {
        version: CAPABILITIES_VERSION,
        protocol_version: PROTOCOL_VERSION,
        server_version: connection.server_info().version.clone(),
        input_parts: InputPartKind::ALL.to_vec(),
        commands: SessionCommandKind::ALL.to_vec(),
        steering: SteeringCapabilities {
            boundary: true,
            interrupt: true,
            max_pending_per_run: qq_core::MAX_PENDING_STEERING,
        },
        limits: LimitCapabilities {
            supported: BudgetLimitKind::ALL.to_vec(),
            max_request_bytes: MAX_REQUEST_BYTES as u64,
            max_event_bytes: MAX_EVENT_BYTES as u64,
            max_input_parts: u16::try_from(MAX_INPUT_PARTS).unwrap_or(u16::MAX),
            max_input_text_bytes: MAX_INPUT_TEXT_BYTES as u64,
            max_input_file_parts: u16::try_from(MAX_INPUT_FILE_PARTS).unwrap_or(u16::MAX),
            max_input_file_bytes: MAX_INPUT_FILE_BYTES as u64,
            max_pending_prompts: qq_core::MAX_PENDING_PROMPTS,
            max_children: qq_core::MAX_SPAWNED_CHILDREN_PER_RUN,
            max_concurrent_children: qq_core::MAX_CONCURRENT_CHILDREN_PER_RUN,
            max_child_depth: qq_core::MAX_CHILD_DEPTH,
            max_correlation_entries: u16::try_from(MAX_CORRELATION_ENTRIES).unwrap_or(u16::MAX),
            max_output_continuations: qq_core::MAX_OUTPUT_CONTINUATIONS,
            max_descendants: qq_core::MAX_DESCENDANTS_PER_ROOT,
        },
        approvals: vec![
            "approve_once".to_owned(),
            "approve_for_session".to_owned(),
            "approve_for_workspace".to_owned(),
            "deny".to_owned(),
        ],
        approval_modes: vec![
            ApprovalMode::ReadOnly,
            ApprovalMode::Ask,
            ApprovalMode::Auto,
            ApprovalMode::Full,
        ],
        profiles,
        tools: ToolCapabilities {
            max_catalog_tools: u32::try_from(qq_core::catalog::MAX_CATALOG_TOOLS)
                .unwrap_or(u32::MAX),
            max_tool_schema_bytes: qq_core::catalog::MAX_TOOL_SCHEMA_BYTES as u64,
            max_catalog_schema_bytes: qq_core::catalog::MAX_CATALOG_SCHEMA_BYTES,
            full_exposure_tools: u32::try_from(qq_core::catalog::FULL_EXPOSURE_TOOLS)
                .unwrap_or(u32::MAX),
            full_exposure_schema_bytes: qq_core::catalog::FULL_EXPOSURE_SCHEMA_BYTES,
            max_pinned_tools: u32::try_from(qq_core::catalog::MAX_PINNED_TOOLS).unwrap_or(u32::MAX),
            max_indexed_skills: u32::try_from(qq_core::MAX_INDEXED_SKILLS).unwrap_or(u32::MAX),
            external_prefixes: vec![
                qq_core::MCP_TOOL_PREFIX.to_owned(),
                qq_core::EMBEDDED_TOOL_PREFIX.to_owned(),
            ],
        },
        workspace_tools,
        events: EventCapabilities {
            post_commit: true,
            replay_page: qq_core::MAX_REPLAY_EVENTS,
            max_subscriptions: u16::try_from(MAX_CONCURRENT_SUBSCRIPTIONS).unwrap_or(u16::MAX),
            max_event_bytes: MAX_EVENT_BYTES as u64,
            retention_bounded: false,
        },
        delegation: delegation.map(|roster| DelegationCapabilities {
            roster,
            max_roster_entries: qq_core::MAX_DELEGATION_ROSTER,
            max_depth_ceiling: qq_core::MAX_CHILD_DEPTH_CEILING,
        }),
    }
}

async fn workspace_snapshot(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Ok(_permit) = Arc::clone(&state.session_requests).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are active",
        );
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = match serde_json::from_slice::<SnapshotRequest>(&body) {
        Ok(request) => request,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
    };
    match state.handler.snapshot(request).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => handler_error_response(error),
    }
}

async fn models(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Response {
    let Ok(_permit) = Arc::clone(&state.session_requests).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are active",
        );
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = match serde_json::from_slice::<ModelCatalogRequest>(&body) {
        Ok(request)
            if !request.workspace.is_empty()
                && request.workspace.len() <= MAX_WORKSPACE_BYTES
                && request
                    .selection
                    .model
                    .as_ref()
                    .is_none_or(|model| model.len() <= MAX_MODEL_BYTES)
                && request
                    .selection
                    .organization
                    .as_ref()
                    .is_none_or(|organization| organization.len() <= MAX_ORGANIZATION_BYTES) =>
        {
            request
        }
        Ok(_) | Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
    };
    match state.handler.models(request).await {
        Ok(models)
            if serde_json::to_vec(&models)
                .is_ok_and(|encoded| encoded.len() <= MAX_MODEL_CATALOG_BYTES) =>
        {
            Json(models).into_response()
        }
        Ok(_) => handler_error_response(ServerHandlerError::Internal),
        Err(error) => handler_error_response(error),
    }
}

async fn workspace_events(
    State(state): State<AppState>,
    AxumPath(workspace_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = match workspace_id.parse::<WorkspaceId>() {
        Ok(workspace_id) => workspace_id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "workspace ID is invalid"),
    };
    let after = match headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
    {
        Some(cursor) => cursor,
        None => return api_error(StatusCode::BAD_REQUEST, "Last-Event-ID is required"),
    };
    let Ok(permit) = Arc::clone(&state.subscriptions).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many event subscriptions are active",
        );
    };
    let events = match state.handler.subscribe(SubscribeRequest {
        workspace_id,
        after,
    }) {
        Ok(events) => events,
        Err(error) => return handler_error_response(error),
    };
    let output = stream! {
        let _permit = permit;
        let mut events = events;
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(_) => return,
            };
            // The store already encoded this event once, on commit; the
            // frame carries those bytes and never re-serializes.
            if event.json.len() > MAX_EVENT_BYTES {
                return;
            }
            yield Ok::<Event, Infallible>(
                Event::default()
                    .id(event.envelope.cursor.to_string())
                    .event("session_event")
                    .data(event.json.as_ref()),
            );
        }
    };
    Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

fn handler_error_response(error: ServerHandlerError) -> Response {
    match error {
        ServerHandlerError::InvalidRequest(message) => api_error(StatusCode::BAD_REQUEST, &message),
        ServerHandlerError::Unavailable => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "request service is unavailable",
        ),
        ServerHandlerError::Internal => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "request failed")
        }
    }
}

#[derive(Serialize)]
struct ApiErrorBody<'a> {
    error: &'a str,
}

fn api_error(status: StatusCode, error: &str) -> Response {
    (status, Json(ApiErrorBody { error })).into_response()
}

async fn method_not_allowed() -> Response {
    api_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

async fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not found")
}

fn constant_time_eq(candidate: &[u8], expected: &[u8]) -> bool {
    let mut difference = candidate.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        difference |= usize::from(candidate_byte ^ expected_byte);
    }
    difference == 0
}

fn valid_process_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 256
        && version.bytes().all(|byte| byte.is_ascii_graphic())
}

fn map_connection_error(error: LocalConnectionError) -> ServerError {
    match error {
        LocalConnectionError::ProtocolMismatch { expected, found } => {
            ServerError::ProtocolMismatch { expected, found }
        }
        LocalConnectionError::InvalidAddress
        | LocalConnectionError::InvalidToken
        | LocalConnectionError::InvalidServerInfo => ServerError::MetadataCorrupt,
    }
}

struct InstanceGuard {
    _lock: File,
    paths: ServerPaths,
    connection: ServerConnection,
    cleaned: bool,
}

impl InstanceGuard {
    fn cleanup(&mut self) -> Result<(), ServerError> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        let Some(metadata) = read_metadata_file(&self.paths)? else {
            return Ok(());
        };
        if !metadata.belongs_to(&self.connection) {
            return Ok(());
        }
        fs::remove_file(&self.paths.metadata_file).map_err(|source| ServerError::StateIo {
            action: "remove metadata from",
            source,
        })?;
        sync_directory(&self.paths.directory)
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn read_connection(paths: &ServerPaths) -> Result<Option<ServerConnection>, ServerError> {
    read_metadata_file(paths)?
        .map(MetadataFile::into_connection)
        .transpose()
}

/// Discovers and probes the current user's running QQ server.
pub async fn discover() -> Result<Option<ServerConnection>, ServerError> {
    discover_with_paths(&ServerPaths::for_user()?).await
}

async fn discover_with_paths(paths: &ServerPaths) -> Result<Option<ServerConnection>, ServerError> {
    let client = probe_client().map_err(|()| ServerError::ExistingServerUnavailable)?;

    for attempt in 0..DISCOVERY_RETRIES {
        let Some(connection) = connection_for_discovery(read_connection(paths))? else {
            return Ok(None);
        };
        match probe_health(&client, &connection).await {
            Ok(info) if info == *connection.server_info() => return Ok(Some(connection)),
            Ok(_) | Err(HealthProbeError::Unavailable) => {}
            Err(HealthProbeError::ProtocolMismatch { found }) => {
                return Err(ServerError::ProtocolMismatch {
                    expected: PROTOCOL_VERSION,
                    found,
                });
            }
        }
        if attempt + 1 < DISCOVERY_RETRIES {
            tokio::time::sleep(DISCOVERY_RETRY_DELAY).await;
        }
    }

    Ok(None)
}

fn connection_for_discovery(
    connection: Result<Option<ServerConnection>, ServerError>,
) -> Result<Option<ServerConnection>, ServerError> {
    match connection {
        Err(ServerError::StateIo { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        connection => connection,
    }
}

fn read_metadata_file(paths: &ServerPaths) -> Result<Option<MetadataFile>, ServerError> {
    if !validate_existing_private_directory(&paths.directory)? {
        return Ok(None);
    }
    let Some(mut file) = open_private_read_file(&paths.metadata_file)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    Read::take(&mut file, (MAX_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| ServerError::StateIo {
            action: "read metadata from",
            source,
        })?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(ServerError::MetadataTooLarge);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ServerError::MetadataCorrupt)?;
    ron::from_str(text)
        .map(Some)
        .map_err(|_| ServerError::MetadataCorrupt)
}

fn write_metadata_atomically(
    paths: &ServerPaths,
    metadata: &MetadataFile,
) -> Result<(), ServerError> {
    if open_private_read_file(&paths.metadata_file)?.is_some() {
        // Opening validates that an existing destination is neither a symlink nor insecure.
    }
    let encoded = ron::ser::to_string(metadata).map_err(|_| ServerError::MetadataCorrupt)?;
    if encoded.len() > MAX_METADATA_BYTES {
        return Err(ServerError::MetadataTooLarge);
    }

    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| ServerError::RandomnessUnavailable)?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = paths.directory.join(format!(
        ".{METADATA_FILE_NAME}.{}.{suffix}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        file.write_all(encoded.as_bytes())
            .map_err(|source| ServerError::StateIo {
                action: "write metadata to",
                source,
            })?;
        file.sync_all().map_err(|source| ServerError::StateIo {
            action: "sync metadata in",
            source,
        })?;
        drop(file);
        fs::rename(&temporary, &paths.metadata_file).map_err(|source| ServerError::StateIo {
            action: "publish metadata in",
            source,
        })?;
        sync_directory(&paths.directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

async fn find_existing_server(paths: &ServerPaths) -> Result<ServerConnection, ServerError> {
    let client = probe_client().map_err(|_| ServerError::ExistingServerUnavailable)?;
    let mut meaningful_error = None;

    for attempt in 0..STARTUP_RETRIES {
        match read_connection(paths) {
            Ok(Some(connection)) => match probe_health(&client, &connection).await {
                Ok(info) if info == *connection.server_info() => return Ok(connection),
                Ok(_) | Err(HealthProbeError::Unavailable) => {}
                Err(HealthProbeError::ProtocolMismatch { found }) => {
                    meaningful_error = Some(ServerError::ProtocolMismatch {
                        expected: PROTOCOL_VERSION,
                        found,
                    });
                }
            },
            Ok(None) => {}
            Err(error @ ServerError::ProtocolMismatch { .. })
            | Err(error @ ServerError::MetadataVersionMismatch { .. })
            | Err(error @ ServerError::MetadataCorrupt)
            | Err(error @ ServerError::MetadataTooLarge) => meaningful_error = Some(error),
            Err(error) => return Err(error),
        }
        if attempt + 1 < STARTUP_RETRIES {
            tokio::time::sleep(STARTUP_RETRY_DELAY).await;
        }
    }

    Err(meaningful_error.unwrap_or(ServerError::ExistingServerUnavailable))
}

fn probe_client() -> Result<reqwest::Client, ()> {
    reqwest::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ())
}

enum HealthProbeError {
    Unavailable,
    ProtocolMismatch { found: u16 },
}

async fn probe_health(
    client: &reqwest::Client,
    connection: &ServerConnection,
) -> Result<ServerInfo, HealthProbeError> {
    let response = client
        .get(connection.endpoint("/v1/health"))
        .bearer_auth(connection.expose_bearer_token())
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|_| HealthProbeError::Unavailable)?;
    if response.status() != StatusCode::OK {
        return Err(HealthProbeError::Unavailable);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HEALTH_BYTES as u64)
    {
        return Err(HealthProbeError::Unavailable);
    }
    let bytes = read_response_bounded(response, MAX_HEALTH_BYTES)
        .await
        .map_err(|_| HealthProbeError::Unavailable)?;
    let info =
        serde_json::from_slice::<ServerInfo>(&bytes).map_err(|_| HealthProbeError::Unavailable)?;
    if info.protocol_version != PROTOCOL_VERSION {
        return Err(HealthProbeError::ProtocolMismatch {
            found: info.protocol_version,
        });
    }
    if info.pid == 0 || !valid_process_version(&info.version) {
        return Err(HealthProbeError::Unavailable);
    }
    Ok(info)
}

async fn read_response_bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ()> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn ensure_private_directory(path: &Path) -> Result<(), ServerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(path)?;
            let metadata = fs::symlink_metadata(path).map_err(|source| ServerError::StateIo {
                action: "inspect",
                source,
            })?;
            validate_directory_metadata(path, &metadata)
        }
        Err(source) => Err(ServerError::StateIo {
            action: "inspect",
            source,
        }),
    }
}

fn validate_existing_private_directory(path: &Path) -> Result<bool, ServerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_directory_metadata(path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ServerError::StateIo {
            action: "inspect",
            source,
        }),
    }
}

fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), ServerError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServerError::InsecureStatePath(path.to_path_buf()));
    }
    validate_private_permissions(path, metadata, 0o700)
}

fn create_private_directory(path: &Path) -> Result<(), ServerError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|source| ServerError::StateIo {
        action: "create",
        source,
    })
}

fn open_private_lock_file(path: &Path) -> Result<File, ServerError> {
    for _ in 0..4 {
        match fs::symlink_metadata(path) {
            Ok(path_metadata) => {
                validate_file_metadata(path, &path_metadata)?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|source| ServerError::StateIo {
                        action: "open lock file in",
                        source,
                    })?;
                validate_open_file(path, &path_metadata, &file)?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match create_private_file(path) {
                    Ok(file) => return Ok(file),
                    Err(ServerError::StateIo { source, .. })
                        if source.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(source) => {
                return Err(ServerError::StateIo {
                    action: "inspect lock file in",
                    source,
                });
            }
        }
    }
    Err(ServerError::StateRace)
}

fn open_private_read_file(path: &Path) -> Result<Option<File>, ServerError> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ServerError::StateIo {
                action: "inspect metadata in",
                source,
            });
        }
    };
    validate_file_metadata(path, &path_metadata)?;
    let file = File::open(path).map_err(|source| ServerError::StateIo {
        action: "open metadata in",
        source,
    })?;
    validate_open_file(path, &path_metadata, &file)?;
    Ok(Some(file))
}

fn create_private_file(path: &Path) -> Result<File, ServerError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|source| ServerError::StateIo {
        action: "create private file in",
        source,
    })?;
    let path_metadata = fs::symlink_metadata(path).map_err(|source| ServerError::StateIo {
        action: "inspect private file in",
        source,
    })?;
    validate_file_metadata(path, &path_metadata)?;
    validate_open_file(path, &path_metadata, &file)?;
    Ok(file)
}

fn validate_file_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), ServerError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServerError::InsecureStatePath(path.to_path_buf()));
    }
    validate_private_permissions(path, metadata, 0o600)
}

fn validate_open_file(
    path: &Path,
    path_metadata: &fs::Metadata,
    file: &File,
) -> Result<(), ServerError> {
    let file_metadata = file.metadata().map_err(|source| ServerError::StateIo {
        action: "inspect open file in",
        source,
    })?;
    validate_file_metadata(path, &file_metadata)?;
    let current_path_metadata =
        fs::symlink_metadata(path).map_err(|source| ServerError::StateIo {
            action: "reinspect open file in",
            source,
        })?;
    validate_file_metadata(path, &current_path_metadata)?;
    if !same_file(path_metadata, &current_path_metadata)
        || !same_file(&current_path_metadata, &file_metadata)
    {
        return Err(ServerError::StateRace);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_permissions(
    path: &Path,
    metadata: &fs::Metadata,
    expected: u32,
) -> Result<(), ServerError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o7777 != expected {
        return Err(ServerError::InsecurePermissions {
            path: path.to_path_buf(),
            expected,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(
    _path: &Path,
    _metadata: &fs::Metadata,
    _expected: u32,
) -> Result<(), ServerError> {
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ServerError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ServerError::StateIo {
            action: "sync",
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ServerError> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("could not determine the user-scoped server state directory")]
    StateDirectoryUnavailable,
    #[error("non-loopback server bind address is not supported: {0}")]
    NonLoopbackBind(SocketAddr),
    #[error("server state path is not a private regular file or directory: {0}")]
    InsecureStatePath(PathBuf),
    #[error("server state path has insecure permissions (expected {expected:o}): {path}")]
    InsecurePermissions { path: PathBuf, expected: u32 },
    #[error("server state changed while it was being validated")]
    StateRace,
    #[error("could not {action} server state")]
    StateIo {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("server metadata is corrupt")]
    MetadataCorrupt,
    #[error("server metadata exceeds the size limit")]
    MetadataTooLarge,
    #[error("server metadata version {found} is unsupported (expected {expected})")]
    MetadataVersionMismatch { expected: u16, found: u16 },
    #[error("server protocol version {found} does not match client version {expected}")]
    ProtocolMismatch { expected: u16, found: u16 },
    #[error("secure random bytes are unavailable")]
    RandomnessUnavailable,
    #[error("could not bind local server at {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("the existing server did not become healthy")]
    ExistingServerUnavailable,
    #[error("local server stopped unexpectedly")]
    Serve {
        #[source]
        source: io::Error,
    },
    #[error("local server task stopped unexpectedly")]
    ServerTaskStopped,
    #[error("local server did not drain active responses before the shutdown deadline")]
    ShutdownTimedOut,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, atomic::AtomicU64, atomic::Ordering},
        time::SystemTime,
    };

    use futures_util::stream as futures_stream;
    use qq_protocol::{
        CommandOutcome, ContentHash, EventCursor, SessionEvent, SessionEventEnvelope, SessionId,
        StoreId,
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        root: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "qq-server-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            Self { root }
        }

        fn paths(&self) -> ServerPaths {
            ServerPaths::new(self.root.join("state"))
        }

        fn child_paths(&self, name: &str) -> ServerPaths {
            ServerPaths::new(self.root.join(name))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct UnavailableHandler;

    impl ServerHandler for UnavailableHandler {}

    fn unavailable_handler() -> Arc<dyn ServerHandler> {
        Arc::new(UnavailableHandler)
    }

    struct HeldSubscriptionHandler {
        event: SessionEventEnvelope,
    }

    impl ServerHandler for HeldSubscriptionHandler {
        fn subscribe(
            &self,
            _request: SubscribeRequest,
        ) -> Result<PublishedEventStream, ServerHandlerError> {
            let json = serde_json::to_string(&self.event).expect("test envelope encodes");
            let published = Arc::new(qq_core::PublishedEvent {
                envelope: self.event.clone(),
                json: Arc::from(json),
            });
            let events = futures_stream::iter([Ok(published)]).chain(futures_stream::pending());
            Ok(Box::pin(events))
        }
    }

    struct CommandCaptureHandler {
        commands: Arc<Mutex<Vec<CommandRequest>>>,
    }

    impl ServerHandler for CommandCaptureHandler {
        fn command(&self, request: CommandRequest) -> CommandFuture {
            let command_id = request.command_id;
            let outcome = match &request.command {
                SessionCommand::SubmitPrompt { session_id, .. } => CommandOutcome::PromptQueued {
                    session_id: *session_id,
                    run_id: qq_protocol::RunId::generate().unwrap(),
                    queue_position: 0,
                },
                _ => {
                    return Box::pin(async {
                        Err(ServerHandlerError::InvalidRequest(
                            "unexpected command".to_owned(),
                        ))
                    });
                }
            };
            self.commands.lock().unwrap().push(request);
            Box::pin(async move {
                Ok(CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id: StoreId::generate().unwrap(),
                        workspace_id: WorkspaceId::generate().unwrap(),
                        sequence: 1,
                    },
                    outcome,
                })
            })
        }
    }

    async fn start_test_server(
        paths: ServerPaths,
        handler: Arc<dyn ServerHandler>,
    ) -> ServerHandle {
        match start(handler, ServerOptions::new(paths)).await.unwrap() {
            StartOutcome::Started(handle) => handle,
            StartOutcome::Existing(_) => panic!("test unexpectedly found an existing server"),
        }
    }

    #[tokio::test]
    async fn health_requires_the_metadata_token() {
        let directory = TestDirectory::new();
        let handler = unavailable_handler();
        let server = start_test_server(directory.paths(), handler).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let health_url = server.connection().endpoint("/v1/health");

        let missing = http.get(&health_url).send().await.unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let wrong = http
            .get(&health_url)
            .header(AUTHORIZATION, format!("Bearer {}", "0".repeat(64)))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let response = http
            .get(&health_url)
            .bearer_auth(server.connection().expose_bearer_token())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<ServerInfo>().await.unwrap(),
            *server.connection().server_info()
        );
        let unsupported = http
            .post(&health_url)
            .bearer_auth(server.connection().expose_bearer_token())
            .send()
            .await
            .unwrap();
        assert_eq!(unsupported.status(), StatusCode::METHOD_NOT_ALLOWED);
        let missing_route = http
            .get(server.connection().endpoint("/not-a-route"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_route.status(), StatusCode::NOT_FOUND);

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_ask_route_is_not_exposed() {
        let directory = TestDirectory::new();
        let handler = unavailable_handler();
        let server = start_test_server(directory.paths(), handler).await;
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(server.connection().endpoint("/v1/ask"))
            .bearer_auth(server.connection().expose_bearer_token())
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn server_forwards_runtime_slash_invocations_without_client_semantics() {
        let directory = TestDirectory::new();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<dyn ServerHandler> = Arc::new(CommandCaptureHandler {
            commands: Arc::clone(&commands),
        });
        let server = start_test_server(directory.paths(), handler).await;
        let session_id = SessionId::generate().unwrap();
        let request = CommandRequest {
            command_id: qq_protocol::CommandId::generate().unwrap(),
            command: SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text(
                    "/review focus on cancellation".to_owned(),
                )],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        };

        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(server.connection().endpoint("/v1/sessions/prompts"))
            .bearer_auth(server.connection().expose_bearer_token())
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        response.json::<CommandReceipt>().await.unwrap();
        {
            let captured = commands.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert!(matches!(
                &captured[0].command,
                SessionCommand::SubmitPrompt { input, .. }
                    if input.as_slice() == [qq_protocol::InputPart::text("/review focus on cancellation")]
            ));
        }
        server.shutdown().await.unwrap();
    }

    /// A handler with a command journal: identical replays return the stored
    /// receipt, a reused id with a different body is a conflict. Mirrors the
    /// durable runtime's contract so route-level idempotency is testable
    /// without SQLite.
    struct JournalHandler {
        journal: Mutex<Vec<(CommandRequest, CommandReceipt)>>,
        handled: Arc<Mutex<Vec<SessionCommand>>>,
    }

    impl ServerHandler for JournalHandler {
        fn command(&self, request: CommandRequest) -> CommandFuture {
            let mut journal = self.journal.lock().unwrap();
            if let Some((stored, receipt)) = journal
                .iter()
                .find(|(stored, _)| stored.command_id == request.command_id)
            {
                let receipt = receipt.clone();
                return if stored.command == request.command {
                    Box::pin(async move { Ok(receipt) })
                } else {
                    Box::pin(async {
                        Err(ServerHandlerError::InvalidRequest(
                            "command ID was reused with different content".to_owned(),
                        ))
                    })
                };
            }
            let run_id = qq_protocol::RunId::from_bytes([4; 16]);
            let outcome = match &request.command {
                SessionCommand::SubmitPrompt { session_id, .. } => CommandOutcome::PromptQueued {
                    session_id: *session_id,
                    run_id,
                    queue_position: 1,
                },
                SessionCommand::SteerRun { run_id, .. } => CommandOutcome::SteeringQueued {
                    run_id: *run_id,
                    message_id: qq_protocol::MessageId::from_bytes([9; 16]),
                },
                SessionCommand::CancelRun { run_id } => {
                    CommandOutcome::CancellationRequested { run_id: *run_id }
                }
                SessionCommand::RespondToolApproval { tool_call_id, .. } => {
                    CommandOutcome::ToolApprovalResolved {
                        tool_call_id: *tool_call_id,
                        resolution: qq_protocol::ApprovalResolution::ApprovedOnce,
                    }
                }
                SessionCommand::SetSessionProfile {
                    session_id,
                    profile,
                } => CommandOutcome::SessionProfileSet {
                    session_id: *session_id,
                    profile: profile.clone(),
                },
                _ => {
                    return Box::pin(async {
                        Err(ServerHandlerError::InvalidRequest(
                            "unexpected command".to_owned(),
                        ))
                    });
                }
            };
            let receipt = CommandReceipt {
                command_id: request.command_id,
                committed_through: EventCursor {
                    store_id: StoreId::from_bytes([1; 16]),
                    workspace_id: WorkspaceId::from_bytes([2; 16]),
                    sequence: u64::try_from(journal.len()).unwrap() + 1,
                },
                outcome,
            };
            self.handled.lock().unwrap().push(request.command.clone());
            journal.push((request, receipt.clone()));
            Box::pin(async move { Ok(receipt) })
        }

        fn profiles(&self, workspace_id: WorkspaceId) -> ProfilesFuture {
            Box::pin(async move {
                if workspace_id == WorkspaceId::from_bytes([2; 16]) {
                    Ok(vec![AgentProfileSummary {
                        id: qq_protocol::AgentProfileId::default(),
                        model: Some("openai/gpt-5.6".to_owned()),
                        approval_mode: ApprovalMode::Auto,
                        pack: None,
                    }])
                } else {
                    Err(ServerHandlerError::InvalidRequest(
                        "workspace was not found".to_owned(),
                    ))
                }
            })
        }

        fn workspace_tools(&self, workspace_id: WorkspaceId) -> WorkspaceToolsFuture {
            Box::pin(async move {
                if workspace_id == WorkspaceId::from_bytes([2; 16]) {
                    Ok(WorkspaceToolCapabilities {
                        catalog_digest: ContentHash::from_bytes([5; 32]),
                        exposure: qq_protocol::ToolExposure::Progressive,
                        hosts: vec![qq_protocol::ToolHostSummary {
                            name: "mcp".to_owned(),
                            generation: 3,
                            tool_count: 40,
                            ready: true,
                            message: None,
                        }],
                        excluded_tools: 1,
                        skills: qq_protocol::SkillCapabilities {
                            digest: ContentHash::from_bytes([6; 32]),
                            indexed: 2,
                            disclosed: 1,
                            truncated: false,
                            entries: Vec::new(),
                        },
                    })
                } else {
                    Err(ServerHandlerError::Unavailable)
                }
            })
        }
    }

    async fn post(
        server: &ServerHandle,
        path: &str,
        body: &impl Serialize,
    ) -> (StatusCode, serde_json::Value) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(server.connection().endpoint(path))
            .bearer_auth(server.connection().expose_bearer_token())
            .json(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.json::<serde_json::Value>().await.unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn retried_commands_replay_receipts_and_conflicting_reuse_is_rejected() {
        let directory = TestDirectory::new();
        let handled = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<dyn ServerHandler> = Arc::new(JournalHandler {
            journal: Mutex::new(Vec::new()),
            handled: Arc::clone(&handled),
        });
        let server = start_test_server(directory.paths(), handler).await;
        let session_id = SessionId::from_bytes([3; 16]);
        let run_id = qq_protocol::RunId::from_bytes([4; 16]);
        let commands: [(&str, SessionCommand); 5] = [
            (
                "/v1/sessions/prompts",
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text("go")],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            ),
            (
                "/v1/runs/steer",
                SessionCommand::SteerRun {
                    run_id,
                    input: vec![qq_protocol::InputPart::text("also tests")],
                    interrupt: true,
                },
            ),
            (
                "/v1/tools/approvals",
                SessionCommand::RespondToolApproval {
                    run_id,
                    tool_call_id: qq_protocol::ToolCallId::from_bytes([5; 16]),
                    decision: qq_protocol::ApprovalDecision::ApproveOnce,
                },
            ),
            ("/v1/runs/cancel", SessionCommand::CancelRun { run_id }),
            (
                "/v1/sessions/profile",
                SessionCommand::SetSessionProfile {
                    session_id,
                    profile: qq_protocol::AgentProfileId::new("review").unwrap(),
                },
            ),
        ];
        for (path, command) in commands {
            let request = CommandRequest {
                command_id: qq_protocol::CommandId::generate().unwrap(),
                command,
            };
            let (status, first) = post(&server, path, &request).await;
            assert_eq!(status, StatusCode::OK, "{path}: {first}");
            let (status, replay) = post(&server, path, &request).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(replay, first, "{path}: a retry must replay the receipt");
            // Same id, different body: rejected, and the handler never ran it.
            let conflicting = CommandRequest {
                command_id: request.command_id,
                command: SessionCommand::CancelRun {
                    run_id: qq_protocol::RunId::from_bytes([0xee; 16]),
                },
            };
            let (status, body) = post(&server, "/v1/runs/cancel", &conflicting).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["error"].as_str().unwrap().contains("reused"));
        }
        assert_eq!(
            handled.lock().unwrap().len(),
            5,
            "each command ran exactly once"
        );

        // A body on the wrong route never reaches the handler.
        let misrouted = CommandRequest {
            command_id: qq_protocol::CommandId::generate().unwrap(),
            command: SessionCommand::CancelRun { run_id },
        };
        let (status, _) = post(&server, "/v1/runs/steer", &misrouted).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(handled.lock().unwrap().len(), 5);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_and_oversized_input_fails_before_the_handler() {
        let directory = TestDirectory::new();
        let handled = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<dyn ServerHandler> = Arc::new(JournalHandler {
            journal: Mutex::new(Vec::new()),
            handled: Arc::clone(&handled),
        });
        let server = start_test_server(directory.paths(), handler).await;
        let session_id = SessionId::from_bytes([3; 16]);
        let bad_inputs = [
            serde_json::json!([]),
            serde_json::json!([{"type": "text", "text": "   "}]),
            serde_json::json!([{"type": "workspace_file", "path": "/etc/passwd"}]),
            serde_json::json!([{"type": "image", "url": "x"}]),
            serde_json::json!([{"type": "text", "text": "x", "extra": 1}]),
            serde_json::json!([{"type": "text", "text": "a".repeat(MAX_INPUT_TEXT_BYTES + 1)}]),
        ];
        for input in bad_inputs {
            let body = serde_json::json!({
                "command_id": qq_protocol::CommandId::generate().unwrap(),
                "command": {"type": "submit_prompt", "session_id": session_id, "input": input},
            });
            let (status, _) = post(&server, "/v1/sessions/prompts", &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let body = serde_json::json!({
                "command_id": qq_protocol::CommandId::generate().unwrap(),
                "command": {"type": "steer_run", "run_id": qq_protocol::RunId::from_bytes([4; 16]), "input": input},
            });
            let (status, _) = post(&server, "/v1/runs/steer", &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        // Unknown command fields and a bad profile id are rejected by the
        // strict decoder before the handler too.
        for command in [
            serde_json::json!({"type": "submit_prompt", "session_id": session_id, "input": [{"type":"text","text":"x"}], "prompt": "legacy"}),
            serde_json::json!({"type": "set_session_profile", "session_id": session_id, "profile": "Not Valid"}),
            serde_json::json!({"type": "submit_prompt", "session_id": session_id, "input": [{"type":"text","text":"x"}], "correlation": {"k": "v".repeat(300)}}),
        ] {
            let path = if command["type"] == "set_session_profile" {
                "/v1/sessions/profile"
            } else {
                "/v1/sessions/prompts"
            };
            let body = serde_json::json!({
                "command_id": qq_protocol::CommandId::generate().unwrap(),
                "command": command,
            });
            let (status, _) = post(&server, path, &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        // Oversized bodies are refused at the transport.
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(server.connection().endpoint("/v1/sessions/prompts"))
            .bearer_auth(server.connection().expose_bearer_token())
            .header("content-type", "application/json")
            .body(vec![b' '; MAX_REQUEST_BYTES + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            handled.lock().unwrap().is_empty(),
            "nothing reached the handler"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn capabilities_document_advertises_bounds_commands_and_workspace_profiles() {
        let directory = TestDirectory::new();
        let handler: Arc<dyn ServerHandler> = Arc::new(JournalHandler {
            journal: Mutex::new(Vec::new()),
            handled: Arc::new(Mutex::new(Vec::new())),
        });
        let server = start_test_server(directory.paths(), handler).await;
        let (status, body) =
            post(&server, "/v1/capabilities", &CapabilitiesRequest::default()).await;
        assert_eq!(status, StatusCode::OK);
        let capabilities: ServerCapabilities = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(capabilities.version, CAPABILITIES_VERSION);
        assert_eq!(capabilities.protocol_version, PROTOCOL_VERSION);
        assert_eq!(capabilities.input_parts, InputPartKind::ALL.to_vec());
        assert_eq!(capabilities.commands, SessionCommandKind::ALL.to_vec());
        assert!(capabilities.steering.boundary && capabilities.steering.interrupt);
        assert_eq!(
            capabilities.steering.max_pending_per_run,
            qq_core::MAX_PENDING_STEERING
        );
        assert_eq!(capabilities.limits.supported, BudgetLimitKind::ALL.to_vec());
        assert_eq!(
            capabilities.limits.max_request_bytes,
            MAX_REQUEST_BYTES as u64
        );
        assert_eq!(capabilities.limits.max_child_depth, 3);
        assert_eq!(capabilities.limits.max_descendants, 24);
        assert_eq!(
            capabilities.limits.max_children,
            qq_core::MAX_SPAWNED_CHILDREN_PER_RUN
        );
        assert!(capabilities.profiles.is_none());
        assert!(capabilities.workspace_tools.is_none());
        assert_eq!(
            capabilities.tools.max_catalog_tools as usize,
            qq_core::catalog::MAX_CATALOG_TOOLS
        );
        assert_eq!(capabilities.tools.external_prefixes, ["mcp__", "ext__"]);
        assert_eq!(body["approvals"][3], "deny");

        let (status, body) = post(
            &server,
            "/v1/capabilities",
            &CapabilitiesRequest {
                workspace_id: Some(WorkspaceId::from_bytes([2; 16])),
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let capabilities: ServerCapabilities = serde_json::from_value(body).unwrap();
        let profiles = capabilities.profiles.unwrap();
        assert_eq!(profiles.len(), 1);
        assert!(profiles[0].id.is_default());
        let tools = capabilities.workspace_tools.unwrap();
        assert_eq!(tools.exposure, qq_protocol::ToolExposure::Progressive);
        assert_eq!(tools.hosts[0].tool_count, 40);
        assert_eq!(tools.skills.disclosed, 1);

        let (status, _) = post(
            &server,
            "/v1/capabilities",
            &CapabilitiesRequest {
                workspace_id: Some(WorkspaceId::from_bytes([7; 16])),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post(&server, "/v1/capabilities", &serde_json::json!({"nope": 1})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Version skew: a client built against an older struct still reads a
        // newer server's health and capabilities documents.
        let health: serde_json::Value = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(server.connection().endpoint("/v1/health"))
            .bearer_auth(server.connection().expose_bearer_token())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut future_health = health.clone();
        future_health["future_field"] = serde_json::json!({"a": 1});
        let decoded: ServerInfo = serde_json::from_value(future_health).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn only_one_concurrent_start_wins() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let handler = unavailable_handler();

        let (left, right) = tokio::join!(
            start(Arc::clone(&handler), ServerOptions::new(paths.clone())),
            start(handler, ServerOptions::new(paths)),
        );

        let mut started = None;
        let mut existing = None;
        for outcome in [left.unwrap(), right.unwrap()] {
            match outcome {
                StartOutcome::Started(handle) => {
                    assert!(started.replace(handle).is_none());
                }
                StartOutcome::Existing(connection) => {
                    assert!(existing.replace(connection).is_none());
                }
            }
        }
        let started = started.expect("one start should win");
        assert_eq!(
            existing.expect("one start should discover it").address(),
            started.connection().address()
        );
        started.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn detects_an_already_running_server() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let handler = unavailable_handler();
        let server = start_test_server(paths.clone(), Arc::clone(&handler)).await;

        let outcome = start(handler, ServerOptions::new(paths)).await.unwrap();
        let StartOutcome::Existing(existing) = outcome else {
            panic!("second start should report the existing server");
        };
        assert_eq!(existing, *server.connection());

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn discovers_the_running_server_connection() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let handler = unavailable_handler();
        let server = start_test_server(paths.clone(), handler).await;

        assert_eq!(
            discover_with_paths(&paths).await.unwrap(),
            Some(server.connection().clone())
        );

        server.shutdown().await.unwrap();
    }

    #[test]
    fn discovery_treats_removed_metadata_as_no_server() {
        let result = connection_for_discovery(Err(ServerError::StateIo {
            action: "open metadata in",
            source: io::Error::from(io::ErrorKind::NotFound),
        }))
        .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn replaces_stale_metadata_when_the_lock_is_available() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        ensure_private_directory(paths.directory()).unwrap();
        let stale_listener = TcpListener::bind(DEFAULT_BIND_ADDRESS).await.unwrap();
        let stale_address = stale_listener.local_addr().unwrap();
        drop(stale_listener);
        let stale = ServerConnection::new(
            stale_address,
            "b".repeat(TOKEN_HEX_BYTES),
            ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "stale".to_owned(),
                pid: 42,
            },
        )
        .unwrap();
        write_metadata_atomically(&paths, &MetadataFile::new(&stale)).unwrap();
        let probe = probe_client().unwrap();
        assert!(matches!(
            probe_health(&probe, &stale).await,
            Err(HealthProbeError::Unavailable)
        ));
        let handler = unavailable_handler();

        let server = start_test_server(paths.clone(), handler).await;

        let current = read_connection(&paths).unwrap().unwrap();
        assert_eq!(current, *server.connection());
        assert_ne!(current, stale);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reports_protocol_mismatch_while_another_instance_owns_the_lock() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        ensure_private_directory(paths.directory()).unwrap();
        let lock = open_private_lock_file(paths.lock_file()).unwrap();
        lock.try_lock().unwrap();
        let mut metadata = MetadataFile {
            format_version: METADATA_FORMAT_VERSION,
            address: "127.0.0.1:9".to_owned(),
            pid: 42,
            protocol_version: PROTOCOL_VERSION + 1,
            version: "future".to_owned(),
            token: "c".repeat(TOKEN_HEX_BYTES),
        };
        metadata.format_version = METADATA_FORMAT_VERSION + 1;
        metadata.protocol_version = PROTOCOL_VERSION;
        write_metadata_atomically(&paths, &metadata).unwrap();
        assert!(matches!(
            read_connection(&paths),
            Err(ServerError::MetadataVersionMismatch {
                expected,
                found,
            }) if expected == METADATA_FORMAT_VERSION && found == METADATA_FORMAT_VERSION + 1
        ));
        metadata.format_version = METADATA_FORMAT_VERSION;
        metadata.protocol_version = PROTOCOL_VERSION + 1;
        write_metadata_atomically(&paths, &metadata).unwrap();

        assert!(matches!(
            read_connection(&paths),
            Err(ServerError::ProtocolMismatch {
                expected,
                found,
            }) if expected == PROTOCOL_VERSION && found == PROTOCOL_VERSION + 1
        ));
        let handler = unavailable_handler();
        let error = start(handler, ServerOptions::new(paths)).await.unwrap_err();
        assert!(matches!(
            error,
            ServerError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found,
            } if found == PROTOCOL_VERSION + 1
        ));
    }

    #[tokio::test]
    async fn dropping_a_reservation_cleans_metadata_and_releases_ownership() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let outcome = reserve(ServerOptions::new(paths.clone())).await.unwrap();
        let ReserveOutcome::Reserved(reservation) = outcome else {
            panic!("test unexpectedly found an existing server");
        };
        assert!(paths.metadata_file().is_file());

        drop(reservation);

        assert!(!paths.metadata_file().exists());
        let lock = open_private_lock_file(paths.lock_file()).unwrap();
        lock.try_lock().unwrap();
    }

    #[tokio::test]
    async fn graceful_shutdown_removes_owned_metadata_and_releases_the_lock() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let handler = unavailable_handler();
        let server = start_test_server(paths.clone(), handler).await;
        assert!(paths.metadata_file().is_file());
        assert!(read_connection(&paths).unwrap().is_some());

        server.shutdown().await.unwrap();

        assert!(!paths.metadata_file().exists());
        assert!(read_connection(&paths).unwrap().is_none());
        let lock = open_private_lock_file(paths.lock_file()).unwrap();
        lock.try_lock().unwrap();
    }

    #[tokio::test]
    async fn shutdown_bounds_a_held_event_subscription_and_releases_ownership() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let store_id = StoreId::generate().unwrap();
        let workspace_id = WorkspaceId::generate().unwrap();
        let session_id = SessionId::generate().unwrap();
        let after = EventCursor {
            store_id,
            workspace_id,
            sequence: 0,
        };
        let handler = Arc::new(HeldSubscriptionHandler {
            event: SessionEventEnvelope {
                cursor: EventCursor {
                    store_id,
                    workspace_id,
                    sequence: 1,
                },
                session_id,
                run_id: None,
                caused_by: None,
                occurred_at_ms: 0,
                event: SessionEvent::SessionDeleted { session_id },
            },
        });
        let server = start_test_server(paths.clone(), handler).await;
        let events_url = server
            .connection()
            .endpoint(&format!("/v1/workspaces/{workspace_id}/events"));
        let token = server.connection().expose_bearer_token().to_owned();
        let mut response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(events_url)
            .bearer_auth(token)
            .header("last-event-id", after.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.chunk().await.unwrap().is_some());

        let error = server
            .shutdown_with_grace(Duration::from_millis(20))
            .await
            .unwrap_err();

        assert!(matches!(error, ServerError::ShutdownTimedOut));
        assert!(!paths.metadata_file().exists());
        let lock = open_private_lock_file(paths.lock_file()).unwrap();
        lock.try_lock().unwrap();
    }

    #[tokio::test]
    async fn dropping_an_event_response_releases_its_stream_and_subscription_slot() {
        struct DropSignal(tokio::sync::mpsc::Sender<()>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                // A failed test may have already dropped its receiver.
                let _ = self.0.try_send(());
            }
        }

        struct DropObservedHandler {
            held: HeldSubscriptionHandler,
            dropped: tokio::sync::mpsc::Sender<()>,
        }

        impl ServerHandler for DropObservedHandler {
            fn subscribe(
                &self,
                request: SubscribeRequest,
            ) -> Result<PublishedEventStream, ServerHandlerError> {
                let mut events = self.held.subscribe(request)?;
                let signal = DropSignal(self.dropped.clone());
                Ok(Box::pin(stream! {
                    let _signal = signal;
                    while let Some(event) = events.next().await {
                        yield event;
                    }
                }))
            }
        }

        let directory = TestDirectory::new();
        let workspace_id = WorkspaceId::generate().unwrap();
        let session_id = SessionId::generate().unwrap();
        let after = EventCursor {
            store_id: StoreId::generate().unwrap(),
            workspace_id,
            sequence: 0,
        };
        let (dropped, mut drops) = tokio::sync::mpsc::channel(MAX_CONCURRENT_SUBSCRIPTIONS + 1);
        let handler = Arc::new(DropObservedHandler {
            held: HeldSubscriptionHandler {
                event: SessionEventEnvelope {
                    cursor: EventCursor {
                        sequence: 1,
                        ..after
                    },
                    session_id,
                    run_id: None,
                    caused_by: None,
                    occurred_at_ms: 0,
                    event: SessionEvent::SessionDeleted { session_id },
                },
            },
            dropped,
        });
        let server = start_test_server(directory.paths(), handler).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let request = || {
            client
                .get(
                    server
                        .connection()
                        .endpoint(&format!("/v1/workspaces/{workspace_id}/events")),
                )
                .bearer_auth(server.connection().expose_bearer_token())
                .header("last-event-id", after.to_string())
                .send()
        };
        let mut responses = Vec::new();
        for _ in 0..MAX_CONCURRENT_SUBSCRIPTIONS {
            let mut response = request().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.chunk().await.unwrap().is_some());
            responses.push(response);
        }
        assert_eq!(
            request().await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(responses.pop());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), drops.recv())
                .await
                .unwrap(),
            Some(())
        );
        let mut replacement = request().await.unwrap();
        assert_eq!(replacement.status(), StatusCode::OK);
        assert!(replacement.chunk().await.unwrap().is_some());
        drop(replacement);
        drop(responses);
        tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..MAX_CONCURRENT_SUBSCRIPTIONS {
                assert_eq!(drops.recv().await, Some(()));
            }
        })
        .await
        .unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_does_not_remove_replaced_metadata() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let handler = unavailable_handler();
        let server = start_test_server(paths.clone(), handler).await;
        let replacement = ServerConnection::new(
            "127.0.0.1:10".parse().unwrap(),
            "d".repeat(TOKEN_HEX_BYTES),
            ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "replacement".to_owned(),
                pid: 43,
            },
        )
        .unwrap();
        write_metadata_atomically(&paths, &MetadataFile::new(&replacement)).unwrap();

        server.shutdown().await.unwrap();

        assert_eq!(read_connection(&paths).unwrap().unwrap(), replacement);
    }

    #[test]
    fn connection_and_metadata_formatting_redact_tokens() {
        let token = "f".repeat(TOKEN_HEX_BYTES);
        let connection = ServerConnection::new(
            "127.0.0.1:1234".parse().unwrap(),
            token.clone(),
            ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "test".to_owned(),
                pid: 1,
            },
        )
        .unwrap();
        let metadata = MetadataFile::new(&connection);

        assert!(!format!("{connection:?}").contains(&token));
        assert!(!connection.to_string().contains(&token));
        assert!(!format!("{metadata:?}").contains(&token));
        assert!(!metadata.to_string().contains(&token));
    }

    #[tokio::test]
    async fn rejects_non_loopback_bind_addresses() {
        let directory = TestDirectory::new();
        let handler = unavailable_handler();
        let options =
            ServerOptions::new(directory.paths()).with_bind_address("0.0.0.0:0".parse().unwrap());

        let error = start(handler, options).await.unwrap_err();

        assert!(matches!(error, ServerError::NonLoopbackBind(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enforces_unix_permissions_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = TestDirectory::new();
        let secure_paths = directory.child_paths("secure");
        let handler = unavailable_handler();
        let server = start_test_server(secure_paths.clone(), handler).await;
        assert_eq!(
            fs::metadata(secure_paths.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(secure_paths.metadata_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        server.shutdown().await.unwrap();

        let insecure_paths = directory.child_paths("insecure");
        fs::create_dir(insecure_paths.directory()).unwrap();
        fs::set_permissions(
            insecure_paths.directory(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let handler = unavailable_handler();
        assert!(matches!(
            start(handler, ServerOptions::new(insecure_paths))
                .await
                .unwrap_err(),
            ServerError::InsecurePermissions { .. }
        ));

        let symlink_paths = directory.child_paths("symlink-state");
        let target = directory.root.join("symlink-target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, symlink_paths.directory()).unwrap();
        let handler = unavailable_handler();
        assert!(matches!(
            start(handler, ServerOptions::new(symlink_paths))
                .await
                .unwrap_err(),
            ServerError::InsecureStatePath(_)
        ));

        let metadata_paths = directory.child_paths("metadata-symlink");
        ensure_private_directory(metadata_paths.directory()).unwrap();
        let metadata_target = directory.root.join("metadata-target");
        File::create(&metadata_target).unwrap();
        symlink(&metadata_target, metadata_paths.metadata_file()).unwrap();
        assert!(matches!(
            read_connection(&metadata_paths).unwrap_err(),
            ServerError::InsecureStatePath(_)
        ));

        let permission_paths = directory.child_paths("metadata-permissions");
        ensure_private_directory(permission_paths.directory()).unwrap();
        fs::write(permission_paths.metadata_file(), b"not important").unwrap();
        fs::set_permissions(
            permission_paths.metadata_file(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(matches!(
            read_connection(&permission_paths).unwrap_err(),
            ServerError::InsecurePermissions { .. }
        ));
    }
}
