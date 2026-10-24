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
                rustix::process::getpid(),
                "the payload must lead its owned group"
            );
            assert_ne!(
                rustix::process::getpgrp(),
                rustix::process::getppid().unwrap(),
                "the trusted guardian must remain outside the payload group"
            );
            assert!(
                matches!(
                    rustix::process::kill_process(
                        rustix::process::getppid().unwrap(),
                        rustix::process::Signal::KILL
                    ),
                    Err(rustix::io::Errno::PERM)
                ),
                "a payload must not kill its trusted cleanup guardian"
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
        "guardian-loss" => {
            use std::io::{Read as _, Write as _};
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "macos::payload_probe",
                    "--nocapture",
                    "--skip",
                    "qq-macos-probe:descendant-wait",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut stream = std::os::unix::net::UnixStream::connect("parent.sock").unwrap();
            for pid in [
                std::process::id(),
                rustix::process::getppid().unwrap().as_raw_nonzero().get() as u32,
                child.id(),
            ] {
                stream.write_all(&pid.to_le_bytes()).unwrap();
            }
            let mut release = [0];
            let _ = stream.read_exact(&mut release);
            child.wait().unwrap();
        }
        "descendant-wait" => {
            use std::io::{Read as _, Write as _};
            let mut stream = std::os::unix::net::UnixStream::connect("descendant.sock").unwrap();
            stream.write_all(&std::process::id().to_le_bytes()).unwrap();
            let mut release = [0];
            stream.read_exact(&mut release).unwrap();
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
    additional_read_only: &[std::path::PathBuf],
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
        // Modern macOS locates the dyld cache in the OS Cryptex, outside
        // /System/Library. Runtime inputs remain explicit read-only grants.
        for path in ["/bin", "/usr", "/System/Library", "/System/Cryptexes/OS"] {
            let path = std::fs::canonicalize(path).unwrap();
            read_only.push(qq_core::command_owner::NativeDirectoryGrant::capture(&path).unwrap());
        }
    }
    for path in additional_read_only {
        let path = path.canonicalize().unwrap();
        read_only.push(qq_core::command_owner::NativeDirectoryGrant::capture(&path).unwrap());
    }
    let request = NativeCommand {
        task_git_call: None,
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
    let result = fixture_command(root.path(), probe("groups"), "exec", false, false, &[]).await;
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
    let result = fixture_command(root.path(), probe("network"), "exec", true, false, &[]).await;
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
    let result =
        fixture_command(root.path(), probe("descriptors"), "exec", false, false, &[]).await;
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
    let read_only = root.path().canonicalize().unwrap().join("read-only");
    std::fs::create_dir(&read_only).unwrap();
    let shared = read_only.join("shared");
    std::fs::write(&shared, "read-only-input").unwrap();
    let command = format!(
        "printf task > allowed; /bin/ln allowed writable-link || exit 94; if cat alias; then exit 91; fi; if printf overwritten > '{}'; then exit 92; fi; cat '{}' > imported; if /bin/ln '{}' linked; then if printf changed > linked; then exit 93; fi; fi; printf denied",
        secret.display(),
        shared.display(),
        shared.display()
    );
    let result = fixture_command(
        &workspace,
        serde_json::json!({"command":command}),
        "shell",
        true,
        true,
        std::slice::from_ref(&read_only),
    )
    .await;
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "host-secret");
    assert_eq!(std::fs::read_to_string(shared).unwrap(), "read-only-input");
    assert_eq!(
        std::fs::read_to_string(workspace.join("imported")).unwrap(),
        "read-only-input"
    );
    for name in ["allowed", "writable-link"] {
        assert_eq!(
            std::fs::read_to_string(workspace.join(name)).unwrap(),
            "task"
        );
    }
}

