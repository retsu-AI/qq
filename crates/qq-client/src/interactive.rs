use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use qq_protocol::{
    CommandId, ModelCatalogRequest, ModelSelection, SessionCommand, SessionId, SnapshotRequest,
    WorkspaceId, WorkspaceSnapshot,
};
use tokio::sync::{Semaphore, mpsc};

use crate::{
    ClientError, ClientFailure as TuiClientFailure, ClientPort, ClientRequest, ClientUpdate,
    Connection, ConnectionState, SessionClient,
};

const TUI_REQUEST_CAPACITY: usize = 64;
const TUI_UPDATE_CAPACITY: usize = 256;
const TUI_CONCURRENT_REQUESTS: usize = 8;
/// Other sessions whose bodies the bootstrap snapshot pre-warms alongside the
/// focused one. Kept below the TUI's warm-body limit so nothing is evicted
/// on arrival.
const PREWARM_SESSIONS: usize = 4;

type ReconnectFuture = Pin<Box<dyn Future<Output = Option<Connection>> + Send + 'static>>;
type ConnectionResolver = Arc<dyn Fn() -> ReconnectFuture + Send + Sync + 'static>;

/// Which session the TUI shows first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialSession {
    /// Create a fresh session with this model before the first snapshot. This
    /// is bare `qq`: every launch starts a new conversation, and earlier ones
    /// stay reachable from the session list or by id.
    New(ModelSelection),
    /// Focus this existing session. Rejected at bootstrap when it is not a
    /// session of the workspace, so the TUI never paints a wrong session.
    Open(SessionId),
    /// Focus whatever the workspace already has (newest first) and create
    /// nothing. Used by clients that attach to a server they do not own, and
    /// by every reconnect after the first bootstrap.
    Existing,
}

struct InteractiveSession {
    workspace: PathBuf,
    selection: ModelSelection,
    initial: InitialSession,
    resolve_connection: ConnectionResolver,
}

pub struct TuiClient {
    requests: mpsc::Sender<ClientRequest>,
    updates: mpsc::Receiver<ClientUpdate>,
}

struct BackgroundTask(tokio::task::JoinHandle<()>);

impl BackgroundTask {
    fn replace(&mut self, replacement: Self) {
        *self = replacement;
    }
}

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct StreamConnectionState {
    next_attempt: ConnectionState,
}

impl StreamConnectionState {
    const fn new() -> Self {
        Self {
            next_attempt: ConnectionState::Connecting,
        }
    }

    const fn attempt(&self) -> ConnectionState {
        self.next_attempt
    }

    fn connected(&mut self) -> ConnectionState {
        self.next_attempt = ConnectionState::Replaying;
        ConnectionState::Live
    }
}

impl TuiClient {
    pub fn start<Resolve, ResolveFuture>(
        connection: Connection,
        workspace: PathBuf,
        selection: ModelSelection,
        initial: InitialSession,
        resolve_connection: Resolve,
    ) -> Result<Self, ClientError>
    where
        Resolve: Fn() -> ResolveFuture + Send + Sync + 'static,
        ResolveFuture: Future<Output = Option<Connection>> + Send + 'static,
    {
        let client = SessionClient::new(connection)?;
        let resolve_connection: ConnectionResolver =
            Arc::new(move || Box::pin(resolve_connection()));
        let (request_tx, request_rx) = mpsc::channel(TUI_REQUEST_CAPACITY);
        let (update_tx, update_rx) = mpsc::channel(TUI_UPDATE_CAPACITY);
        tokio::spawn(run_tui_client(
            client,
            InteractiveSession {
                workspace,
                selection,
                initial,
                resolve_connection,
            },
            request_rx,
            update_tx,
        ));
        Ok(Self {
            requests: request_tx,
            updates: update_rx,
        })
    }
}

impl ClientPort for TuiClient {
    fn try_send(&self, request: ClientRequest) -> Result<(), TuiClientFailure> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    TuiClientFailure::new("client request queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => TuiClientFailure::new("client stopped"),
            })
    }

    async fn recv(&mut self) -> Option<ClientUpdate> {
        self.updates.recv().await
    }
}

