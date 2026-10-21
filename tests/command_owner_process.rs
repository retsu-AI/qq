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
    // A clean Linux build captures its executable hash before launch. Keep
    // preparation separate from the command's five-second completion guard;
    // debug executables and shared CI CPUs are substantially slower to hash.
    wait_for(
        client,
        connection,
        id,
        Duration::from_secs(30),
        |snapshot| snapshot.process.is_some() || snapshot.status == CommandStatus::Finished,
    )
    .await;
    wait_for(client, connection, id, Duration::from_secs(5), |snapshot| {
        snapshot.status == CommandStatus::Finished
    })
    .await
}

async fn wait_for(
    client: &reqwest::Client,
    connection: &ServerConnection,
    id: ToolCallId,
    budget: Duration,
    ready: impl Fn(&CommandSnapshot) -> bool,
) -> CommandSnapshot {
    let mut last = None;
    let result = tokio::time::timeout(budget, async {
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
            if ready(&snapshot) {
                break snapshot;
            }
            assert_ne!(snapshot.status, CommandStatus::Lost);
            after = snapshot.version;
            last = Some(snapshot);
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("command did not reach its expected state; last: {last:?}"))
}

#[tokio::test]
async fn command_process_persists_results_across_owner_restart_without_reexecution() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let (mut child, connection) = start(&directory).await;
    let store_id = connection.server_info().server_id;
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
        arguments: r#"{"command":"echo once>>count"}"#.to_owned(),
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
    assert_eq!(connection.server_info().server_id, store_id);
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

#[tokio::test]
async fn a_fatal_journal_failure_stops_the_worker_and_releases_its_instance() {
    for fault in ["write", "snapshot", "submit"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("owner");
        let (mut child, connection) = start(&directory).await;
        let store_id = connection.server_info().server_id;
        let db = rusqlite::Connection::open(directory.join("commands/commands.sqlite3")).unwrap();
        db.execute_batch(if fault == "write" {
            "CREATE TRIGGER fail_admission BEFORE INSERT ON commands
             BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;"
        } else {
            "ALTER TABLE commands RENAME TO hidden_commands"
        })
        .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let id = ToolCallId::generate().unwrap();
        let (operation, request) = match fault {
            "write" => ("cancel", serde_json::json!({"id":id})),
            "snapshot" => ("snapshot", serde_json::json!({"id":id})),
            "submit" => (
                "submit",
                serde_json::to_value(NativeCommand {
                    id,
                    workspace: std::fs::canonicalize(root.path()).unwrap(),
                    tool: "shell".to_owned(),
                    arguments: r#"{"command":"echo forbidden"}"#.to_owned(),
                    shell_policy: Default::default(),
                    approved_images: Vec::new(),
                    not_after_ms: None,
                })
                .unwrap(),
            ),
            _ => unreachable!(),
        };
        let _ = client
            .post(format!(
                "http://{}/internal/command-owner/v1/{operation}",
                connection.address()
            ))
            .bearer_auth(connection.expose_bearer_token())
            .json(&request)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        let stopped = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        if stopped.is_err() {
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
        let status = stopped
            .expect("a failed worker must exit without an OS signal")
            .unwrap();
        assert!(!status.success());
        assert!(!directory.join("server.ron").exists());
        db.execute_batch(if fault == "write" {
            "DROP TRIGGER fail_admission"
        } else {
            "ALTER TABLE hidden_commands RENAME TO commands"
        })
        .unwrap();
        drop(db);
        let (mut replacement, connection) = start(&directory).await;
        assert_eq!(connection.server_info().server_id, store_id);
        replacement.kill().await.unwrap();
        replacement.wait().await.unwrap();
    }
}

async fn invoke_start(directory: &Path) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_qq"))
            .args(["_command-owner", "--state-dir"])
            .arg(directory)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn a_worker_refuses_to_reuse_a_regular_server_even_with_the_same_label() {
    struct Unavailable;
    impl qq_server::ServerHandler for Unavailable {}
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let paths = ServerPaths::new(&directory);
    let reservation = match qq_server::reserve(qq_server::ServerOptions::new(paths.clone()))
        .await
        .unwrap()
    {
        qq_server::ReserveOutcome::Reserved(reservation) => reservation,
        qq_server::ReserveOutcome::Existing(_) => panic!("fixture must reserve a new server"),
    };
    let identity = qq_server::ServerIdentity::new(
        qq_protocol::StoreId::generate().unwrap(),
        Some("qq-command-owner"),
    );
    let server = reservation
        .start(std::sync::Arc::new(Unavailable), identity)
        .unwrap();
    let metadata = tokio::fs::read(directory.join("server.ron")).await.unwrap();
    let output = invoke_start(&directory).await;
    let discovered = qq_server::discover_at(&paths).await.unwrap().unwrap();
    assert_eq!(discovered.server_info(), server.connection().server_info());
    assert_eq!(
        tokio::fs::read(directory.join("server.ron")).await.unwrap(),
        metadata
    );
    server.shutdown().await.unwrap();
    assert!(
        !output.status.success(),
        "a regular server cannot host command execution: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!directory.join("commands").exists());
    let (mut worker, connection) = start(&directory).await;
    let output = invoke_start(&directory).await;
    let reused = qq_server::discover_at(&paths).await.unwrap().unwrap();
    worker.kill().await.unwrap();
    worker.wait().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("qq command owner already running at ")
    );
    assert_eq!(reused.server_info(), connection.server_info());
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
    wait_for(
        &client,
        &connection,
        id,
        Duration::from_secs(30),
        |snapshot| snapshot.process.is_some() || snapshot.status == CommandStatus::Finished,
    )
    .await;
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
async fn stopping_the_worker_settles_running_commands_before_exit() {
    for signal in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
        assert_worker_stops(signal).await;
    }
}

#[cfg(unix)]
async fn assert_worker_stops(signal: rustix::process::Signal) {
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
    rustix::process::kill_process(pid, signal).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    if !status.success() {
        // Clean up only this fixture's live command, captured before its owner
        // was signalled; never use a recovered journal PID for teardown.
        let receipt = serde_json::to_value(running.process.as_ref().unwrap()).unwrap();
        let facts = receipt.get("facts").unwrap_or(&receipt);
        let group =
            rustix::process::Pid::from_raw(i32::try_from(facts["pid"].as_u64().unwrap()).unwrap())
                .unwrap();
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    assert!(status.success(), "worker must settle on {signal:?}");
    assert!(!directory.join("server.ron").exists());
    let (mut restarted, connection) = start(&directory).await;
    let terminal = finished(&client, &connection, id).await;
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
    assert!(terminal.result.unwrap().is_error());
    assert_eq!(terminal.process, running.process);
}
