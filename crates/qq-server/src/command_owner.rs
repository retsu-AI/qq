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
    ServerConnection, ServerError, ServerHandle, ServerIdentity, ServerReservation, api_error,
};

const PREFIX: &str = "/internal/command-owner/v1";
const MAX_REQUEST_BYTES: usize = 256 * 1024;

impl ServerReservation {
    /// Starts only the command-owner transport. The caller retains ownership
    /// of the journal and must shut it down after stopping this listener.
    pub fn start_command_owner(self, owner: CommandOwner) -> Result<ServerHandle, ServerError> {
        let identity = ServerIdentity::new(
            qq_protocol::StoreId::generate().map_err(|_| ServerError::RandomnessUnavailable)?,
            Some("qq-command-owner"),
        );
        self.start_with(identity, |connection, _| router(owner, connection))
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
        .route("/v1/health", get(health))
        .route(&format!("{PREFIX}/submit"), post(submit))
        .route(&format!("{PREFIX}/snapshot"), post(snapshot))
        .route(&format!("{PREFIX}/wait"), post(wait))
        .route(&format!("{PREFIX}/cancel"), post(cancel))
        .route_layer(middleware::from_fn_with_state(state.clone(), admit))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

async fn admit(State(state): State<OwnerState>, request: Request, next: Next) -> Response {
    if !crate::authorized(request.headers(), &state.connection) {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    if request.uri().path() == "/v1/health" {
        return next.run(request).await;
    }
    let slots = match request.uri().path() {
        "/internal/command-owner/v1/wait" => state.waits,
        "/internal/command-owner/v1/cancel" => state.cancellations,
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

async fn health(State(state): State<OwnerState>) -> Json<qq_protocol::ServerInfo> {
    Json(state.connection.server_info().clone())
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

    async fn fixture() -> (tempfile::TempDir, CommandOwner, ServerHandle) {
        let root = tempfile::tempdir().unwrap();
        let owner = CommandOwner::open(root.path().join("journal"), "unknown")
            .await
            .unwrap();
        let reservation = match crate::reserve(ServerOptions::new(ServerPaths::new(
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
            id: ToolCallId::generate().unwrap(),
            workspace: std::fs::canonicalize(workspace.path()).unwrap(),
            tool: "shell".to_owned(),
            arguments: r#"{"command":"echo reconnected"}"#.to_owned(),
            shell_policy: Default::default(),
            approved_images: Vec::new(),
            not_after_ms: None,
        };
        let base = format!("http://{}/internal/command-owner/v1", connection.address());
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
        server.shutdown().await.unwrap();
        owner.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn private_command_routes_authenticate_before_parsing_or_cancelling() {
        let (_root, owner, server) = fixture().await;
        let connection = server.connection();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let id = ToolCallId::generate().unwrap();
        for path in ["submit", "snapshot", "wait", "cancel"] {
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
                id,
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
