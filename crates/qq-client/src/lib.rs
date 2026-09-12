//! Authenticated HTTP/SSE client for QQ servers: the local loopback instance
//! or a remote one reached over TLS.
//!
//! The crate has two transports selected by cargo feature. `native` (default)
//! drives `reqwest` on Tokio and ships the interactive `TuiClient` loop and
//! the durable `observer`. `wasm` drives the browser's `fetch` through the
//! same `reqwest` surface; the decoder, cursor validation, and `ClientPort`
//! vocabulary are identical, and the embedding app owns its own loop.

#![forbid(unsafe_code)]

use std::{marker::PhantomData, path::Path, pin::Pin, time::Duration};

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use qq_protocol::{
    AgentProfileId, ApprovalDecision, CapabilitiesRequest, CommandId, CommandReceipt,
    CommandRequest, Correlation, EventCursor, InputPart, MAX_EVENT_BYTES, MAX_REQUEST_BYTES,
    ModelCatalogRequest, ModelDescriptor, RunId, RunLimits, ServerCapabilities, SessionCommand,
    SessionEventEnvelope, SessionId, SnapshotRequest, ToolCallId, WorkspaceId, WorkspaceSnapshot,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;

#[cfg(not(any(feature = "native", all(feature = "wasm", target_arch = "wasm32"))))]
compile_error!(
    "qq-client needs a transport: enable `native` (default) on native targets or `wasm` on wasm32"
);

#[cfg(feature = "native")]
mod interactive;
#[cfg(feature = "native")]
pub mod observer;
mod port;
pub mod state;
mod time;

#[cfg(feature = "native")]
pub use interactive::{InitialSession, TuiClient};
pub use port::{ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState};

/// Marker for values that must cross threads on native targets. Browser
/// WebAssembly is single-threaded and its futures are `!Send`, so the bound
/// is dropped there without changing any public signature.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

#[cfg(not(target_arch = "wasm32"))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_SSE_WIRE_EVENT_BYTES: usize = MAX_EVENT_BYTES + 16 * 1024;
const MAX_SSE_LINE_BYTES: usize = MAX_SSE_WIRE_EVENT_BYTES;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_MODEL_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_CAPABILITIES_BYTES: usize = 256 * 1024;

/// Authenticated coordinates for the server a client attaches to. A local
/// `LocalServerConnection` converts losslessly with `into()`.
pub type Connection = qq_protocol::ServerConnection;

fn fresh_command_id() -> Result<CommandId, ClientError> {
    CommandId::generate().map_err(|_| ClientError::Unavailable)
}

#[cfg(not(target_arch = "wasm32"))]
pub type SessionEventStream =
    Pin<Box<dyn Stream<Item = Result<SessionEventEnvelope, ClientError>> + Send + 'static>>;
#[cfg(target_arch = "wasm32")]
pub type SessionEventStream =
    Pin<Box<dyn Stream<Item = Result<SessionEventEnvelope, ClientError>> + 'static>>;

#[derive(Clone)]
pub struct SessionClient {
    connection: Connection,
    http: reqwest::Client,
}

impl SessionClient {
    pub fn new(connection: impl Into<Connection>) -> Result<Self, ClientError> {
        let connection = connection.into();
        let http = http_client()?;
        Ok(Self { connection, http })
    }

    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, ClientError> {
        let path = match command {
            SessionCommand::ResolveWorkspace { .. } => "/v1/workspaces/resolve",
            SessionCommand::CreateSession { .. } => "/v1/sessions",
            SessionCommand::SubmitPrompt { .. } => "/v1/sessions/prompts",
            SessionCommand::SteerRun { .. } => "/v1/runs/steer",
            SessionCommand::CancelRun { .. } => "/v1/runs/cancel",
            SessionCommand::RespondToolApproval { .. } => "/v1/tools/approvals",
            SessionCommand::SetApprovalMode { .. } => "/v1/sessions/approval-mode",
            SessionCommand::SetSessionModel { .. } => "/v1/sessions/model",
            SessionCommand::SetSessionProfile { .. } => "/v1/sessions/profile",
            SessionCommand::DeleteSession { .. } => "/v1/sessions/delete",
            SessionCommand::PruneSessions { .. } => "/v1/sessions/prune",
            SessionCommand::CompactSession { .. } => "/v1/sessions/compact",
            SessionCommand::RollbackCompaction { .. } => "/v1/sessions/compact/rollback",
        };
        self.post_json(
            path,
            &CommandRequest {
                command_id,
                command,
            },
            MAX_ERROR_BODY_BYTES,
        )
        .await
    }

    /// Queues a new run on `session` from structured input. Every command id
    /// is fresh; callers that need retry idempotency use [`Self::command`]
    /// with their own id.
    pub async fn submit(
        &self,
        session_id: SessionId,
        input: Vec<InputPart>,
        limits: RunLimits,
        correlation: Correlation,
    ) -> Result<CommandReceipt, ClientError> {
        self.command(
            fresh_command_id()?,
            SessionCommand::SubmitPrompt {
                session_id,
                input,
                limits,
                correlation,
            },
        )
        .await
    }

    /// Adds input to an executing run at its next model/tool boundary.
    pub async fn steer(
        &self,
        run_id: RunId,
        input: Vec<InputPart>,
    ) -> Result<CommandReceipt, ClientError> {
        self.command(
            fresh_command_id()?,
            SessionCommand::SteerRun {
                run_id,
                input,
                interrupt: false,
            },
        )
        .await
    }

    /// Aborts the run's in-flight provider stream or tool and applies `input`
    /// at the boundary that creates.
    pub async fn interrupt(
        &self,
        run_id: RunId,
        input: Vec<InputPart>,
    ) -> Result<CommandReceipt, ClientError> {
        self.command(
            fresh_command_id()?,
            SessionCommand::SteerRun {
                run_id,
                input,
                interrupt: true,
            },
        )
        .await
    }

    pub async fn cancel(&self, run_id: RunId) -> Result<CommandReceipt, ClientError> {
        self.command(fresh_command_id()?, SessionCommand::CancelRun { run_id })
            .await
    }

    pub async fn approve(
        &self,
        run_id: RunId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<CommandReceipt, ClientError> {
        self.command(
            fresh_command_id()?,
            SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id,
                decision,
            },
        )
        .await
    }

    pub async fn set_profile(
        &self,
        session_id: SessionId,
        profile: AgentProfileId,
    ) -> Result<CommandReceipt, ClientError> {
        self.command(
            fresh_command_id()?,
            SessionCommand::SetSessionProfile {
                session_id,
                profile,
            },
        )
        .await
    }

    /// The server's versioned capability document. Pass a workspace to
    /// include its configured agent profiles.
    pub async fn capabilities(
        &self,
        workspace_id: Option<WorkspaceId>,
    ) -> Result<ServerCapabilities, ClientError> {
        self.post_json(
            "/v1/capabilities",
            &CapabilitiesRequest { workspace_id },
            MAX_CAPABILITIES_BYTES,
        )
        .await
    }

    pub async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, ClientError> {
        self.post_json("/v1/workspaces/snapshot", &request, MAX_SNAPSHOT_BYTES)
            .await
    }

    pub async fn models(
        &self,
        request: ModelCatalogRequest,
    ) -> Result<Vec<ModelDescriptor>, ClientError> {
        self.post_json("/v1/models", &request, MAX_MODEL_CATALOG_BYTES)
            .await
    }

    pub async fn events(
        &self,
        workspace_id: WorkspaceId,
        after: EventCursor,
    ) -> Result<SessionEventStream, ClientError> {
        if after.workspace_id != workspace_id {
            return Err(ClientError::InvalidCursor);
        }
        let endpoint = self
            .connection
            .endpoint(&format!("/v1/workspaces/{workspace_id}/events"));
        let response = time::timeout(
            SSE_HEADER_TIMEOUT,
            authorize(&self.connection, self.http.get(endpoint))
                .header(ACCEPT, "text/event-stream")
                .header(
                    "last-event-id",
                    HeaderValue::from_str(&after.to_string())
                        .map_err(|_| ClientError::InvalidCursor)?,
                )
                .send(),
        )
        .await
        .map_err(|_| ClientError::Unavailable)?
        .map_err(|_| ClientError::Unavailable)?;
        check_success(response.status().as_u16())?;
        if !is_event_stream(response.headers().get(CONTENT_TYPE)) {
            return Err(ClientError::UnexpectedContentType);
        }

        let output = stream! {
            let mut chunks = response.bytes_stream();
            let mut decoder = SseDecoder::<SessionEventEnvelope>::default();
            let mut sequence = after.sequence;
            loop {
                let chunk = match time::timeout(SSE_IDLE_TIMEOUT, chunks.next()).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(_) => {
                        yield Err(ClientError::StreamTransport);
                        return;
                    }
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        yield Err(ClientError::StreamTransport);
                        return;
                    }
                };
                for byte in chunk {
                    match decoder.feed_byte(byte) {
                        Ok(Some(decoded)) => {
                            if !session_event_cursor_is_next(
                                decoded.id.as_deref(),
                                &decoded.event.cursor,
                                workspace_id,
                                after.store_id,
                                sequence,
                            ) {
                                yield Err(ClientError::InvalidCursor);
                                return;
                            }
                            sequence = decoded.event.cursor.sequence;
                            yield Ok(decoded.event);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    }
                }
            }
            match decoder.finish() {
                Ok(Some(decoded)) => {
                    if session_event_cursor_is_next(
                        decoded.id.as_deref(),
                        &decoded.event.cursor,
                        workspace_id,
                        after.store_id,
                        sequence,
                    ) {
                        yield Ok(decoded.event);
                    } else {
                        yield Err(ClientError::InvalidCursor);
                    }
                }
                Ok(None) => {}
                Err(error) => yield Err(error),
            }
        };
        Ok(Box::pin(output))
    }

    pub async fn resolve_workspace(
        &self,
        path: &Path,
    ) -> Result<(WorkspaceId, EventCursor), ClientError> {
        let path = path.to_str().ok_or(ClientError::InvalidWorkspacePath)?;
        let receipt = self
            .command(
                CommandId::generate().map_err(|_| ClientError::Unavailable)?,
                SessionCommand::ResolveWorkspace {
                    path: path.to_owned(),
                },
            )
            .await?;
        let qq_protocol::CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome
        else {
            return Err(ClientError::MalformedEvent);
        };
        Ok((workspace_id, receipt.committed_through))
    }

    async fn post_json<Request, Response>(
        &self,
        path: &str,
        request: &Request,
        response_limit: usize,
    ) -> Result<Response, ClientError>
    where
        Request: serde::Serialize,
        Response: DeserializeOwned,
    {
        let body = serde_json::to_vec(request).map_err(|_| ClientError::InvalidRequestEncoding)?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ClientError::RequestTooLarge);
        }
        let response = authorize(
            &self.connection,
            self.http.post(self.connection.endpoint(path)),
        )
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .body(body)
        .send();
        let response = time::timeout(REQUEST_TIMEOUT, response)
            .await
            .map_err(|_| ClientError::Unavailable)?
            .map_err(|_| ClientError::Unavailable)?;
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > response_limit as u64)
        {
            return Err(ClientError::ResponseTooLarge);
        }
        let bytes = read_response_bounded(response, response_limit)
            .await
            .map_err(|()| ClientError::ResponseTooLarge)?;
        if !(200..300).contains(&status) {
            return Err(server_response_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|_| ClientError::MalformedEvent)
    }
}

