//! Credential-free native command ownership and journal recovery.
//! Run with `cargo run -p qq-core --example command_owner`.
#![forbid(unsafe_code)]

use qq_core::{
    ShellPolicy,
    command_owner::{CommandOwner, CommandStatus, NativeCommand},
};
use qq_protocol::ToolCallId;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (root, workspace, workspace_path) = tokio::task::spawn_blocking(|| {
        let root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let workspace_path = std::fs::canonicalize(workspace.path())?;
        Ok::<_, std::io::Error>((root, workspace, workspace_path))
    })
    .await??;
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown").await?;
    let id = ToolCallId::generate()?;
    owner
        .submit(NativeCommand {
            id,
            workspace: workspace_path,
            tool: "shell".to_owned(),
            arguments: r#"{"command":"echo owned"}"#.to_owned(),
            shell_policy: ShellPolicy::default(),
            approved_images: Vec::new(),
            not_after_ms: None,
        })
        .await?;
    let mut snapshot = owner.wait(id, 0).await?;
    while snapshot.status != CommandStatus::Finished {
        snapshot = owner.wait(id, snapshot.version).await?;
    }
    let result = snapshot
        .result
        .as_ref()
        .ok_or("command result is missing")?;
    if result.is_error() {
        return Err(std::io::Error::other(result.text()).into());
    }
    println!("{}", result.text());
    owner.shutdown().await?;
    drop(owner);
    let reopened = CommandOwner::open(directory, "unknown").await?;
    assert_eq!(reopened.snapshot(id).await?, snapshot);
    reopened.shutdown().await?;
    drop(reopened);
    tokio::task::spawn_blocking(move || {
        drop(workspace);
        drop(root);
    })
    .await?;
    Ok(())
}
