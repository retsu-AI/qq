//! Real resource exhaustion through the production worker's delegation setup.
use super::filesystem::{finish, request};
use qq_core::command_owner::{
    CommandLauncher, CommandOwner, NativeDirectoryGrant, NativeResourceLimits, NativeResources,
};
use std::{
    io::{Read as _, Write as _},
    sync::{Arc, Condvar, Mutex},
};

#[test]
fn resource_probe() {
    let args = std::env::args().collect::<Vec<_>>();
    let Some(marker) = args
        .iter()
        .find(|arg| arg.starts_with("qq-resource-probe="))
    else {
        return;
    };
    let mut socket =
        std::net::TcpStream::connect(marker.trim_start_matches("qq-resource-probe=")).unwrap();
    socket.write_all(b"r").unwrap();
    let mut mode = [0];
    socket.read_exact(&mut mode).unwrap();
    if mode == *b"m" {
        println!("memory-probe-started");
        std::io::stdout().flush().unwrap();
        let mut allocations = Vec::new();
        for _ in 0..32 {
            allocations.push(vec![0x5a_u8; 4 * 1024 * 1024]);
            std::hint::black_box(&allocations);
        }
        println!("memory-probe-exceeded-limit");
        return;
    }
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let mut threads = Vec::new();
    for _ in 0..32 {
        let gate = Arc::clone(&gate);
        match std::thread::Builder::new().spawn(move || {
            let (lock, wake) = &*gate;
            let guard = lock.lock().unwrap();
            drop(wake.wait_while(guard, |done| !*done).unwrap());
        }) {
            Ok(thread) => threads.push(thread),
            Err(error) => {
                assert_eq!(
                    error.raw_os_error(),
                    Some(rustix::io::Errno::AGAIN.raw_os_error())
                );
                println!("native-task-limit-enforced");
                break;
            }
        }
    }
    let exhausted = threads.len() < 32;
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(exhausted, "the payload escaped its aggregate task limit");
}

#[tokio::test]
async fn delegated_resource_scope_bounds_native_tasks_and_memory_and_removes_groups() {
    let delegation = super::delegation::prepare().await.unwrap().path;
    // A dead owner may have left an empty cgroup after an abrupt stop.
    let mut exited = std::process::Command::new("true").spawn().unwrap();
    let old_owner = exited.id();
    assert!(exited.wait().unwrap().success());
    use std::os::unix::fs::MetadataExt as _;
    let namespace = std::fs::metadata("/proc/self/ns/pid").unwrap().ino();
    let stale = delegation.join(format!(
        "qq-native-{namespace}-{old_owner}-{}",
        qq_protocol::ToolCallId::generate().unwrap()
    ));
    std::fs::create_dir(&stale).unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut launcher = CommandLauncher::new(env!("CARGO_BIN_EXE_qq").into());
    launcher.cgroup_root = Some(NativeDirectoryGrant::capture(&delegation).unwrap());
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher)
        .await
        .unwrap();
    let executable = std::env::current_exe().unwrap();
    for mode in *b"pmc" {
        let oom_before = event(&delegation, "memory.events", "oom_kill");
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut command = request(&workspace, "exec", "unused");
        command.resources = NativeResources::Limited {
            limits: NativeResourceLimits {
                memory_bytes: 32 * 1024 * 1024,
                max_processes: 8,
                cpu_millicores: 250,
            },
        };
        if let qq_core::command_owner::NativeFilesystem::Restricted { read_only, .. } =
            &mut command.filesystem
        {
            read_only.push(NativeDirectoryGrant::capture(executable.parent().unwrap()).unwrap());
        }
        command.arguments = serde_json::json!({"program":executable, "args":["--exact", "resources::resource_probe", "--nocapture", "--skip", format!("qq-resource-probe={}", socket.local_addr().unwrap())]}).to_string();
        let id = command.id;
        let execution = finish(&owner, command);
        tokio::pin!(execution);
        let (mut peer, _) = tokio::select! {
            accepted = socket.accept() => accepted.unwrap(),
            finished = &mut execution => panic!("payload did not reach probe: {:?}", finished.result),
        };
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut ready = [0];
        peer.read_exact(&mut ready).await.unwrap();
        assert_eq!(ready, *b"r");
        let groups = std::fs::read_dir(&delegation)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("qq-native-")
                    .then(|| entry.path())
            })
            .collect::<Vec<_>>();
        assert!(!stale.exists(), "an empty dead owner's cgroup was retained");
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        for (name, expected) in [
            ("memory.max", "33554432"),
            ("memory.swap.max", "0"),
            ("memory.oom.group", "1"),
            ("pids.max", "8"),
            ("cpu.max", "25000 100000"),
        ] {
            assert_eq!(
                std::fs::read_to_string(group.join(name)).unwrap().trim(),
                expected
            );
        }
        if mode == b'c' {
            owner.cancel(id).await.unwrap();
        } else {
            peer.write_all(&[mode]).await.unwrap();
        }
        let result = execution.await.result.unwrap();
        if mode == b'p' {
            assert!(!result.is_error(), "{}", result.text());
            assert!(
                result.text().contains("native-task-limit-enforced"),
                "{}",
                result.text()
            );
        } else if mode == b'm' {
            assert!(result.is_error(), "{}", result.text());
            assert!(
                result.text().contains("memory-probe-started"),
                "{}",
                result.text()
            );
            assert!(
                !result.text().contains("memory-probe-exceeded-limit"),
                "{}",
                result.text()
            );
        }
        if mode == b'c' {
            assert!(result.is_error(), "a cancelled command reported success");
        }
        if mode == b'm' {
            assert!(
                event(&delegation, "memory.events", "oom_kill") > oom_before,
                "the kernel did not record an OOM kill"
            );
        }
        assert!(!group.exists(), "finished invocation retained its cgroup");
    }
    owner.shutdown().await.unwrap();
}

fn event(directory: &std::path::Path, file: &str, key: &str) -> u64 {
    let values = std::fs::read_to_string(directory.join(file)).unwrap();
    values
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.parse::<u64>().unwrap())
        })
        .expect("kernel event counter")
}