fn session_event_cursor_is_next(
    event_id: Option<&str>,
    cursor: &EventCursor,
    workspace_id: WorkspaceId,
    store_id: qq_protocol::StoreId,
    previous_sequence: u64,
) -> bool {
    let expected_id = cursor.to_string();
    event_id == Some(expected_id.as_str())
        && cursor.workspace_id == workspace_id
        && cursor.store_id == store_id
        && previous_sequence.checked_add(1) == Some(cursor.sequence)
}

fn check_success(status: u16) -> Result<(), ClientError> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(ClientError::ServerResponse { status })
    }
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: String,
}

fn server_response_error(status: u16, body: &[u8]) -> ClientError {
    serde_json::from_slice::<ApiErrorBody>(body)
        .ok()
        .filter(|body| !body.error.trim().is_empty())
        .map_or(ClientError::ServerResponse { status }, |body| {
            ClientError::ServerMessage {
                status,
                message: body.error,
            }
        })
}

fn authorize(connection: &Connection, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.bearer_auth(connection.expose_credential())
}

#[cfg(not(target_arch = "wasm32"))]
fn http_client() -> Result<reqwest::Client, ClientError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ClientError::Unavailable)
}

/// The browser owns connection setup, proxies, and redirects; `reqwest`'s
/// wasm builder exposes none of them and `CONNECT_TIMEOUT` is meaningless.
#[cfg(target_arch = "wasm32")]
fn http_client() -> Result<reqwest::Client, ClientError> {
    reqwest::Client::builder()
        .build()
        .map_err(|_| ClientError::Unavailable)
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

fn is_event_stream(value: Option<&reqwest::header::HeaderValue>) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

#[derive(Debug)]
struct DecodedSse<T> {
    id: Option<String>,
    event: T,
}

struct SseDecoder<T> {
    line: Vec<u8>,
    data: Vec<u8>,
    id: Option<String>,
    event_bytes: usize,
    first_line: bool,
    skip_lf: bool,
    marker: PhantomData<T>,
}

impl<T> Default for SseDecoder<T> {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            id: None,
            event_bytes: 0,
            first_line: true,
            skip_lf: false,
            marker: PhantomData,
        }
    }
}