async fn run_tui_client(
    mut client: SessionClient,
    session: InteractiveSession,
    mut requests: mpsc::Receiver<ClientRequest>,
    updates: mpsc::Sender<ClientUpdate>,
) {
    let InteractiveSession {
        workspace,
        selection,
        initial,
        resolve_connection,
    } = session;
    if updates
        .send(ClientUpdate::Connection(ConnectionState::Connecting))
        .await
        .is_err()
    {
        return;
    }
    let (mut workspace_id, snapshot) = match bootstrap_tui(&client, &workspace, &initial).await {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            send_bootstrap_failure(&updates, error).await;
            return;
        }
    };
    let mut cursor = snapshot.cursor;
    // An attached client with an empty workspace creates one session once
    // the catalog has validated the model. `New` already created; `Open`
    // never creates.
    let create_after_validation =
        initial == InitialSession::Existing && snapshot.sessions.is_empty();
    if updates
        .send(ClientUpdate::Snapshot(snapshot))
        .await
        .is_err()
    {
        return;
    }
    let mut catalog_task = start_tui_model_load(
        client.clone(),
        workspace.clone(),
        workspace_id,
        selection.clone(),
        create_after_validation,
        updates.clone(),
    );

    let request_permits = Arc::new(Semaphore::new(TUI_CONCURRENT_REQUESTS));
    let mut reconnect_delay = Duration::from_millis(50);
    let mut connection_state = StreamConnectionState::new();
    loop {
        if updates
            .send(ClientUpdate::Connection(connection_state.attempt()))
            .await
            .is_err()
        {
            return;
        }
        let mut events = match client.events(workspace_id, cursor).await {
            Ok(events) => events,
            Err(error) => {
                if let Some((recovered_client, recovered_workspace, snapshot)) =
                    recover_tui_client(&client, &workspace, &error, &resolve_connection).await
                {
                    client = recovered_client;
                    workspace_id = recovered_workspace;
                    cursor = snapshot.cursor;
                    catalog_task.replace(start_tui_model_load(
                        client.clone(),
                        workspace.clone(),
                        workspace_id,
                        selection.clone(),
                        snapshot.sessions.is_empty(),
                        updates.clone(),
                    ));
                    reconnect_delay = Duration::from_millis(50);
                    if updates
                        .send(ClientUpdate::ResetSnapshot(snapshot))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                if updates
                    .send(ClientUpdate::Connection(ConnectionState::Offline))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        if updates
            .send(ClientUpdate::Connection(connection_state.connected()))
            .await
            .is_err()
        {
            return;
        }
        let mut reset_error = None;
        loop {
            tokio::select! {
                biased;
                request = requests.recv() => {
                    let Some(request) = request else { return; };
                    dispatch_tui_request(
                        client.clone(),
                        workspace_id,
                        request,
                        Arc::clone(&request_permits),
                        updates.clone(),
                    );
                }
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        reconnect_delay = Duration::from_millis(50);
                        cursor = event.cursor;
                        if updates.send(ClientUpdate::Event(event)).await.is_err() {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        if matches!(error, ClientError::InvalidCursor | ClientError::EventTooLarge) {
                            reset_error = Some(error);
                            break;
                        }
                        if updates
                            .send(ClientUpdate::Connection(ConnectionState::Offline))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
                        break;
                    },
                    None => {
                        if updates
                            .send(ClientUpdate::Connection(ConnectionState::Offline))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
                        break;
                    },
                }
            }
        }
        if let Some(error) = reset_error {
            if let Some((recovered_client, recovered_workspace, snapshot)) =
                recover_tui_client(&client, &workspace, &error, &resolve_connection).await
            {
                client = recovered_client;
                workspace_id = recovered_workspace;
                cursor = snapshot.cursor;
                catalog_task.replace(start_tui_model_load(
                    client.clone(),
                    workspace.clone(),
                    workspace_id,
                    selection.clone(),
                    snapshot.sessions.is_empty(),
                    updates.clone(),
                ));
                reconnect_delay = Duration::from_millis(50);
                if updates
                    .send(ClientUpdate::ResetSnapshot(snapshot))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            if updates
                .send(ClientUpdate::Connection(ConnectionState::Offline))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
        }
    }
}

fn start_tui_model_load(
    client: SessionClient,
    workspace: PathBuf,
    workspace_id: WorkspaceId,
    selection: ModelSelection,
    create_initial_session: bool,
    updates: mpsc::Sender<ClientUpdate>,
) -> BackgroundTask {
    BackgroundTask(tokio::spawn(load_tui_models(
        client,
        workspace,
        workspace_id,
        selection,
        create_initial_session,
        updates,
    )))
}

async fn load_tui_models(
    client: SessionClient,
    workspace: PathBuf,
    workspace_id: WorkspaceId,
    selection: ModelSelection,
    create_initial_session: bool,
    updates: mpsc::Sender<ClientUpdate>,
) {
    // Capabilities ride the same background task as the catalog: neither
    // gates first paint, and both restart together after a recovery. A
    // failure leaves them unadvertised, which the TUI treats as absent. The
    // workspace scope is what makes profiles and skills part of the document.
    if let Ok(capabilities) = client.capabilities(Some(workspace_id)).await
        && updates
            .send(ClientUpdate::Capabilities(Arc::new(capabilities)))
            .await
            .is_err()
    {
        return;
    }
    let Ok(models) = client
        .models(ModelCatalogRequest {
            workspace: workspace.to_string_lossy().into_owned(),
            selection: selection.clone(),
        })
        .await
    else {
        return;
    };
    let selection_is_valid = models
        .iter()
        .any(|model| model.selection.model == selection.model);
    if updates
        .send(ClientUpdate::Models {
            models,
            selected: selection_is_valid.then_some(selection.clone()),
        })
        .await
        .is_err()
        || !create_initial_session
        || !selection_is_valid
    {
        return;
    }
    let Ok(snapshot) = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 1,
        })
        .await
    else {
        return;
    };
    if !snapshot.sessions.is_empty() {
        return;
    }
    let Ok(command_id) = CommandId::generate() else {
        return;
    };
    let Ok(receipt) = client
        .command(
            command_id,
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: selection,
                approval_mode: qq_protocol::ApprovalMode::default(),
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
    else {
        return;
    };
    let qq_protocol::CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
        return;
    };
    let Ok(snapshot) = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 512,
            message_limit: 256,
        })
        .await
    else {
        return;
    };
    let _ = updates.send(ClientUpdate::Snapshot(snapshot)).await;
}