#[tokio::test]
async fn worker_death_leaves_the_guardian_alive_until_every_payload_process_exits() {
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
    use tokio::io::AsyncReadExt as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().canonicalize().unwrap();
    let parent = tokio::net::UnixListener::bind(workspace.join("parent.sock")).unwrap();
    let descendant = tokio::net::UnixListener::bind(workspace.join("descendant.sock")).unwrap();
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
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let request = NativeCommand {
            task_git_call: None,
            id: ToolCallId::generate().map_err(std::io::Error::other)?,
            workspace_identity: qq_core::command_owner::NativeWorkspaceIdentity::capture(
                &workspace,
            )?,
            workspace,
            tool: "exec".to_owned(),
            arguments: probe("guardian-loss").to_string(),
            environment: NativeEnvironment::capture(&[]).map_err(std::io::Error::other)?,
            shell_policy: Default::default(),
            approved_images: vec![],
            network: qq_core::command_owner::NativeNetwork::Host,
            filesystem: qq_core::command_owner::NativeFilesystem::Host,
            resources: qq_core::command_owner::NativeResources::Host,
            not_after_ms: None,
        };
        worker
            .submit(request)
            .await
            .map_err(std::io::Error::other)?;
        let ((mut parent, _), (mut descendant, _)) =
            tokio::try_join!(parent.accept(), descendant.accept())?;
        let mut parent_frame = [0; 12];
        let mut descendant_frame = [0; 4];
        parent.read_exact(&mut parent_frame).await?;
        descendant.read_exact(&mut descendant_frame).await?;
        if parent_frame[8..] != descendant_frame {
            return Err(std::io::Error::other(
                "descendant readiness did not match its parent's child",
            ));
        }
        let pids = parent_frame
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
            .collect::<std::collections::BTreeSet<_>>();
        if pids.len() != 3 || pids.contains(&0) {
            return Err(std::io::Error::other(
                "fixture did not identify three live owned processes",
            ));
        }
        let queue = Kqueue::new()?;
        let changes = pids
            .iter()
            .map(|pid| {
                KEvent::new(
                    *pid,
                    EventFilter::EVFILT_PROC,
                    EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
                    FilterFlag::NOTE_EXIT,
                    0,
                    0,
                )
            })
            .collect::<Vec<_>>();
        queue.kevent(
            &changes,
            &mut [],
            Some(nix::libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )?;
        // Each process is still held by a separate private socket. Closing
        // the coordinator cannot accidentally release these fixture waits.
        worker.stop_spawned().await;
        let exited = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut pending = pids;
            let mut events = [KEvent::new(
                0,
                EventFilter::EVFILT_PROC,
                EvFlags::empty(),
                FilterFlag::empty(),
                0,
                0,
            ); 3];
            while !pending.is_empty() {
                let remaining = deadline
                    .checked_duration_since(std::time::Instant::now())
                    .ok_or_else(|| {
                        std::io::Error::other(format!("owned processes did not exit: {pending:?}"))
                    })?;
                let timeout = nix::libc::timespec {
                    tv_sec: remaining.as_secs() as i64,
                    tv_nsec: i64::from(remaining.subsec_nanos()),
                };
                let count = match queue.kevent(&[], &mut events, Some(timeout)) {
                    Ok(count) => count,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(error) => return Err(std::io::Error::from(error)),
                };
                for event in &events[..count] {
                    if event.flags().contains(EvFlags::EV_ERROR)
                        || !event.fflags().contains(FilterFlag::NOTE_EXIT)
                    {
                        return Err(std::io::Error::other(format!(
                            "invalid native process-exit event: {event:?}"
                        )));
                    }
                    pending.remove(&event.ident());
                }
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(std::io::Error::other)?;
        // Release fixture channels even if the old implementation orphaned a
        // descendant. No cleanup ever signals a PID from a terminal receipt.
        drop(parent);
        drop(descendant);
        exited
    })
    .await;
    worker.stop_spawned().await;
    result
        .expect("native worker-loss fixture exceeded its deadline")
        .unwrap();
}