impl<T> SseDecoder<T>
where
    T: DeserializeOwned,
{
    fn feed_byte(&mut self, byte: u8) -> Result<Option<DecodedSse<T>>, ClientError> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }

        match byte {
            b'\r' => {
                self.skip_lf = true;
                self.finish_line()
            }
            b'\n' => self.finish_line(),
            byte => {
                self.event_bytes = self
                    .event_bytes
                    .checked_add(1)
                    .ok_or(ClientError::EventTooLarge)?;
                if self.event_bytes > MAX_SSE_WIRE_EVENT_BYTES {
                    return Err(ClientError::EventTooLarge);
                }
                if self.line.len() >= MAX_SSE_LINE_BYTES {
                    return Err(ClientError::EventTooLarge);
                }
                self.line.push(byte);
                Ok(None)
            }
        }
    }

    fn finish(mut self) -> Result<Option<DecodedSse<T>>, ClientError> {
        let line_event = if self.line.is_empty() {
            None
        } else {
            self.finish_line()?
        };
        if line_event.is_some() {
            return Ok(line_event);
        }
        self.dispatch_event()
    }

    fn finish_line(&mut self) -> Result<Option<DecodedSse<T>>, ClientError> {
        if self.line.is_empty() {
            self.first_line = false;
            self.event_bytes = 0;
            return self.dispatch_event();
        }

        let line = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&line).map_err(|_| ClientError::MalformedSse)?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            self.event_bytes = 0;
            return self.dispatch_event();
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        if field == "data" {
            if self.data.len().saturating_add(value.len()) > MAX_EVENT_BYTES {
                return Err(ClientError::EventTooLarge);
            }
            self.data.extend_from_slice(value.as_bytes());
            self.data.push(b'\n');
        } else if field == "id" {
            if value.len() > 256 || value.as_bytes().contains(&0) {
                return Err(ClientError::MalformedSse);
            }
            self.id = Some(value.to_owned());
        }
        Ok(None)
    }

    fn dispatch_event(&mut self) -> Result<Option<DecodedSse<T>>, ClientError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        self.data.pop();
        let data = std::mem::take(&mut self.data);
        let event = serde_json::from_slice(&data).map_err(|_| ClientError::MalformedEvent)?;
        Ok(Some(DecodedSse {
            id: self.id.take(),
            event,
        }))
    }
}

