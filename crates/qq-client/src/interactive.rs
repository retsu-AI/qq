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

type ReconnectFuture = Pin<Box<dyn Future<Output = Option<Connection>> + Send + 'static>>;
type ConnectionResolver = Arc<dyn Fn() -> ReconnectFuture + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialSession {
    New(ModelSelection),
    Open(SessionId),
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
    let (mut workspace_id, _) = match client.resolve_workspace(&workspace).await {
        Ok(workspace) => workspace,
        Err(error) => {
            send_bootstrap_failure(&updates, error).await;
            return;
        }
    };
    let (empty_tx, empty_rx) = tokio::sync::oneshot::channel();
    let mut catalog_task = {
        let client = client.clone();
        let workspace = workspace.clone();
        let selection = selection.clone();
        let updates = updates.clone();
        BackgroundTask(tokio::spawn(async move {
            let valid = load_tui_models(
                client.clone(),
                workspace,
                workspace_id,
                selection.clone(),
                updates.clone(),
            )
            .await;
            if valid
                && empty_rx.await.unwrap_or(false)
                && let Some(snapshot) =
                    create_initial_session_if_empty(&client, workspace_id, selection).await
            {
                let _ = updates.send(ClientUpdate::Snapshot(snapshot)).await;
            }
        }))
    };
    let snapshot = match bootstrap_tui(&client, workspace_id, &initial).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            send_bootstrap_failure(&updates, error).await;
            return;
        }
    };
    let create_initial = initial == InitialSession::Existing && snapshot.sessions.is_empty();
    let mut cursor = snapshot.cursor;
    if updates
        .send(ClientUpdate::Snapshot(snapshot))
        .await
        .is_err()
    {
        return;
    }
    let _ = empty_tx.send(create_initial);

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
                        workspace.clone(),
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
    create_initial: bool,
    updates: mpsc::Sender<ClientUpdate>,
) -> BackgroundTask {
    BackgroundTask(tokio::spawn(async move {
        let valid = load_tui_models(
            client.clone(),
            workspace,
            workspace_id,
            selection.clone(),
            updates.clone(),
        )
        .await;
        if valid
            && create_initial
            && let Some(snapshot) =
                create_initial_session_if_empty(&client, workspace_id, selection).await
        {
            let _ = updates.send(ClientUpdate::Snapshot(snapshot)).await;
        }
    }))
}

async fn load_tui_models(
    client: SessionClient,
    workspace: PathBuf,
    workspace_id: WorkspaceId,
    selection: ModelSelection,
    updates: mpsc::Sender<ClientUpdate>,
) -> bool {
    let capabilities = client.capabilities(Some(workspace_id));
    let models = client.models(ModelCatalogRequest {
        workspace: workspace.to_string_lossy().into_owned(),
        selection: selection.clone(),
    });
    let (capabilities, models) = tokio::join!(capabilities, models);
    if let Ok(capabilities) = capabilities
        && updates
            .send(ClientUpdate::Capabilities(Arc::new(capabilities)))
            .await
            .is_err()
    {
        return false;
    }
    let Ok(models) = models else {
        return false;
    };
    let selection_is_valid = models
        .iter()
        .any(|model| model.selection.model == selection.model);
    if updates
        .send(ClientUpdate::Models {
            models,
            selected: selection_is_valid.then_some(selection),
        })
        .await
        .is_err()
    {
        return false;
    }
    selection_is_valid
}

async fn create_initial_session_if_empty(
    client: &SessionClient,
    workspace_id: WorkspaceId,
    selection: ModelSelection,
) -> Option<WorkspaceSnapshot> {
    let snapshot = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 1,
        })
        .await
        .ok()?;
    if !snapshot.sessions.is_empty() {
        return None;
    }
    let command_id = CommandId::generate().ok()?;
    let receipt = client
        .command(
            command_id,
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: selection,
                approval_mode: qq_protocol::ApprovalMode::default(),
                profile: qq_protocol::AgentProfileId::default(),
                reasoning_effort: None,
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
        .ok()?;
    let qq_protocol::CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
        return None;
    };
    client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 512,
            message_limit: 256,
        })
        .await
        .ok()
}

