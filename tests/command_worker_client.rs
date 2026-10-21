#![forbid(unsafe_code)]
#![cfg(any(target_os = "linux", windows))]
#[path = "../src/command_owner/client.rs"]
mod client;
use qq_core::command_owner::{
    CommandOwnerError, CommandStatus, NativeCommand, NativeCommandService, NativeEnvironment,
};
use qq_protocol::ToolCallId;

#[tokio::test]
async fn reconnecting_to_a_restarted_worker_observes_the_same_command_without_reexecution() {
    for replace_store in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let directory = root.path().join("worker");
        let client = client::CommandWorker::new(
            directory.clone(),
            env!("CARGO_BIN_EXE_qq").into(),
            concat!(
                env!("CARGO_PKG_VERSION"),
                "+",
                env!("QQ_BUILD_REVISION"),
                ".",
                env!("QQ_BUILD_DATE")
            ),
        )
        .unwrap();
        let request = NativeCommand {
            environment: NativeEnvironment::capture(&[]).unwrap(),
            id: ToolCallId::generate().unwrap(),
            workspace_identity: qq_core::command_owner::NativeWorkspaceIdentity::capture(
                &(std::fs::canonicalize(workspace.path()).unwrap()),
            )
            .unwrap(),
            workspace: std::fs::canonicalize(workspace.path()).unwrap(),
            tool: "shell".to_owned(),
            arguments: r#"{"command":"echo once>>counter"}"#.to_owned(),
            shell_policy: Default::default(),
            approved_images: Vec::new(),
            network: qq_core::command_owner::NativeNetwork::Host,
            resources: qq_core::command_owner::NativeResources::Host,
            filesystem: qq_core::command_owner::NativeFilesystem::Host,
            not_after_ms: None,
        };
        let (a, b) = tokio::join!(
            client.submit(request.clone()),
            client.submit(request.clone())
        );
        a.unwrap();
        b.unwrap();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let mut after = 0;
            loop {
                let snapshot = client.wait(request.id, after).await.unwrap();
                if snapshot.status == CommandStatus::Finished {
                    break snapshot;
                }
                after = snapshot.version;
            }
        })
        .await
        .unwrap();
        client.stop_spawned().await;
        if replace_store {
            std::fs::rename(
                directory.join("commands"),
                root.path().join("original-commands"),
            )
            .unwrap();
        }
        let observed = client.wait(request.id, 0).await;
        client.stop_spawned().await;
        if replace_store {
            assert!(matches!(observed, Err(CommandOwnerError::Unavailable)));
        } else {
            assert_eq!(observed.unwrap(), finished);
        }
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("counter"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["once"]
        );
    }
}

#[cfg(target_os = "linux")]
#[path = "command_worker_client/cli.rs"]
mod cli;

#[tokio::test]
async fn incompatible_worker_build_is_refused_before_command_admission() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("worker");
    let client = client::CommandWorker::new(
        directory.clone(),
        env!("CARGO_BIN_EXE_qq").into(),
        "incompatible-build",
    )
    .unwrap();
    let request = NativeCommand {
        environment: NativeEnvironment::capture(&[]).unwrap(),
        id: ToolCallId::generate().unwrap(),
        workspace_identity: qq_core::command_owner::NativeWorkspaceIdentity::capture(
            &(std::fs::canonicalize(workspace.path()).unwrap()),
        )
        .unwrap(),
        workspace: std::fs::canonicalize(workspace.path()).unwrap(),
        tool: "shell".to_owned(),
        arguments: r#"{"command":"echo forbidden>counter"}"#.to_owned(),
        shell_policy: Default::default(),
        approved_images: Vec::new(),
        network: qq_core::command_owner::NativeNetwork::Host,
        resources: qq_core::command_owner::NativeResources::Host,
        filesystem: qq_core::command_owner::NativeFilesystem::Host,
        not_after_ms: None,
    };
    let result = client.submit(request).await;
    client.stop_spawned().await;
    assert!(matches!(
        result,
        Err(CommandOwnerError::IncompatibleBuild { .. })
    ));
    assert!(!workspace.path().join("counter").exists());
    let connection =
        rusqlite::Connection::open(directory.join("commands/commands.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM commands", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[cfg(target_os = "linux")]
#[path = "command_worker_client/goal_recovery.rs"]
mod goal_recovery;

#[cfg(target_os = "linux")]
#[path = "command_worker_client/filesystem.rs"]
mod filesystem;

#[cfg(target_os = "linux")]
#[path = "command_worker_client/network.rs"]
mod network;

#[cfg(target_os = "linux")]
#[path = "command_worker_client/resources.rs"]
mod resources;

#[cfg(target_os = "linux")]
#[path = "command_worker_client/kernel_isolation.rs"]
mod kernel_isolation;
