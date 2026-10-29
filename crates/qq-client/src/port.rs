use std::{future::Future, sync::Arc};

use qq_protocol::{
    CommandId, CommandReceipt, CommandRequest, ModelDescriptor, ServerCapabilities,
    SessionEventEnvelope, SnapshotRequest, WorkspaceSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequest {
    Command(CommandRequest),
    Snapshot(SnapshotRequest),
    Capabilities,
    Models(qq_protocol::ModelSelection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientUpdate {
    Connection(ConnectionState),
    Snapshot(WorkspaceSnapshot),
    ResetSnapshot(WorkspaceSnapshot),
    Models {
        models: Vec<ModelDescriptor>,
        selected: Option<qq_protocol::ModelSelection>,
    },
    Capabilities(Arc<ServerCapabilities>),
    Event(SessionEventEnvelope),
    CommandResult {
        command_id: CommandId,
        result: Result<CommandReceipt, ClientFailure>,
    },
    SnapshotFailed(ClientFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Replaying,
    Live,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFailure {
    message: String,
}

impl ClientFailure {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

pub trait ClientPort: Send {
    fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure>;

    fn recv(&mut self) -> impl Future<Output = Option<ClientUpdate>> + Send;
}