async fn bootstrap_tui(
    client: &SessionClient,
    workspace: &Path,
    initial: &InitialSession,
) -> Result<(WorkspaceId, WorkspaceSnapshot), ClientError> {
    let (workspace_id, _) = client.resolve_workspace(workspace).await?;
    let snapshot = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: Vec::new(),
            session_limit: 512,
            message_limit: 256,
        })
        .await?;
    let focused = match initial {
        InitialSession::New(model) => {
            let receipt = client
                .command(
                    CommandId::generate().map_err(|_| ClientError::Unavailable)?,
                    SessionCommand::CreateSession {
                        workspace_id,
                        parent_id: None,
                        model: model.clone(),
                        approval_mode: qq_protocol::ApprovalMode::default(),
                        profile: qq_protocol::AgentProfileId::default(),
                        correlation: qq_protocol::Correlation::default(),
                    },
                )
                .await?;
            let qq_protocol::CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
                return Err(ClientError::MalformedEvent);
            };
            session_id
        }
        InitialSession::Open(session_id) => {
            // Only root sessions are openable; a sub-agent session is shown
            // under its parent, never as the entry point.
            match snapshot
                .sessions
                .iter()
                .find(|session| session.id == *session_id)
            {
                Some(session) if session.parent_id.is_none() => *session_id,
                Some(_) => return Err(ClientError::SessionIsChild(*session_id)),
                None => return Err(ClientError::SessionNotInWorkspace(*session_id)),
            }
        }
        InitialSession::Existing => match snapshot.sessions.first() {
            Some(session) => session.id,
            None => return Ok((workspace_id, snapshot)),
        },
    };
    // Pre-warm the most recent other sessions so the first few switches in
    // the TUI cost no round trip. Sessions arrive newest-first.
    let include_sessions = snapshot
        .sessions
        .iter()
        .map(|session| session.id)
        .filter(|id| *id != focused)
        .take(PREWARM_SESSIONS)
        .collect();
    let snapshot = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(focused),
            include_sessions,
            session_limit: 512,
            message_limit: 256,
        })
        .await?;
    Ok((workspace_id, snapshot))
}

