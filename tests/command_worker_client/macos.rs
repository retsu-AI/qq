//! Native kernel checks; these run only on macOS, never as a simulated backend.
use super::*;
use std::{os::fd::AsRawFd as _, time::Duration};

#[test]
fn payload_probe() {
    let Some(mode) =
        std::env::args().find_map(|arg| arg.strip_prefix("qq-macos-probe:").map(str::to_owned))
    else {
        return;
    };
    match mode.as_str() {
        "groups" => {
            assert_eq!(
                rustix::process::getpgrp(),
                rustix::process::getppid().unwrap(),
                "the payload must stay in its trusted helper's group"
            );
            assert!(matches!(
                rustix::process::setsid(),
                Err(rustix::io::Errno::PERM)
            ));
            assert!(matches!(
                rustix::process::setpgid(None, None),
                Err(rustix::io::Errno::PERM)
            ));
        }
        "network" => {
            let address = std::fs::read_to_string("tcp-address")
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err()
            );
            assert!(std::os::unix::net::UnixStream::connect("socket").is_err());
        }
        "descriptors" => {
            let fd = std::fs::read_to_string("inherited-fd").unwrap();
            let opened = std::fs::File::open(format!("/dev/fd/{fd}"));
            assert!(
                opened.is_err(),
                "ambient descriptor must be closed, even with host filesystem authority"
            );
        }
        _ => panic!("unknown native probe"),
    }
    std::fs::write("probe-result", "enforced").unwrap();
}

async fn fixture_command(
    workspace: &std::path::Path,
    arguments: serde_json::Value,
    tool: &str,
    isolated: bool,
    restricted: bool,
) -> qq_core::command_owner::CommandResult {
    let root = tempfile::tempdir().unwrap();
    let worker = client::CommandWorker::new(
        root.path().join("worker"),
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
    let workspace = workspace.canonicalize().unwrap();
    let mut read_only = Vec::new();
    if restricted {
        for path in ["/bin", "/usr", "/System/Library"] {
            let path = std::fs::canonicalize(path).unwrap();
            read_only.push(qq_core::command_owner::NativeDirectoryGrant::capture(&path).unwrap());
        }
    }
    let request = NativeCommand {
        id: ToolCallId::generate().unwrap(),
        workspace_identity: qq_core::command_owner::NativeWorkspaceIdentity::capture(&workspace)
            .unwrap(),
        workspace: workspace.clone(),
        tool: tool.to_owned(),
        arguments: arguments.to_string(),
        environment: NativeEnvironment::capture(&[]).unwrap(),
        shell_policy: Default::default(),
        approved_images: vec![],
        network: if isolated {
            qq_core::command_owner::NativeNetwork::Isolated
        } else {
            qq_core::command_owner::NativeNetwork::Host
        },
        filesystem: if restricted {
            qq_core::command_owner::NativeFilesystem::Restricted {
                read_only,
                read_write: vec![
                    qq_core::command_owner::NativeDirectoryGrant::capture(&workspace).unwrap(),
                ],
            }
        } else {
            qq_core::command_owner::NativeFilesystem::Host
        },
        resources: qq_core::command_owner::NativeResources::Host,
        not_after_ms: None,
    };
    worker.submit(request.clone()).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut after = 0;
        loop {
            let snapshot = worker.wait(request.id, after).await.unwrap();
            if snapshot.status == CommandStatus::Finished {
                break snapshot.result.unwrap();
            }
            after = snapshot.version;
        }
    })
    .await;
    worker.stop_spawned().await;
    result.expect("native command must finish without host fallback")
}

fn probe(mode: &str) -> serde_json::Value {
    serde_json::json!({"program":std::env::current_exe().unwrap(),"args":["--exact","macos::payload_probe","--nocapture","--skip",format!("qq-macos-probe:{mode}")]})
}

#[tokio::test]
async fn payload_cannot_leave_the_owned_process_group() {
    let root = tempfile::tempdir().unwrap();
    let result = fixture_command(root.path(), probe("groups"), "exec", false, false).await;
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe-result")).unwrap(),
        "enforced"
    );
}

#[tokio::test]
async fn isolated_network_denies_tcp_and_host_unix_sockets() {
    let root = tempfile::tempdir().unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let _unix = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
    std::fs::write(
        root.path().join("tcp-address"),
        tcp.local_addr().unwrap().to_string(),
    )
    .unwrap();
    let result = fixture_command(root.path(), probe("network"), "exec", true, false).await;
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe-result")).unwrap(),
        "enforced"
    );
}

#[tokio::test]
async fn inherited_file_authority_is_closed_before_payload_execution() {
    let root = tempfile::tempdir().unwrap();
    let secret = tempfile::tempfile().unwrap();
    let inherited = rustix::io::fcntl_dupfd_cloexec(&secret, 1000).unwrap();
    rustix::io::fcntl_setfd(&inherited, rustix::io::FdFlags::empty()).unwrap();
    std::fs::write(
        root.path().join("inherited-fd"),
        inherited.as_raw_fd().to_string(),
    )
    .unwrap();
    let result = fixture_command(root.path(), probe("descriptors"), "exec", false, false).await;
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe-result")).unwrap(),
        "enforced"
    );
}

#[tokio::test]
async fn restricted_filesystem_allows_task_writes_and_denies_outside_reads_and_writes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("task");
    std::fs::create_dir(&workspace).unwrap();
    let secret = root.path().canonicalize().unwrap().join("secret");
    std::fs::write(&secret, "host-secret").unwrap();
    std::os::unix::fs::symlink(&secret, workspace.join("alias")).unwrap();
    let command = format!(
        "printf task > allowed; if cat alias; then exit 91; fi; if printf overwritten > '{}'; then exit 92; fi; printf denied",
        secret.display()
    );
    let result = fixture_command(
        &workspace,
        serde_json::json!({"command":command}),
        "shell",
        true,
        true,
    )
    .await;
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "host-secret");
    assert_eq!(
        std::fs::read_to_string(workspace.join("allowed")).unwrap(),
        "task"
    );
}