/// Sanitized HTTP and SSE failures.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClientError {
    #[error("server is unavailable")]
    Unavailable,
    #[error("request cannot be encoded")]
    InvalidRequestEncoding,
    #[error("request exceeds the wire size limit")]
    RequestTooLarge,
    #[error("response exceeds the wire size limit")]
    ResponseTooLarge,
    #[error("workspace path must be valid UTF-8")]
    InvalidWorkspacePath,
    #[error("server returned an invalid event cursor")]
    InvalidCursor,
    #[error("server returned HTTP status {status}")]
    ServerResponse { status: u16 },
    #[error("server rejected the request ({status}): {message}")]
    ServerMessage { status: u16, message: String },
    #[error("server returned an unexpected content type")]
    UnexpectedContentType,
    #[error("server stream failed")]
    StreamTransport,
    #[error("server returned malformed SSE")]
    MalformedSse,
    #[error("server returned a malformed event")]
    MalformedEvent,
    #[error("server event exceeds the wire size limit")]
    EventTooLarge,
    #[error("session {0} does not exist in this workspace")]
    SessionNotInWorkspace(qq_protocol::SessionId),
    #[error("session {0} is a spawned sub-agent session; open its root session instead")]
    SessionIsChild(qq_protocol::SessionId),
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use std::sync::{Arc, Mutex};

    use qq_protocol::{
        CommandOutcome, EventCursor, LocalServerConnection, ModelSelection, PROTOCOL_VERSION,
        ServerInfo, SessionId, StoreId,
    };
    use qq_server::{
        CommandFuture, ModelsFuture, ServerHandle, ServerHandler, ServerOptions, ServerPaths,
        StartOutcome,
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    struct CatalogHandler;

    impl ServerHandler for CatalogHandler {
        fn models(&self, request: ModelCatalogRequest) -> ModelsFuture {
            Box::pin(async move {
                Ok(vec![ModelDescriptor {
                    provider: "openai".to_owned(),
                    model: "gpt-test".to_owned(),
                    name: Some("GPT Test".to_owned()),
                    context_window: Some(128_000),
                    selection: request.selection,
                }])
            })
        }
    }

    struct CommandEchoHandler {
        commands: Arc<Mutex<Vec<CommandRequest>>>,
    }

    impl ServerHandler for CommandEchoHandler {
        fn command(&self, request: CommandRequest) -> CommandFuture {
            self.commands.lock().unwrap().push(request.clone());
            Box::pin(async move {
                Ok(CommandReceipt {
                    command_id: request.command_id,
                    committed_through: EventCursor {
                        store_id: StoreId::from_bytes([1; 16]),
                        workspace_id: WorkspaceId::from_bytes([2; 16]),
                        sequence: 1,
                    },
                    outcome: CommandOutcome::SessionDeleted {
                        session_id: SessionId::from_bytes([3; 16]),
                    },
                })
            })
        }
    }

    async fn start_test_server(handler: Arc<dyn ServerHandler>) -> (TempDir, ServerHandle) {
        let directory = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(directory.path().join("state"));
        let identity =
            qq_server::ServerIdentity::new(StoreId::from_bytes([0xAA; 16]), Some("test"));
        let server = match qq_server::start(handler, identity, ServerOptions::new(paths))
            .await
            .unwrap()
        {
            StartOutcome::Started(server) => server,
            StartOutcome::Existing(_) => panic!("test unexpectedly found an existing server"),
        };
        (directory, server)
    }

    #[tokio::test]
    async fn model_catalog_is_authenticated_and_round_trips() {
        let (_directory, server) = start_test_server(Arc::new(CatalogHandler)).await;
        let client = SessionClient::new(server.connection().clone()).unwrap();
        let selection = ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(100),
            organization: None,
        };
        let models = client
            .models(ModelCatalogRequest {
                workspace: "/test/workspace".to_owned(),
                selection: selection.clone(),
            })
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].selection, selection);
        assert_eq!(models[0].context_window, Some(128_000));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_management_commands_reach_their_routes_and_only_theirs() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let (_directory, server) = start_test_server(Arc::new(CommandEchoHandler {
            commands: Arc::clone(&commands),
        }))
        .await;
        let client = SessionClient::new(server.connection().clone()).unwrap();
        let session_id = SessionId::from_bytes([3; 16]);
        let workspace_id = WorkspaceId::from_bytes([2; 16]);

        for command in [
            SessionCommand::SetSessionModel {
                session_id,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
            },
            SessionCommand::DeleteSession { session_id },
            SessionCommand::PruneSessions { workspace_id },
            SessionCommand::CompactSession { session_id },
        ] {
            let command_id = CommandId::from_bytes([9; 16]);
            let receipt = client.command(command_id, command.clone()).await.unwrap();
            assert_eq!(receipt.command_id, command_id);
            assert_eq!(commands.lock().unwrap().last().unwrap().command, command);
        }

        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = http
            .post(server.connection().endpoint("/v1/sessions/model"))
            .bearer_auth(server.connection().expose_bearer_token())
            .json(&CommandRequest {
                command_id: CommandId::from_bytes([9; 16]),
                command: SessionCommand::DeleteSession { session_id },
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(commands.lock().unwrap().len(), 4);

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounds_non_success_response_bodies() {
        const MAX_ERROR_BODY_FOR_TEST: usize = 32 * 1024;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let raw_server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let body = vec![b'x'; MAX_ERROR_BODY_FOR_TEST];
            let headers = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(headers).await;
            let _ = socket.write_all(&body).await;
        });
        let connection = LocalServerConnection::new(
            address,
            "e".repeat(64),
            ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "test".to_owned(),
                pid: 1,
                server_id: StoreId::from_bytes([0xAA; 16]),
                display_name: "test".to_owned(),
            },
        )
        .unwrap();

        let client = SessionClient::new(connection).unwrap();
        let error = client
            .command(
                CommandId::from_bytes([9; 16]),
                SessionCommand::DeleteSession {
                    session_id: SessionId::from_bytes([3; 16]),
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error, ClientError::ResponseTooLarge);
        raw_server.await.unwrap();
    }
}

