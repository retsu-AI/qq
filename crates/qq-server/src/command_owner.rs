//! Private transport for the trusted host's independent command process.
//! These endpoints are not part of the public QQ session protocol.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use qq_core::command_owner::{CommandOwner, CommandOwnerError, NativeCommand};
use qq_protocol::ToolCallId;
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::{
    ServerConnection, ServerError, ServerHandle, ServerIdentity, ServerOptions, ServerPaths,
    ServerReservation, api_error,
};

const PREFIX: &str = "/internal/command-owner/v3";
const HEALTH_PATH: &str = "/internal/command-owner/v3/health";
const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

impl ServerReservation {
    /// Starts only the command-owner transport. The caller retains ownership
    /// of the journal and must shut it down after stopping this listener.
    pub fn start_command_owner(self, owner: CommandOwner) -> Result<ServerHandle, ServerError> {
        let identity = ServerIdentity::new(owner.store_id(), Some("qq-command-owner"));
        self.start_with(identity, |connection, _, _| router(owner, connection))
    }
}

/// Discover only a healthy command worker with matching discovery identity.
pub async fn discover_at(paths: &ServerPaths) -> Result<Option<ServerConnection>, ServerError> {
    crate::discover_at_endpoint(paths, HEALTH_PATH).await
}

impl ServerOptions {
    /// Reserve or reuse the private command service, never a session server.
    #[must_use]
    pub fn for_command_owner(paths: ServerPaths) -> Self {
        Self {
            health_path: HEALTH_PATH,
            ..Self::new(paths)
        }
    }
}

#[derive(Clone)]
struct OwnerState {
    owner: CommandOwner,
    connection: ServerConnection,
    requests: Arc<Semaphore>,
    waits: Arc<Semaphore>,
    cancellations: Arc<Semaphore>,
}

fn router(owner: CommandOwner, connection: ServerConnection) -> Router {
    let state = OwnerState {
        owner,
        connection,
        requests: Arc::new(Semaphore::new(128)),
        waits: Arc::new(Semaphore::new(128)),
        cancellations: Arc::new(Semaphore::new(16)),
    };
    Router::new()
        .route(HEALTH_PATH, get(health))
        .route(&format!("{PREFIX}/submit"), post(submit))
        .route(&format!("{PREFIX}/snapshot"), post(snapshot))
        .route(&format!("{PREFIX}/wait"), post(wait))
        .route(&format!("{PREFIX}/cancel"), post(cancel))
        .route(&format!("{PREFIX}/acknowledge"), post(acknowledge))
        .route_layer(middleware::from_fn_with_state(state.clone(), admit))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

async fn admit(State(state): State<OwnerState>, request: Request, next: Next) -> Response {
    if !crate::authorized(request.headers(), &state.connection) {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    if request.uri().path() == HEALTH_PATH {
        return next.run(request).await;
    }
    let slots = match request.uri().path() {
        "/internal/command-owner/v3/wait" => state.waits,
        "/internal/command-owner/v3/cancel" => state.cancellations,
        _ => state.requests,
    };
    let Ok(permit) = slots.try_acquire_owned() else {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "command transport capacity exhausted",
        );
    };
    // Bound request parsing and retained response bodies, including slow
    // observers. Waiters cannot consume cancellation or submission capacity.
    next.run(request).await.map(|body| {
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            let _ = &permit;
            chunk
        }))
    })
}

async fn health(State(state): State<OwnerState>) -> Response {
    if !state.owner.is_available() {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "command owner unavailable");
    }
    Json(state.connection.server_info().clone()).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    id: ToolCallId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    id: ToolCallId,
    after: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledgement {
    ids: Vec<ToolCallId>,
}

async fn acknowledge(
    State(state): State<OwnerState>,
    Json(request): Json<Acknowledgement>,
) -> Response {
    match state.owner.acknowledge(request.ids).await {
        Ok(()) => Json(()).into_response(),
        Err(error) => failure(error),
    }
}

async fn submit(State(state): State<OwnerState>, Json(request): Json<NativeCommand>) -> Response {
    match state.owner.submit(request).await {
        Ok(()) => Json(()).into_response(),
        Err(error) => failure(error),
    }
}

async fn snapshot(State(state): State<OwnerState>, Json(request): Json<Identity>) -> Response {
    match state.owner.snapshot(request.id).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => failure(error),
    }
}

