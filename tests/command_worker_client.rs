#![forbid(unsafe_code)]
#![cfg(any(target_os = "linux", target_os = "macos", windows))]
#[path = "../src/command_owner/client.rs"]
mod client;
#[cfg(target_os = "linux")]
#[path = "../src/command_owner/delegation.rs"]
mod delegation;
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
            task_git_call: None,
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
        let mut last = None;
        // Preparation may take up to 120 seconds, including the initial
        // debug-image producer hash. This is an observation safety guard,
        // not an additional latency limit on the worker's startup contract.
        let finished = tokio::time::timeout(std::time::Duration::from_secs(150), async {
            let mut after = 0;
            loop {
                let snapshot = client.wait(request.id, after).await.unwrap();
                last = Some(snapshot.clone());
                if matches!(
                    snapshot.status,
                    CommandStatus::Finished | CommandStatus::Lost
                ) {
                    break snapshot;
                }
                after = snapshot.version;
            }
        })
        .await;
        // Capture the queued/running row and metadata-only phase trace before
        // stopping the worker, so a timeout does not destroy its evidence.
        #[cfg(target_os = "linux")]
        let diagnostics = if finished.is_err() {
            let state = directory.clone();
            let logs = client.diagnostic_directory().join("qq/logs");
            let id = request.id;
            tokio::task::spawn_blocking(move || {
                use std::io::Read as _;
                let row = rusqlite::Connection::open_with_flags(
                    state.join("commands/commands.sqlite3"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .and_then(|connection| {
                    connection.query_row(
                        "SELECT status,version,length(output),receipt IS NOT NULL FROM commands WHERE id=?1",
                        [id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, u64>(1)?,
                                row.get::<_, u64>(2)?,
                                row.get::<_, bool>(3)?,
                            ))
                        },
                    )
                });
                let mut report = format!("journal={row:?}");
                if let Ok(entries) = std::fs::read_dir(logs) {
                    for entry in entries.take(4).flatten() {
                        if let Ok(file) = std::fs::File::open(entry.path()) {
                            let mut text = String::new();
                            let _ = file.take(16 * 1024).read_to_string(&mut text);
                            report.push_str(&text);
                        }
                    }
                }
                report
            })
            .await
            .unwrap()
        } else {
            String::new()
        };
        #[cfg(not(target_os = "linux"))]
        let diagnostics = String::new();
        client.stop_spawned().await;
        let finished = finished.unwrap_or_else(|error| panic!(
            "initial command did not finish: {error}; replace_store={replace_store}; last={last:?}; {diagnostics}; counter={:?}",
            std::fs::read_to_string(workspace.path().join("counter")),
        ));
        assert_eq!(finished.status, CommandStatus::Finished, "{finished:?}");
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
        task_git_call: None,
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "command_worker_client/task_git.rs"]
mod task_git;

#[cfg(target_os = "linux")]
#[path = "command_worker_client/task_git.rs"]
mod task_git;

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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "command_worker_client/runtime.rs"]
mod runtime;

#[cfg(target_os = "macos")]
#[path = "command_worker_client/macos.rs"]
mod macos;

#[cfg(windows)]
#[path = "command_worker_client/windows.rs"]
mod windows;

#[cfg(windows)]
#[path = "../crates/qq-core/src/tools/windows_owner/arguments.rs"]
mod windows_arguments;