/// Decoder and cursor tests that hold on every transport. They run under
/// `cargo test` natively and under `wasm-bindgen-test` on `wasm32`.
#[cfg(test)]
mod decode_tests {
    use qq_protocol::StoreId;
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[derive(Debug, PartialEq, Eq, serde::Deserialize)]
    #[serde(rename_all = "snake_case", tag = "type")]
    enum TestEvent {
        Started,
        Completed,
    }

    fn decode_fragments(fragments: &[&[u8]]) -> Result<Vec<TestEvent>, ClientError> {
        let mut decoder = SseDecoder::<TestEvent>::default();
        let mut events = Vec::new();
        for fragment in fragments {
            for byte in *fragment {
                if let Some(event) = decoder.feed_byte(*byte)? {
                    events.push(event.event);
                }
            }
        }
        if let Some(event) = decoder.finish()? {
            events.push(event.event);
        }
        Ok(events)
    }

    #[test]
    fn decodes_fragmented_crlf_and_multiline_sse() {
        let events = decode_fragments(&[
            b"\xef",
            b"\xbb\xbf: hea",
            b"rtbeat\r",
            b"\ndata: {\"type\":\r\n",
            b"data: \"started\"}\r",
            b"\n\r\ndata: {\"type\":\"completed\"}\n\n",
        ])
        .unwrap();

        assert_eq!(events, vec![TestEvent::Started, TestEvent::Completed]);
    }

