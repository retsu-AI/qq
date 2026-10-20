use std::{path::Path, process::Stdio, time::Duration};

use qq_core::command_owner::{CommandSnapshot, CommandStatus, NativeCommand};
use qq_protocol::ToolCallId;
use qq_server::{ServerConnection, ServerPaths};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

async fn start(directory: &Path) -> (Child, ServerConnection) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qq"));
    command.env_clear();
    for name in [
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "HOME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .arg("_command-owner")
        .arg("--state-dir")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(
        line.starts_with("qq command owner listening at "),
        "{line:?}"
    );
    let connection = qq_server::discover_at(&ServerPaths::new(directory))
        .await
        .unwrap()
        .unwrap();
    (child, connection)
}

async fn finished(
    client: &reqwest::Client,
    connection: &ServerConnection,
    id: ToolCallId,
) -> CommandSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut after = 0;
        loop {
            let response = client
                .post(format!(
                    "http://{}/internal/command-owner/v1/wait",
                    connection.address()
                ))
                .bearer_auth(connection.expose_bearer_token())
                .json(&serde_json::json!({"id":id,"after":after}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let snapshot: CommandSnapshot = response.json().await.unwrap();
            if snapshot.status == CommandStatus::Finished {
                break snapshot;
            }
            after = snapshot.version;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn command_process_persists_results_across_owner_restart_without_reexecution() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let (mut child, connection) = start(&directory).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let command = NativeCommand {
        id,
        workspace: std::fs::canonicalize(workspace.path()).unwrap(),
        tool: "shell".to_owned(),
        arguments: r#"{"command":"echo once >> count"}"#.to_owned(),
        shell_policy: Default::default(),
        approved_images: Vec::new(),
        not_after_ms: None,
    };
    let response = client
        .post(format!(
            "http://{}/internal/command-owner/v1/submit",
            connection.address()
        ))
        .bearer_auth(connection.expose_bearer_token())
        .json(&command)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let completed = finished(&client, &connection, id).await;
    assert!(!completed.result.as_ref().unwrap().is_error());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let (mut restarted, connection) = start(&directory).await;
    let recovered = finished(&client, &connection, id).await;
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
    assert_eq!(recovered, completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("count"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["once"]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn detached_command_process_finishes_after_observers_disconnect() {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let release = workspace.path().join("release");
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&release)
            .kill_on_drop(true)
            .status(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success());
    let mut gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap())
        .open(release)
        .unwrap();
    let (mut child, connection) = start(&root.path().join("owner")).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let id = ToolCallId::generate().unwrap();
    let command = NativeCommand {
        id,
        workspace: std::fs::canonicalize(workspace.path()).unwrap(),
        tool: "shell".to_owned(),
        arguments: r#"{"command":"printf x >> count; printf started; read token < release; printf finished"}"#.to_owned(),
        shell_policy: Default::default(),
        approved_images: Vec::new(),
        not_after_ms: None,
    };
    let submit = format!(
        "http://{}/internal/command-owner/v1/submit",
        connection.address()
    );
    assert!(
        client
            .post(&submit)
            .bearer_auth(connection.expose_bearer_token())
            .json(&command)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        let mut after = 0;
        loop {
            let snapshot: CommandSnapshot = client
                .post(format!(
                    "http://{}/internal/command-owner/v1/wait",
                    connection.address()
                ))
                .bearer_auth(connection.expose_bearer_token())
                .json(&serde_json::json!({"id":id,"after":after}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if snapshot.output.contains("started") {
                break snapshot;
            }
            after = snapshot.version;
        }
    })
    .await
    .unwrap();
    assert_eq!(running.status, CommandStatus::Running);
    let mut observer = Box::pin(
        client
            .post(format!(
                "http://{}/internal/command-owner/v1/wait",
                connection.address()
            ))
            .bearer_auth(connection.expose_bearer_token())
            .json(&serde_json::json!({"id":id,"after":running.version}))
            .send(),
    );
    assert!(futures_util::poll!(observer.as_mut()).is_pending());
    drop(observer);
    drop(client);
    assert!(child.try_wait().unwrap().is_none());
    let reconnected = reqwest::Client::builder().no_proxy().build().unwrap();
    assert!(
        reconnected
            .post(&submit)
            .bearer_auth(connection.expose_bearer_token())
            .json(&command)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    gate.write_all(b"continue\n").unwrap();
    let completed = finished(&reconnected, &connection, id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert!(completed.output.ends_with("finished"));
    assert_eq!(completed.process, running.process);
    assert_eq!(std::fs::read(workspace.path().join("count")).unwrap(), b"x");
}

#[cfg(unix)]
#[tokio::test]
async fn interrupting_the_worker_settles_running_commands_before_exit() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let (mut child, connection) = start(&directory).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let id = ToolCallId::generate().unwrap();
    let command = NativeCommand {
        id,
        workspace: std::fs::canonicalize(workspace.path()).unwrap(),
        tool: "shell".to_owned(),
        arguments: r#"{"command":"sleep 300"}"#.to_owned(),
        shell_policy: Default::default(),
        approved_images: Vec::new(),
        not_after_ms: None,
    };
    assert!(
        client
            .post(format!(
                "http://{}/internal/command-owner/v1/submit",
                connection.address()
            ))
            .bearer_auth(connection.expose_bearer_token())
            .json(&command)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let running: CommandSnapshot = client
        .post(format!(
            "http://{}/internal/command-owner/v1/wait",
            connection.address()
        ))
        .bearer_auth(connection.expose_bearer_token())
        .json(&serde_json::json!({"id":id,"after":0}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(running.status, CommandStatus::Running);
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id().unwrap()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!directory.join("server.ron").exists());
    let (mut restarted, connection) = start(&directory).await;
    let terminal = finished(&client, &connection, id).await;
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
    assert!(terminal.result.unwrap().is_error());
    assert_eq!(terminal.process, running.process);
}