async fn wait(State(state): State<OwnerState>, Json(request): Json<Wait>) -> Response {
    match state.owner.wait(request.id, request.after).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => failure(error),
    }
}

async fn cancel(State(state): State<OwnerState>, Json(request): Json<Identity>) -> Response {
    match state.owner.cancel(request.id).await {
        Ok(()) => Json(()).into_response(),
        Err(error) => failure(error),
    }
}

fn failure(error: CommandOwnerError) -> Response {
    let (status, message) = match error {
        CommandOwnerError::Invalid => (StatusCode::BAD_REQUEST, "invalid native command"),
        CommandOwnerError::Conflict => (StatusCode::CONFLICT, "command identity conflict"),
        CommandOwnerError::NotFound => (StatusCode::NOT_FOUND, "command not found"),
        CommandOwnerError::Retired => (StatusCode::GONE, "command observations acknowledged"),
        CommandOwnerError::Full => (
            StatusCode::TOO_MANY_REQUESTS,
            "command owner capacity exhausted",
        ),
        CommandOwnerError::Unavailable => {
            (StatusCode::SERVICE_UNAVAILABLE, "command owner unavailable")
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "command owner failed"),
    };
    api_error(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReserveOutcome, ServerOptions, ServerPaths};
    use qq_core::command_owner::{CommandSnapshot, CommandStatus, NativeCommand};
    use qq_protocol::ToolCallId;

    fn launcher() -> qq_core::command_owner::CommandLauncher {
        let binary = std::env::current_exe().unwrap();
        qq_core::command_owner::CommandLauncher::new(
            binary
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("examples")
                .join(format!(
                    "command_owner_launcher{}",
                    std::env::consts::EXE_SUFFIX
                )),
        )
    }

    async fn fixture() -> (tempfile::TempDir, CommandOwner, ServerHandle) {
        let root = tempfile::tempdir().unwrap();
        let owner = CommandOwner::open(root.path().join("journal"), "unknown", launcher())
            .await
            .unwrap();
        let reservation = match crate::reserve(ServerOptions::for_command_owner(ServerPaths::new(
            root.path().join("transport"),
        )))
        .await
        .unwrap()
        {
            ReserveOutcome::Reserved(reservation) => reservation,
            ReserveOutcome::Existing(_) => panic!("isolated owner must be new"),
        };
        let server = reservation.start_command_owner(owner.clone()).unwrap();
        (root, owner, server)
    }

    #[tokio::test]
    async fn an_unavailable_owner_is_not_advertised_as_healthy() {
        let (_root, owner, server) = fixture().await;
        let connection = server.connection();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{}{HEALTH_PATH}", connection.address());
        let token = connection.expose_bearer_token();
        let healthy = client.get(&url).bearer_auth(token).send().await.unwrap();
        assert_eq!(healthy.status(), StatusCode::OK);
        owner.shutdown().await.unwrap();
        let unavailable = client.get(&url).bearer_auth(token).send().await.unwrap();
        server.shutdown().await.unwrap();
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_authorized_native_command_is_observed_by_a_new_connection() {
        let (_root, owner, server) = fixture().await;
        let workspace = tempfile::tempdir().unwrap();
        let connection = server.connection().clone();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let request = NativeCommand {
            task_git_call: None,
            environment: qq_core::command_owner::NativeEnvironment::capture(&[]).unwrap(),
            id: ToolCallId::generate().unwrap(),
            workspace_identity: qq_core::command_owner::NativeWorkspaceIdentity::capture(
                &(std::fs::canonicalize(workspace.path()).unwrap()),
            )
            .unwrap(),
            workspace: std::fs::canonicalize(workspace.path()).unwrap(),
            tool: "shell".to_owned(),
            arguments: r#"{"command":"echo reconnected"}"#.to_owned(),
            shell_policy: Default::default(),
            approved_images: Vec::new(),
            network: qq_core::command_owner::NativeNetwork::Host,
            resources: qq_core::command_owner::NativeResources::Host,
            filesystem: qq_core::command_owner::NativeFilesystem::Host,
            not_after_ms: None,
        };
        let base = format!("http://{}/internal/command-owner/v3", connection.address());
        let response = client
            .post(format!("{base}/submit"))
            .bearer_auth(connection.expose_bearer_token())
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        drop(client);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut after = 0;
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let response = client
                    .post(format!("{base}/wait"))
                    .bearer_auth(connection.expose_bearer_token())
                    .json(&serde_json::json!({"id":request.id,"after":after}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                let snapshot: CommandSnapshot = response.json().await.unwrap();
                if snapshot.status == CommandStatus::Finished {
                    break snapshot;
                }
                after = snapshot.version;
            }
        })
        .await
        .unwrap();
        assert!(snapshot.output.contains("reconnected"));
        assert!(!snapshot.result.unwrap().is_error());
        let acknowledged = client
            .post(format!("{base}/acknowledge"))
            .bearer_auth(connection.expose_bearer_token())
            .json(&serde_json::json!({"ids":[request.id]}))
            .send()
            .await
            .unwrap();
        assert_eq!(acknowledged.status(), StatusCode::OK);
        let replay = client
            .post(format!("{base}/submit"))
            .bearer_auth(connection.expose_bearer_token())
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::GONE);
        assert!(owner.is_available());
        server.shutdown().await.unwrap();
        owner.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn private_command_routes_authenticate_before_parsing_or_cancelling() {
        let (_root, owner, server) = fixture().await;
        let connection = server.connection();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let id = ToolCallId::generate().unwrap();
        for path in ["submit", "snapshot", "wait", "cancel", "acknowledge"] {
            let response = client
                .post(format!("http://{}{PREFIX}/{path}", connection.address()))
                .bearer_auth("not-the-owner-token")
                .json(&serde_json::json!({"id":id}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
        assert!(matches!(
            owner.snapshot(id).await,
            Err(CommandOwnerError::NotFound)
        ));
        let response = client
            .post(format!("http://{}{PREFIX}/submit", connection.address()))
            .bearer_auth(connection.expose_bearer_token())
            .header("content-type", "application/json")
            .body(vec![b'x'; MAX_REQUEST_BYTES + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        server.shutdown().await.unwrap();
        owner.shutdown().await.unwrap();
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn saturated_completion_waits_do_not_block_command_cancellation() {
        let (_root, owner, server) = fixture().await;
        let connection = server.connection().clone();
        let workspace = tempfile::tempdir().unwrap();
        let id = ToolCallId::generate().unwrap();
        owner
            .submit(NativeCommand {
                task_git_call: None,
                environment: qq_core::command_owner::NativeEnvironment::capture(&[]).unwrap(),
                id,
                workspace_identity: qq_core::command_owner::NativeWorkspaceIdentity::capture(
                    &(std::fs::canonicalize(workspace.path()).unwrap()),
                )
                .unwrap(),
                workspace: std::fs::canonicalize(workspace.path()).unwrap(),
                tool: "shell".to_owned(),
                arguments: if cfg!(windows) {
                    r#"{"command":"for /L %i in (1,1,2147483647) do @rem waiting"}"#
                } else {
                    r#"{"command":"sleep 300"}"#
                }
                .to_owned(),
                shell_policy: Default::default(),
                approved_images: Vec::new(),
                network: qq_core::command_owner::NativeNetwork::Host,
                resources: qq_core::command_owner::NativeResources::Host,
                filesystem: qq_core::command_owner::NativeFilesystem::Host,
                not_after_ms: None,
            })
            .await
            .unwrap();
        let running = owner.wait(id, 0).await.unwrap();
        assert_eq!(running.status, CommandStatus::Running);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut observers = tokio::task::JoinSet::new();
        for _ in 0..129 {
            let connection = connection.clone();
            let client = client.clone();
            observers.spawn(async move {
                client
                    .post(format!("http://{}{PREFIX}/wait", connection.address()))
                    .bearer_auth(connection.expose_bearer_token())
                    .json(&serde_json::json!({"id":id,"after":running.version}))
                    .send()
                    .await
                    .unwrap()
            });
        }
        let rejected =
            tokio::time::timeout(std::time::Duration::from_secs(10), observers.join_next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client
                .post(format!("http://{}{PREFIX}/cancel", connection.address()))
                .bearer_auth(connection.expose_bearer_token())
                .json(&serde_json::json!({"id":id}))
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(cancelled.status(), StatusCode::OK);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(response) = observers.join_next().await {
                assert_eq!(response.unwrap().status(), StatusCode::OK);
            }
        })
        .await
        .unwrap();
        let finished = owner.snapshot(id).await.unwrap();
        assert_eq!(finished.status, CommandStatus::Finished);
        assert!(finished.result.unwrap().is_error());
        server.shutdown().await.unwrap();
        owner.shutdown().await.unwrap();
    }
}
