use std::{future::Future, sync::Arc};

use qq_protocol::{
    CommandId, CommandReceipt, CommandRequest, ModelDescriptor, ServerCapabilities,
    SessionEventEnvelope, SnapshotRequest, WorkspaceSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequest {
    Command(CommandRequest),
    Snapshot(SnapshotRequest),
    /// Re-read the workspace-scoped capability document. Profiles and skills
    /// compile lazily from workspace files, so a client asks again after an
    /// edit rather than restarting.
    Capabilities,
    /// Re-read the model catalog for this workspace with `selection` as the
    /// client default, exactly as bootstrap does. Delivered as
    /// [`ClientUpdate::Models`]; a client asks after its configuration
    /// becomes loadable (a trusted project) rather than restarting.
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
    /// The server's workspace-scoped capability document. Arrives once per
    /// connection after bootstrap and again per `ClientRequest::Capabilities`;
    /// absent until then, so the TUI treats steering, profiles, and approval
    /// modes as unavailable. Shared because pickers hold it while it renders.
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
