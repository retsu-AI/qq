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
    std::fs::write("probe-group", rustix::process::getpgrp().to_string()).unwrap();
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
            let child = |mode: &str| {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.args([
                    "--exact",
                    "macos::payload_probe",
                    "--nocapture",
                    "--skip",
                    &format!("qq-macos-probe:{mode}"),
                ]);
                command
            };
            let inherited = child("group-child").output().unwrap();
            assert!(
                inherited.status.success(),
                "{}",
                String::from_utf8_lossy(&inherited.stderr)
            );
            let facts: (u32, i32) =
                serde_json::from_slice(&std::fs::read("child-group").unwrap()).unwrap();
            assert_ne!(facts.0 as i32, facts.1);
            assert_eq!(facts.1, rustix::process::getpgrp().as_raw_nonzero().get());
            std::fs::remove_file("child-group").unwrap();
            for session in [false, true] {
                spawned_child_keeps_seatbelt(session);
            }
            let session = child("setsid-child").output().unwrap();
            assert!(
                session.status.success(),
                "{}",
                String::from_utf8_lossy(&session.stderr)
            );
            assert!(matches!(
                rustix::process::setpgid(None, None),
                Err(rustix::io::Errno::PERM)
            ));
        }
        "group-child" => {
            std::fs::write(
                "child-group",
                serde_json::to_vec(&(
                    std::process::id(),
                    rustix::process::getpgrp().as_raw_nonzero().get(),
                ))
                .unwrap(),
            )
            .unwrap();
            return;
        }
        "spawn-pgroup-child" | "spawn-session-child" => {
            let outside = std::fs::read_to_string("outside-path").unwrap();
            for result in [
                std::fs::read(&outside).map(|_| ()),
                std::fs::write(&outside, b"escaped"),
            ] {
                assert_eq!(
                    result.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
            let address = std::fs::read_to_string("tcp-address")
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err()
            );
            let socket = std::fs::read_to_string("unix-address").unwrap();
            assert!(std::os::unix::net::UnixStream::connect(socket).is_err());
            std::fs::write(
                mode.as_str(),
                serde_json::to_vec(&(
                    rustix::process::getpid().as_raw_nonzero().get(),
                    rustix::process::getpgrp().as_raw_nonzero().get(),
                    rustix::process::getsid(None)
                        .unwrap()
                        .as_raw_nonzero()
                        .get(),
                ))
                .unwrap(),
            )
            .unwrap();
            return;
        }
        "setsid-child" => {
            let group = rustix::process::getpgrp();
            assert_ne!(group, rustix::process::getpid());
            assert!(matches!(
                rustix::process::setsid(),
                Err(rustix::io::Errno::PERM)
            ));
            assert_eq!(rustix::process::getpgrp(), group);
            return;
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
        "memory" => {
            println!("native pressure probe started");
            let mut memory = vec![0_u8; 256 * 1024 * 1024];
            for byte in memory.iter_mut().step_by(4096) {
                *byte = 1;
            }
            std::fs::write("pressure-ready", "memory").unwrap();
            std::hint::black_box(&memory);
            loop {
                std::thread::park();
            }
        }
        "threads" => {
            println!("native pressure probe started");
            let mut threads = Vec::new();
            for _ in 0..32 {
                threads.push(std::thread::spawn(|| {
                    loop {
                        std::thread::park();
                    }
                }));
            }
            std::fs::write("pressure-ready", "threads").unwrap();
            std::hint::black_box(&threads);
            loop {
                std::thread::park();
            }
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

// Wait on this exact child before returning. The fallback also reaps it if an
// assertion fails, so this fixture never relies on group cleanup for detachment.
struct SpawnedProbe(nix::unistd::Pid);
impl Drop for SpawnedProbe {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
        loop {
            match nix::sys::wait::waitpid(self.0, None) {
                Err(nix::errno::Errno::EINTR) => continue,
                _ => break,
            }
        }
    }
}

fn spawned_child_keeps_seatbelt(session: bool) {
    use nix::spawn::{PosixSpawnAttr, PosixSpawnFileActions, PosixSpawnFlags, posix_spawn};
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
    let mode = if session {
        "spawn-session-child"
    } else {
        "spawn-pgroup-child"
    };
    let executable = std::env::current_exe().unwrap();
    let executable = CString::new(executable.as_os_str().as_bytes()).unwrap();
    let args = [
        executable.clone(),
        CString::new("--exact").unwrap(),
        CString::new("macos::payload_probe").unwrap(),
        CString::new("--nocapture").unwrap(),
        CString::new("--skip").unwrap(),
        CString::new(format!("qq-macos-probe:{mode}")).unwrap(),
    ];
    let environment = std::env::vars_os()
        .map(|(mut name, value)| {
            name.push("=");
            name.push(value);
            CString::new(name.as_bytes()).unwrap()
        })
        .collect::<Vec<_>>();
    let mut attributes = PosixSpawnAttr::init().unwrap();
    let flags = if session {
        // Darwin's public POSIX_SPAWN_SETSID, absent from nix's named flags.
        // apple-oss-distributions/xnu: bsd/sys/spawn.h
        PosixSpawnFlags::from_bits_retain(0x0400)
    } else {
        attributes
            .set_pgroup(nix::unistd::Pid::from_raw(0))
            .unwrap();
        PosixSpawnFlags::POSIX_SPAWN_SETPGROUP
    };
    attributes.set_flags(flags).unwrap();
    let actions = PosixSpawnFileActions::init().unwrap();
    let child = SpawnedProbe(
        posix_spawn(
            executable.as_c_str(),
            &actions,
            &attributes,
            &args,
            &environment,
        )
        .unwrap(),
    );
    let status = loop {
        match nix::sys::wait::waitpid(child.0, None) {
            Err(nix::errno::Errno::EINTR) => continue,
            result => break result.unwrap(),
        }
    };
    // The PID is no longer ours after the successful reap.
    std::mem::forget(child);
    assert!(
        matches!(status, nix::sys::wait::WaitStatus::Exited(_, 0)),
        "{status:?}"
    );
    let facts: (i32, i32, i32) = serde_json::from_slice(&std::fs::read(mode).unwrap()).unwrap();
    assert_eq!(
        facts.0, facts.1,
        "spawned child must lead a different group"
    );
    assert_ne!(facts.1, rustix::process::getpgrp().as_raw_nonzero().get());
    if session {
        assert_eq!(facts.0, facts.2, "SETSID child must lead its own session");
    } else {
        assert_eq!(
            facts.2,
            rustix::process::getsid(None)
                .unwrap()
                .as_raw_nonzero()
                .get()
        );
    }
}

async fn fixture_command(
    workspace: &std::path::Path,
    arguments: serde_json::Value,
    tool: &str,
    isolated: bool,
    restricted: bool,
    additional_read_only: &[std::path::PathBuf],
) -> qq_core::command_owner::CommandResult {
    fixture_command_with_resources(
        workspace,
        arguments,
        tool,
        isolated,
        restricted,
        additional_read_only,
        qq_core::command_owner::NativeResources::Host,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fixture_command_with_resources(
    workspace: &std::path::Path,
    arguments: serde_json::Value,
    tool: &str,
    isolated: bool,
    restricted: bool,
    additional_read_only: &[std::path::PathBuf],
    resources: qq_core::command_owner::NativeResources,
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
        resources,
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
async fn spawn_attributes_preserve_seatbelt_outside_the_owned_group() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("task");
    std::fs::create_dir(&workspace).unwrap();
    let secret = root.path().canonicalize().unwrap().join("secret");
    std::fs::write(&secret, b"host secret").unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = root.path().join("host.sock");
    let _unix = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap());
    drop(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    std::fs::write(workspace.join("outside-path"), secret.to_str().unwrap()).unwrap();
    std::fs::write(
        workspace.join("tcp-address"),
        tcp.local_addr().unwrap().to_string(),
    )
    .unwrap();
    std::fs::write(workspace.join("unix-address"), socket.to_str().unwrap()).unwrap();
    let executable = std::env::current_exe().unwrap();
    let result = fixture_command(
        &workspace,
        probe("groups"),
        "exec",
        true,
        true,
        &[executable.parent().unwrap().to_owned()],
    )
    .await;
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        std::fs::read_to_string(workspace.join("probe-result")).unwrap(),
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

#[tokio::test]
async fn guarded_commands_complete_and_resource_pressure_stops_the_owned_group() {
    use qq_core::command_owner::NativeResources;
    let refused = tempfile::tempdir().unwrap();
    let result = fixture_command_with_resources(
        refused.path(),
        serde_json::json!({"command":"printf escaped > forbidden"}),
        "shell",
        false,
        false,
        &[],
        NativeResources::Guarded {
            memory_bytes: 256 * 1024 * 1024,
            max_tasks: 16,
        },
    )
    .await;
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("restricted filesystem authority"),
        "{}",
        result.text()
    );
    assert!(!refused.path().join("forbidden").exists());
    let executable = std::env::current_exe().unwrap();
    let executable_directory = executable.parent().unwrap().to_owned();
    for mode in ["groups", "memory", "threads"] {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"host secret").unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = outside.path().join("host.sock");
        let _unix = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap());
        drop(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        std::fs::write(
            workspace.path().join("outside-path"),
            secret.to_str().unwrap(),
        )
        .unwrap();
        std::fs::write(
            workspace.path().join("tcp-address"),
            tcp.local_addr().unwrap().to_string(),
        )
        .unwrap();
        std::fs::write(
            workspace.path().join("unix-address"),
            socket.to_str().unwrap(),
        )
        .unwrap();
        let result = fixture_command_with_resources(
            workspace.path(),
            probe(mode),
            "exec",
            true,
            true,
            std::slice::from_ref(&executable_directory),
            NativeResources::Guarded {
                memory_bytes: 256 * 1024 * 1024,
                max_tasks: 16,
            },
        )
        .await;
        if mode == "groups" {
            assert!(!result.is_error(), "{}", result.text());
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("probe-result")).unwrap(),
                "enforced"
            );
        } else {
            assert!(result.is_error(), "{}", result.text());
            assert!(
                result.text().contains("native pressure probe started"),
                "{}",
                result.text()
            );
            assert!(
                result.text().contains("native resource safeguard"),
                "{}",
                result.text()
            );
            let group = std::fs::read_to_string(workspace.path().join("probe-group"))
                .unwrap()
                .parse::<i32>()
                .unwrap();
            nix::errno::Errno::clear();
            let members =
                libproc::processes::pids_by_type(libproc::processes::ProcFilter::ByProgramGroup {
                    pgrpid: group as u32,
                })
                .unwrap();
            assert!(
                (members.is_empty() || members.len() < members.capacity()) && members.len() <= 4096
            );
            for pid in members {
                match libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid as i32, 1) {
                    Ok(info) => assert!(
                        info.pbi_pgid != group as u32 || info.pbi_status == nix::libc::SZOMB,
                        "resource termination left a live group member: {pid}"
                    ),
                    Err(error) => assert!(
                        matches!(
                            rustix::process::getpgid(Some(
                                rustix::process::Pid::from_raw(pid as i32).unwrap()
                            )),
                            Err(rustix::io::Errno::SRCH)
                        ),
                        "could not prove process exit for {pid}: {error}"
                    ),
                }
            }
        }
    }
}