/// Re-bootstraps after a stream failure. Always [`InitialSession::Existing`]:
/// the first bootstrap already created or opened what was asked for, and a
/// reconnect must neither create a second session nor fail on an id that
/// has since been deleted.
async fn recover_tui_client(
    current: &SessionClient,
    workspace: &Path,
    error: &ClientError,
    resolve_connection: &ConnectionResolver,
) -> Option<(SessionClient, WorkspaceId, WorkspaceSnapshot)> {
    if matches!(
        error,
        ClientError::InvalidCursor
            | ClientError::EventTooLarge
            | ClientError::ServerResponse { status: 400 }
            | ClientError::ServerMessage { status: 400, .. }
    ) && let Ok((workspace_id, snapshot)) =
        bootstrap_tui(current, workspace, &InitialSession::Existing).await
    {
        return Some((current.clone(), workspace_id, snapshot));
    }
    if !matches!(
        error,
        ClientError::Unavailable | ClientError::ServerResponse { status: 401 }
    ) {
        return None;
    }
    let connection = resolve_connection().await?;
    let client = SessionClient::new(connection).ok()?;
    let (workspace_id, snapshot) = bootstrap_tui(&client, workspace, &InitialSession::Existing)
        .await
        .ok()?;
    Some((client, workspace_id, snapshot))
}

fn dispatch_tui_request(
    client: SessionClient,
    workspace_id: WorkspaceId,
    request: ClientRequest,
    permits: Arc<Semaphore>,
    updates: mpsc::Sender<ClientUpdate>,
) {
    let Ok(permit) = permits.try_acquire_owned() else {
        let update = match request {
            ClientRequest::Command(command) => ClientUpdate::CommandResult {
                command_id: command.command_id,
                result: Err(TuiClientFailure::new("too many client requests are active")),
            },
            ClientRequest::Snapshot(_) => ClientUpdate::SnapshotFailed(TuiClientFailure::new(
                "too many client requests are active",
            )),
            // A refresh that cannot run now is simply not delivered: the TUI
            // keeps the document it has and the user can ask again.
            ClientRequest::Capabilities => return,
        };
        let _ = updates.try_send(update);
        return;
    };
    tokio::spawn(async move {
        // A newly created session has an empty transcript by construction, so
        // the TUI adopts it from the receipt alone; no follow-up snapshot.
        let update = match request {
            ClientRequest::Command(command) => ClientUpdate::CommandResult {
                command_id: command.command_id,
                result: client
                    .command(command.command_id, command.command)
                    .await
                    .map_err(|error| TuiClientFailure::new(error.to_string())),
            },
            ClientRequest::Snapshot(request) => match client.snapshot(request).await {
                Ok(snapshot) => ClientUpdate::Snapshot(snapshot),
                Err(error) => {
                    ClientUpdate::SnapshotFailed(TuiClientFailure::new(error.to_string()))
                }
            },
            ClientRequest::Capabilities => match client.capabilities(Some(workspace_id)).await {
                Ok(capabilities) => ClientUpdate::Capabilities(Arc::new(capabilities)),
                Err(_) => return,
            },
        };
        let _permit = permit;
        let _ = updates.send(update).await;
    });
}

async fn send_bootstrap_failure(updates: &mpsc::Sender<ClientUpdate>, error: ClientError) {
    let _ = updates
        .send(ClientUpdate::SnapshotFailed(TuiClientFailure::new(
            error.to_string(),
        )))
        .await;
    let _ = updates
        .send(ClientUpdate::Connection(ConnectionState::Offline))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_connection_state_distinguishes_initial_and_reconnect_attempts() {
        let mut state = StreamConnectionState::new();

        assert_eq!(
            [state.attempt(), state.connected(), state.attempt()],
            [
                ConnectionState::Connecting,
                ConnectionState::Live,
                ConnectionState::Replaying,
            ]
        );
    }
}
