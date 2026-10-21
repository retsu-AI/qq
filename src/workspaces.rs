//! Workspace discovery and selection through the existing authenticated server.
use crate::cli::WorkspaceCommand;
use qq_protocol::{SnapshotRequest, WorkspaceCatalogRequest};
use std::io::{self, Write};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("could not discover qq serve: {0}")]
    Discovery(#[from] qq_server::ServerError),
    #[error("no QQ server is running; start qq serve, then retry this command")]
    NoServer,
    #[error("server request failed: {0}")]
    Client(#[from] qq_client::ClientError),
    #[error("the server returned a different workspace or store")]
    IdentityMismatch,
    #[error("could not encode workspace data: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("could not write workspace data: {0}")]
    Output(#[from] io::Error),
}

pub async fn run(command: WorkspaceCommand) -> Result<(), WorkspaceError> {
    let connection = qq_server::discover()
        .await?
        .ok_or(WorkspaceError::NoServer)?;
    let store_id = connection.server_info().server_id;
    let client = qq_client::SessionClient::new(connection)?;
    match command {
        WorkspaceCommand::List { after, limit } => {
            let catalog = client
                .workspaces(WorkspaceCatalogRequest { after, limit })
                .await?;
            write_result(&catalog)?;
        }
        WorkspaceCommand::Show { workspace } => {
            let snapshot = client
                .snapshot(SnapshotRequest::new(workspace, None, 1, 1))
                .await?;
            if snapshot.cursor.store_id != store_id
                || snapshot.cursor.workspace_id != workspace
                || snapshot.workspace.id != workspace
            {
                return Err(WorkspaceError::IdentityMismatch);
            }
            write_result(&snapshot)?;
        }
    }
    Ok(())
}

fn write_result(value: &impl serde::Serialize) -> Result<(), WorkspaceError> {
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}