    #[test]
    fn accepts_a_final_event_without_a_blank_line() {
        let events = decode_fragments(&[b"data: {\"type\":\"completed\"}"]).unwrap();

        assert_eq!(events, vec![TestEvent::Completed]);
    }

    #[test]
    fn rejects_malformed_json_without_echoing_it() {
        let error = decode_fragments(&[b"data: definitely-secret\n\n"]).unwrap_err();

        assert_eq!(error, ClientError::MalformedEvent);
        assert!(!error.to_string().contains("definitely-secret"));
    }

    #[test]
    fn bounds_sse_lines_and_events() {
        let mut decoder = SseDecoder::<TestEvent>::default();
        for _ in 0..MAX_SSE_LINE_BYTES {
            decoder.feed_byte(b'x').unwrap();
        }

        assert_eq!(
            decoder.feed_byte(b'x').unwrap_err(),
            ClientError::EventTooLarge
        );

        let mut decoder = SseDecoder::<TestEvent> {
            event_bytes: MAX_SSE_WIRE_EVENT_BYTES,
            ..SseDecoder::default()
        };
        assert_eq!(
            decoder.feed_byte(b'x').unwrap_err(),
            ClientError::EventTooLarge
        );
    }

    #[test]
    fn rejects_forward_session_event_cursor_gaps() {
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let store_id = StoreId::from_bytes([2; 16]);
        let mut cursor = EventCursor {
            store_id,
            workspace_id,
            sequence: 11,
        };
        let mut event_id = cursor.to_string();

        assert!(session_event_cursor_is_next(
            Some(&event_id),
            &cursor,
            workspace_id,
            store_id,
            10,
        ));

        cursor.sequence = 12;
        event_id = cursor.to_string();
        assert!(!session_event_cursor_is_next(
            Some(&event_id),
            &cursor,
            workspace_id,
            store_id,
            10,
        ));
    }
}