async fn bootstrap_tui(
    client: &SessionClient,
    workspace_id: WorkspaceId,
    initial: &InitialSession,
) -> Result<WorkspaceSnapshot, ClientError> {
    let focused_session_id = match initial {
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
                        reasoning_effort: None,
                        correlation: qq_protocol::Correlation::default(),
                    },
                )
                .await?;
            let qq_protocol::CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
                return Err(ClientError::MalformedEvent);
            };
            Some(session_id)
        }
        InitialSession::Open(session_id) => Some(*session_id),
        InitialSession::Existing => None,
    };
    let mut snapshot = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id,
            include_sessions: Vec::new(),
            session_limit: 512,
            message_limit: 256,
        })
        .await
        .map_err(|error| match (initial, &error) {
            (InitialSession::Open(session_id), ClientError::ServerMessage { status: 400, .. }) => {
                ClientError::SessionNotInWorkspace(*session_id)
            }
            _ => error,
        })?;
    if *initial == InitialSession::Existing
        && snapshot.focused.is_none()
        && let Some(session_id) = snapshot
            .sessions
            .iter()
            .find(|session| {
                session.parent_id.is_none() && session.purpose != qq_protocol::SessionPurpose::Audit
            })
            .map(|session| session.id)
    {
        snapshot = client
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                include_sessions: snapshot
                    .sessions
                    .iter()
                    .filter(|session| {
                        session.parent_id.is_none()
                            && session.purpose != qq_protocol::SessionPurpose::Audit
                            && session.id != session_id
                    })
                    .take(4)
                    .map(|session| session.id)
                    .collect(),
                session_limit: 512,
                message_limit: 256,
            })
            .await?;
    }
    if let InitialSession::Open(session_id) = initial {
        match snapshot
            .sessions
            .iter()
            .find(|session| session.id == *session_id)
        {
            Some(session) if session.parent_id.is_none() => {}
            Some(_) => return Err(ClientError::SessionIsChild(*session_id)),
            None => return Err(ClientError::SessionNotInWorkspace(*session_id)),
        }
    }
    Ok(snapshot)
}

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
    ) && let Ok(snapshot) = bootstrap_tui(
        current,
        current.resolve_workspace(workspace).await.ok()?.0,
        &InitialSession::Existing,
    )
    .await
    {
        return Some((current.clone(), snapshot.workspace.id, snapshot));
    }
    if !matches!(
        error,
        ClientError::Unavailable | ClientError::ServerResponse { status: 401 }
    ) {
        return None;
    }
    let connection = resolve_connection().await?;
    let client = SessionClient::new(connection).ok()?;
    let workspace_id = client.resolve_workspace(workspace).await.ok()?.0;
    let snapshot = bootstrap_tui(&client, workspace_id, &InitialSession::Existing)
        .await
        .ok()?;
    Some((client, workspace_id, snapshot))
}

fn dispatch_tui_request(
    client: SessionClient,
    workspace: PathBuf,
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
            ClientRequest::Capabilities | ClientRequest::Models(_) => return,
        };
        let _ = updates.try_send(update);
        return;
    };
    tokio::spawn(async move {
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
            ClientRequest::Models(selection) => {
                let request = ModelCatalogRequest {
                    workspace: workspace.to_string_lossy().into_owned(),
                    selection: selection.clone(),
                };
                match client.models(request).await {
                    Ok(models) => {
                        let selected = models
                            .iter()
                            .any(|model| model.selection.model == selection.model)
                            .then_some(selection);
                        ClientUpdate::Models { models, selected }
                    }
                    Err(_) => return,
                }
            }
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
