use super::*;
use std::io::Read as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn socket_path() -> Option<PathBuf> {
    std::env::args().find_map(|argument| argument.strip_prefix("qq-startup:").map(PathBuf::from))
}

fn connect(path: &std::path::Path, label: u8) -> std::os::unix::net::UnixStream {
    let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    stream.write_all(&[label]).unwrap();
    stream
}

#[test]
fn fixture_paused_monitor() {
    let Some(socket) = socket_path() else {
        return;
    };
    let encoded = std::env::args()
        .find(|arg| arg.starts_with("qq-native-"))
        .unwrap();
    if encoded.starts_with("qq-native-monitor:") {
        let mut stream = connect(&socket, b'm');
        if stream.read_exact(&mut [0]).is_err() {
            return;
        }
    } else {
        std::fs::write(
            socket.parent().unwrap().join("namespace-created"),
            b"started",
        )
        .unwrap();
    }
    let code = crate::command_owner::execute_native_launch(&encoded).unwrap_or(125);
    std::process::exit(i32::from(code));
}

#[test]
fn fixture_startup_owner() {
    let Some(socket) = socket_path() else {
        return;
    };
    let launcher = crate::command_owner::CommandLauncher {
        cgroup_root: None,
        executable: std::env::current_exe().unwrap(),
        arguments: vec![
            "--exact".into(),
            "tools::linux_owner::tests_startup::fixture_paused_monitor".into(),
            "--nocapture".into(),
            "--skip".into(),
            format!("qq-startup:{}", socket.display()).into(),
            "--skip".into(),
        ],
    };
    let directory =
        cap_std::fs::Dir::open_ambient_dir(socket.parent().unwrap(), cap_std::ambient_authority())
            .unwrap();
    let mut payload = tokio::process::Command::new("/bin/sh");
    payload
        .env_clear()
        .args(["-c", "printf executed > forbidden"]);
    let (mut command, startup) = LinuxOwner::discover(launcher)
        .unwrap()
        .wrap(payload, &directory, None, None)
        .unwrap();
    let mut child = command
        .as_std_mut()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    drop(command);
    let mut stream = connect(&socket, b'w');
    let _ = stream.read_exact(&mut [0]);
    drop(startup);
    child.wait().unwrap();
}

#[tokio::test]
async fn owner_loss_before_monitor_arming_never_creates_a_namespace() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("control");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let mut worker = tokio::process::Command::new(std::env::current_exe().unwrap());
    worker
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "--exact",
            "tools::linux_owner::tests_startup::fixture_startup_owner",
            "--nocapture",
            "--skip",
        ])
        .arg(format!("qq-startup:{}", socket.display()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut worker = worker.spawn().unwrap();
    let mut streams = Vec::new();
    let mut monitor = None;
    for _ in 0..2 {
        let (mut stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut label = [0];
        stream.read_exact(&mut label).await.unwrap();
        if label == *b"m" {
            let pid =
                rustix::process::Pid::from_raw(stream.peer_cred().unwrap().pid().unwrap()).unwrap();
            let fd =
                rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap();
            monitor = Some((tokio::io::unix::AsyncFd::new(fd).unwrap(), stream));
        } else {
            assert_eq!(label, *b"w");
            streams.push(stream);
        }
    }
    let (monitor, mut stream) = monitor.unwrap();
    worker.kill().await.unwrap();
    worker.wait().await.unwrap();
    stream.write_all(b"g").await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), monitor.readable())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !root.path().join("namespace-created").exists(),
        "a monitor must reject an owner that died before it armed parent-death custody"
    );
    assert!(!root.path().join("forbidden").exists());
    drop(streams);
}

#[test]
fn fixture_root_payload() {
    if !std::env::args().any(|arg| arg == "qq-root-privacy") {
        return;
    }
    assert_eq!(rustix::process::getuid().as_raw(), 0);
    let capabilities = rustix::thread::capabilities(None).unwrap();
    assert!(capabilities.effective.is_empty());
    assert!(capabilities.permitted.is_empty());
    assert!(capabilities.inheritable.is_empty());
    assert!(rustix::thread::no_new_privs().unwrap());
    assert!(
        File::open(format!(
            "/proc/1/fd/{}",
            std::env::args()
                .find_map(|arg| arg.strip_prefix("qq-private-fd:").map(str::to_owned))
                .unwrap()
        ))
        .is_err(),
        "UID 0 must not bypass supervisor protection"
    );
}

#[test]
fn fixture_root_supervisor() {
    if !std::env::args().any(|arg| arg == "qq-root-privacy") {
        return;
    }
    assert_eq!(rustix::process::getpid().as_raw_nonzero().get(), 1);
    let mut secret = File::from(
        rustix::fs::memfd_create("supervisor-only", rustix::fs::MemfdFlags::CLOEXEC).unwrap(),
    );
    secret.write_all(b"private supervisor fixture").unwrap();
    exec::protect_supervisor().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tools::linux_owner::tests_startup::fixture_root_payload",
            "--nocapture",
            "--skip",
            "qq-root-privacy",
        ])
        .args(["--skip", &format!("qq-private-fd:{}", secret.as_raw_fd())])
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn namespace_root_cannot_regain_capabilities_or_read_the_supervisor() {
    // Map this unprivileged account to namespace UID 0; no host root is used.
    let namespace = LinuxOwner::discover(crate::command_owner::tests::launcher())
        .unwrap()
        .executable;
    let output = tokio::process::Command::new(namespace)
        .env_clear()
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--kill-child=KILL",
            "--mount-proc",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tools::linux_owner::tests_startup::fixture_root_supervisor",
            "--nocapture",
            "--skip",
            "qq-root-privacy",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
