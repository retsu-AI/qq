use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{
    io::{Read as _, Write as _},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, unix::AsyncFd};

#[test]
fn blocked_command() {
    let arguments = std::env::args().collect::<Vec<_>>();
    let Some(marker) = arguments.windows(2).find_map(|pair| {
        (pair[0] == "--skip" && pair[1].starts_with("qq-client-")).then_some(&pair[1])
    }) else {
        return;
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("executions")
        .unwrap()
        .write_all(b"once\n")
        .unwrap();
    let mut stream = std::os::unix::net::UnixStream::connect(marker).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    stream.write_all(b"ready").unwrap();
    let mut release = [0];
    stream.read_exact(&mut release).unwrap();
    std::fs::write("artifact", "native-once").unwrap();
    println!("native-result");
}

struct Worker(AsyncFd<std::os::fd::OwnedFd>);
impl Worker {
    fn child_of(coordinator: u32) -> Self {
        for task in std::fs::read_dir(format!("/proc/{coordinator}/task")).unwrap() {
            let task = task.unwrap();
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children.split_whitespace() {
                let path = format!("/proc/{child}");
                let Ok(command) = std::fs::read(format!("{path}/cmdline")) else {
                    continue;
                };
                if !command
                    .split(|byte| *byte == 0)
                    .any(|arg| arg == b"_command-owner")
                {
                    continue;
                }
                let before = std::fs::read_to_string(format!("{path}/stat")).unwrap();
                let pid = rustix::process::Pid::from_raw(child.parse().unwrap()).unwrap();
                let fd =
                    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap();
                let after = std::fs::read_to_string(format!("{path}/stat")).unwrap();
                let fields = |stat: &str| {
                    stat.rsplit_once(')')
                        .unwrap()
                        .1
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                };
                let (before, after) = (fields(&before), fields(&after));
                assert_eq!(before[19], after[19]);
                assert_eq!(after[1], coordinator.to_string());
                return Self(AsyncFd::new(fd).unwrap());
            }
        }
        panic!("the real CLI must create an independently owned command worker");
    }
    async fn stop(&self) {
        rustix::process::pidfd_send_signal(self.0.get_ref(), rustix::process::Signal::TERM)
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(20), self.0.readable())
            .await
            .unwrap()
            .unwrap();
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = rustix::process::pidfd_send_signal(self.0.get_ref(), rustix::process::Signal::KILL);
    }
}

fn reply(events: Vec<Value>) -> (u16, Option<&'static str>, Vec<Vec<u8>>) {
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    (200, Some("text/event-stream"), vec![body.into_bytes()])
}

#[tokio::test]
async fn real_cli_returns_piped_output_while_its_command_worker_remains_alive() {
    exercise_cli(false).await;
}

#[tokio::test]
async fn restarted_cli_recovers_the_original_command_result_without_reexecution() {
    exercise_cli(true).await;
}

async fn exercise_cli(restart: bool) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let marker = format!("qq-client-{}", qq_protocol::ToolCallId::generate().unwrap());
    let listener = tokio::net::UnixListener::bind(workspace.join(&marker)).unwrap();
    let arguments = json!({"program":std::env::current_exe().unwrap(),"args":["--exact","cli::blocked_command","--nocapture","--skip",marker]}).to_string();
    let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![
            reply(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":arguments}}),
                completed.clone(),
            ]),
            reply(vec![
                json!({"type":"response.output_text.delta","delta":"finished"}),
                completed,
            ]),
        ],
        Duration::from_secs(60),
    );
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    let mut command = configured_cli(root.path(), &workspace, &config);
    let mut child = command
        .args([
            "run",
            "--approval",
            "full",
            "--format",
            "jsonl",
            "execute the fixture",
        ])
        .spawn()
        .unwrap();
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ready = [0; 5];
    stream.read_exact(&mut ready).await.unwrap();
    assert_eq!(&ready, b"ready");
    let worker = Worker::child_of(child.id().unwrap());
    let output = if restart {
        child.start_kill().unwrap();
        let killed = child.wait_with_output().await.unwrap();
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            killed.status.signal(),
            Some(9),
            "fixture must kill the coordinator with SIGKILL"
        );
        assert!(
            tokio::time::timeout(Duration::ZERO, worker.0.readable())
                .await
                .is_err(),
            "the worker must survive coordinator death"
        );
        let data = root.path().join("data/qq");
        let connection = rusqlite::Connection::open(data.join("sessions.sqlite3")).unwrap();
        let (call, session): (String, String) = connection
            .query_row(
                "SELECT t.id,r.session_id FROM tool_calls t JOIN runs r ON r.id=t.run_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(connection);
        stream.write_all(b"e").await.unwrap();
        let observer = super::client::CommandWorker::new(
            data.join("command-owner-v3"),
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
        let id = call.parse().unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut version = 0;
            loop {
                let observed = super::NativeCommandService::wait(&observer, id, version)
                    .await
                    .unwrap();
                if observed.status == super::CommandStatus::Finished {
                    break;
                }
                version = observed.version;
            }
        })
        .await
        .unwrap();
        let mut resumed = configured_cli(root.path(), &workspace, &config);
        tokio::time::timeout(
            Duration::from_secs(20),
            resumed
                .args([
                    "run",
                    "--session",
                    &session,
                    "--format",
                    "jsonl",
                    "continue the fixture",
                ])
                .output(),
        )
        .await
    } else {
        stream.write_all(b"e").await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await
    };
    assert!(
        tokio::time::timeout(Duration::ZERO, worker.0.readable())
            .await
            .is_err(),
        "the worker must outlive the CLI"
    );
    worker.stop().await;
    let output = output
        .expect("piped CLI streams must close independently of worker lifetime")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(if restart {
            "finished"
        } else {
            "native-result"
        })
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("executions")).unwrap(),
        "once\n",
        "command execution must not be replayed"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifact")).unwrap(),
        "native-once"
    );
    let data = root.path().join("data/qq");
    let calls = |path: &Path, query: &str| {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .prepare(query)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let session = calls(
        &data.join("sessions.sqlite3"),
        "SELECT id FROM tool_calls WHERE state='completed'",
    );
    let worker = calls(
        &data.join("command-owner-v3/commands/commands.sqlite3"),
        "SELECT id FROM retired_commands",
    );
    assert_eq!(session.len(), 1);
    assert_eq!(session, worker);
    assert!(
        calls(
            &data.join("command-owner-v3/commands/commands.sqlite3"),
            "SELECT id FROM commands",
        )
        .is_empty(),
        "the durable session result must release worker observations"
    );
    let connection = rusqlite::Connection::open(data.join("sessions.sqlite3")).unwrap();
    let saved: String = connection
        .query_row(
            "SELECT result FROM tool_calls WHERE id=?1",
            [&session[0]],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        saved.contains("native-result"),
        "reclaimed worker output must remain in the session: {saved}"
    );
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), 2);
    assert!(
        captured[1].body().contains("native-result"),
        "the resumed model must see the original command result"
    );
}

fn configured_cli(home: &Path, workspace: &Path, config: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_qq"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("QQ_CONFIG_CONTENT", config)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, directory) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ] {
        command.env(name, home.join(directory));
    }
    command
}
