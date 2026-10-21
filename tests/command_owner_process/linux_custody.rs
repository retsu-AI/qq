use super::*;
use std::io::{Read as _, Write as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, unix::AsyncFd};

#[test]
fn native_helper_boots_from_a_non_utf8_install_path() {
    use std::os::unix::ffi::OsStringExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root
        .path()
        .join(std::ffi::OsString::from_vec(b"qq-\xff".to_vec()));
    std::fs::copy(env!("CARGO_BIN_EXE_qq"), &executable).unwrap();
    let output = std::process::Command::new(executable)
        .args(["_command-exec", "invalid"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.starts_with("native launch:"), "{error}");
    assert!(!error.contains("panicked"));
}

fn fixture_socket() -> Option<std::path::PathBuf> {
    let arguments = std::env::args().collect::<Vec<_>>();
    let marker = arguments.windows(2).find_map(|pair| {
        (pair[0] == "--skip" && pair[1].starts_with("qq-custody-")).then(|| pair[1].clone())
    })?;
    Some(std::env::current_dir().unwrap().join(marker))
}

fn connect_fixture(label: u8) -> u8 {
    let socket = fixture_socket().unwrap();
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    stream.write_all(&[label]).unwrap();
    let mut release = [0];
    let _ = stream.read_exact(&mut release);
    release[0]
}

// These entry points are inert in an ordinary test run. The command below
// supplies the random socket name through a libtest --skip argument.
#[test]
fn fixture_parent() {
    if fixture_socket().is_none() {
        return;
    }
    let marker = fixture_socket().unwrap().file_name().unwrap().to_owned();
    let mut child = std::process::Command::new("setsid")
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "linux_custody::fixture_descendant",
            "--nocapture",
            "--skip",
        ])
        .arg(marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    if connect_fixture(b'p') == b'e' {
        // Model a command that exits successfully with a detached child alive.
        // The owner, rather than this fixture, must stop that child.
        std::process::exit(0);
    }
    child.wait().unwrap();
}

#[test]
fn fixture_descendant() {
    if fixture_socket().is_some() {
        connect_fixture(b'c');
    }
}

#[test]
fn fixture_supervisor_privacy() {
    if fixture_socket().is_none() {
        return;
    }
    assert!(
        std::fs::File::open("/proc/1/fd/0").is_err(),
        "the command must not read its supervisor's control descriptors"
    );
}

