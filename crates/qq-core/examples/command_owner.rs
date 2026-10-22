//! Credential-free native command ownership and journal recovery.
//! Run with `cargo run -p qq-core --example command_owner`.
#![forbid(unsafe_code)]

use qq_core::{
    ShellPolicy,
    command_owner::{CommandOwner, CommandStatus, NativeCommand},
};
use qq_protocol::ToolCallId;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("_command-exec")) {
        let code = qq_core::command_owner::execute_native_launch(
            std::env::args_os()
                .nth(2)
                .ok_or("missing native launch descriptors")?
                .to_str()
                .ok_or("invalid native launch descriptors")?,
        )?;
        std::process::exit(i32::from(code));
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (root, workspace, workspace_path, workspace_identity) = tokio::task::spawn_blocking(|| {
        let root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let workspace_path = std::fs::canonicalize(workspace.path())?;
        let workspace_identity =
            qq_core::command_owner::NativeWorkspaceIdentity::capture(&workspace_path)?;
        Ok::<_, std::io::Error>((root, workspace, workspace_path, workspace_identity))
    })
    .await??;
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(
        directory.clone(),
        "unknown",
        qq_core::command_owner::CommandLauncher::new(std::env::current_exe()?),
    )
    .await?;
    let id = ToolCallId::generate()?;
    owner
        .submit(NativeCommand {
            environment: qq_core::command_owner::NativeEnvironment::capture(&[])?,
            id,
            workspace_identity,
            workspace: workspace_path,
            tool: "shell".to_owned(),
            arguments: r#"{"command":"echo owned"}"#.to_owned(),
            shell_policy: ShellPolicy::default(),
            approved_images: Vec::new(),
            network: qq_core::command_owner::NativeNetwork::Host,
            resources: qq_core::command_owner::NativeResources::Host,
            filesystem: qq_core::command_owner::NativeFilesystem::Host,
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
    let reopened = CommandOwner::open(
        directory,
        "unknown",
        qq_core::command_owner::CommandLauncher::new(std::env::current_exe()?),
    )
    .await?;
    assert_eq!(reopened.snapshot(id).await?, snapshot);
    // This finite example now discards its observations deliberately. A host
    // retaining task history saves the result before acknowledging it.
    reopened.acknowledge(vec![id]).await?;
    assert!(matches!(
        reopened.snapshot(id).await,
        Err(qq_core::command_owner::CommandOwnerError::Retired)
    ));
    reopened.shutdown().await?;
    drop(reopened);
    tokio::task::spawn_blocking(move || {
        drop(workspace);
        drop(root);
    })
    .await?;
    Ok(())
}