#[tokio::test]
async fn command_cannot_read_its_supervisors_control_descriptors() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut worker, connection) = start(&root.path().join("owner")).await;
    let id = ToolCallId::generate().unwrap();
    let request = NativeCommand {
        environment: qq_core::command_owner::NativeEnvironment::capture(&[]).unwrap(),
        id, workspace: std::fs::canonicalize(workspace.path()).unwrap(), tool: "exec".to_owned(),
        arguments: serde_json::json!({"program": std::env::current_exe().unwrap(), "args": ["--exact", "linux_custody::fixture_supervisor_privacy", "--nocapture", "--skip", format!("qq-custody-{id}")]}).to_string(),
        shell_policy: Default::default(), approved_images: Vec::new(), not_after_ms: None,
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert!(
        client
            .post(format!(
                "http://{}/internal/command-owner/v2/submit",
                connection.address()
            ))
            .bearer_auth(connection.expose_bearer_token())
            .json(&request)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let result = finished(&client, &connection, id).await.result.unwrap();
    worker.kill().await.unwrap();
    worker.wait().await.unwrap();
    assert!(!result.is_error(), "{}", result.text());
}

#[tokio::test]
async fn worker_crash_stops_foreground_and_detached_descendants_before_recovery() {
    exercise_ending(Ending::Crash).await;
}

#[tokio::test]
async fn completion_stops_detached_descendants_before_reporting_finished() {
    exercise_ending(Ending::Complete).await;
}

#[tokio::test]
async fn cancellation_stops_detached_descendants_before_reporting_finished() {
    exercise_ending(Ending::Cancel).await;
}

#[tokio::test]
async fn deadline_stops_detached_descendants_before_reporting_finished() {
    exercise_ending(Ending::Deadline).await;
}

#[derive(Clone, Copy)]
enum Ending {
    Crash,
    Complete,
    Cancel,
    Deadline,
}

async fn exercise_ending(ending: Ending) {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let id = ToolCallId::generate().unwrap();
    let marker = format!("qq-custody-{id}");
    let listener = tokio::net::UnixListener::bind(workspace.path().join(&marker)).unwrap();
    let (mut worker, connection) = start(&directory).await;
    let request = NativeCommand {
        environment: qq_core::command_owner::NativeEnvironment::capture(&[]).unwrap(),
        id,
        workspace: std::fs::canonicalize(workspace.path()).unwrap(),
        tool: "exec".to_owned(),
        arguments: serde_json::json!({
            "program": std::env::current_exe().unwrap(),
            "args": ["--exact", "linux_custody::fixture_parent", "--nocapture", "--skip", &marker]
        })
        .to_string(),
        shell_policy: Default::default(),
        approved_images: Vec::new(),
        not_after_ms: matches!(ending, Ending::Deadline).then(|| {
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
                // Include cold executable hashing and preparation on CI. The
                // fixture stays blocked until this actual worker deadline;
                // completion must still imply both processes have stopped.
                + 30_000
        }),
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert!(
        client
            .post(format!(
                "http://{}/internal/command-owner/v2/submit",
                connection.address()
            ))
            .bearer_auth(connection.expose_bearer_token())
            .json(&request)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let mut streams = Vec::new();
    let mut processes = Vec::new();
    let mut labels = Vec::new();
    for _ in 0..2 {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            snapshot = wait_for(&client, &connection, id, Duration::from_secs(30), |snapshot| snapshot.status == CommandStatus::Finished) => {
                panic!("fixture exited before connecting: {snapshot:?}");
            }
        };
        let (mut stream, _) = accepted;
        let mut label = [0];
        stream.read_exact(&mut label).await.unwrap();
        assert!(matches!(label[0], b'p' | b'c'));
        assert!(!labels.contains(&label[0]));
        labels.push(label[0]);
        let pid =
            rustix::process::Pid::from_raw(stream.peer_cred().unwrap().pid().unwrap()).unwrap();
        let process =
            rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap();
        let process = AsyncFd::new(process).unwrap();
        assert!(
            tokio::time::timeout(Duration::ZERO, process.readable())
                .await
                .is_err(),
            "fixture must still be alive"
        );
        streams.push(stream);
        processes.push(process);
    }
    match ending {
        Ending::Crash => {
            worker.kill().await.unwrap();
            worker.wait().await.unwrap();
        }
        Ending::Complete => {
            let parent = labels.iter().position(|label| *label == b'p').unwrap();
            streams[parent].write_all(b"e").await.unwrap();
        }
        Ending::Deadline => {}
        Ending::Cancel => {
            assert!(
                client
                    .post(format!(
                        "http://{}/internal/command-owner/v2/cancel",
                        connection.address()
                    ))
                    .bearer_auth(connection.expose_bearer_token())
                    .json(&serde_json::json!({"id":id}))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
        }
    }
    if !matches!(ending, Ending::Crash) {
        let terminal = if matches!(ending, Ending::Deadline) {
            wait_for(
                &client,
                &connection,
                id,
                Duration::from_secs(35),
                |snapshot| snapshot.status == CommandStatus::Finished,
            )
            .await
        } else {
            finished(&client, &connection, id).await
        };
        assert_eq!(terminal.status, CommandStatus::Finished);
        assert_eq!(
            terminal.result.as_ref().unwrap().is_error(),
            !matches!(ending, Ending::Complete)
        );
        let mut all_stopped = true;
        for process in &processes {
            all_stopped &= tokio::time::timeout(Duration::ZERO, process.readable())
                .await
                .is_ok();
        }
        for stream in &mut streams {
            let _ = stream.write_all(b"x").await;
        }
        worker.kill().await.unwrap();
        worker.wait().await.unwrap();
        assert!(
            all_stopped,
            "Finished must imply the descendant has already stopped"
        );
        return;
    }
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        for process in &processes {
            let _ = process.readable().await.unwrap();
        }
    })
    .await;
    // Release the old implementation's surviving fixtures through private
    // sockets, never by signalling an integer PID after the crash.
    for stream in &mut streams {
        let _ = stream.write_all(b"x").await;
    }
    let (mut replacement, restored) = start(&directory).await;
    let recovered: CommandSnapshot = client
        .post(format!(
            "http://{}/internal/command-owner/v2/snapshot",
            restored.address()
        ))
        .bearer_auth(restored.expose_bearer_token())
        .json(&serde_json::json!({"id":id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    replacement.kill().await.unwrap();
    replacement.wait().await.unwrap();
    assert_eq!(
        restored.server_info().server_id,
        connection.server_info().server_id
    );
    assert_eq!(recovered.status, CommandStatus::Lost);
    assert!(
        stopped.is_ok(),
        "the kernel must stop both processes when their owner crashes"
    );
}
