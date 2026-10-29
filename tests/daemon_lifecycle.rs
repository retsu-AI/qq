#![forbid(unsafe_code)]
#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);

struct Home {
    _root: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let workspace = home.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        Self {
            _root: root,
            home,
            workspace,
        }
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_qq"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.home)
            .current_dir(&self.workspace)
            .args(args)
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
            command.env(name, self.home.join(directory));
        }
        command
    }

    async fn run(&self, args: &[&str]) -> Output {
        tokio::time::timeout(COMMAND_TIMEOUT, self.command(args).output())
            .await
            .unwrap_or_else(|_| panic!("`qq {}` did not finish", args.join(" ")))
            .unwrap()
    }

    fn server_paths(&self) -> qq_server::ServerPaths {
        qq_server::ServerPaths::new(self.home.join("runtime/qq"))
    }

    fn store_lock(&self) -> PathBuf {
        self.home.join("data/qq/sessions.sqlite3.lock")
    }
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn pid_in(line: &str) -> u32 {
    line.rsplit_once("(pid ")
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("no pid in {line:?}"))
        .parse()
        .unwrap()
}

fn alive(pid: u32) -> bool {
    rustix::process::test_kill_process(
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
    )
    .is_ok()
}

fn store_held(path: &Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

#[tokio::test]
async fn detached_daemon_starts_once_reports_status_and_stops_after_closing_its_store() {
    let home = Home::new();
    let missing = home.run(&["daemon", "status"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&missing.stdout),
        "qq daemon is not running\n"
    );

    let started = stdout(&home.run(&["daemon", "run", "--detach"]).await);
    assert!(
        started.starts_with("qq daemon started at 127.0.0.1:"),
        "{started}"
    );
    let pid = pid_in(started.trim_end());
    assert!(alive(pid));
    let connection = qq_server::discover_at(&home.server_paths())
        .await
        .unwrap()
        .expect("a detached start returns only after the daemon serves");
    assert_eq!(connection.server_info().pid, pid);
    assert!(store_held(&home.store_lock()));

    let again = stdout(&home.run(&["daemon", "run", "--detach"]).await);
    assert_eq!(
        again,
        format!(
            "qq daemon already running at {} (pid {pid})\n",
            connection.address()
        )
    );

    let status = stdout(&home.run(&["daemon", "status"]).await);
    assert!(
        status.starts_with(&format!(
            "qq daemon running at {}\n  pid       {pid}\n",
            connection.address()
        )),
        "{status}"
    );

    let stopped = stdout(&home.run(&["daemon", "stop"]).await);
    assert_eq!(stopped, format!("qq daemon stopped (pid {pid})\n"));
    assert!(!store_held(&home.store_lock()));
    assert!(
        qq_server::discover_at(&home.server_paths())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        stdout(&home.run(&["daemon", "stop"]).await),
        "qq daemon is not running\n"
    );
    let log = std::fs::read_to_string(home.server_paths().directory().join("daemon.log")).unwrap();
    assert!(log.contains("qq daemon listening at 127.0.0.1:"), "{log}");
}

#[tokio::test]
async fn restart_replaces_the_daemon_process_and_keeps_its_store_identity() {
    let home = Home::new();
    let first = pid_in(stdout(&home.run(&["daemon", "run", "--detach"]).await).trim_end());
    let before = qq_server::discover_at(&home.server_paths())
        .await
        .unwrap()
        .unwrap();

    let restarted = stdout(&home.run(&["daemon", "restart"]).await);
    let mut lines = restarted.lines();
    assert_eq!(
        lines.next(),
        Some(format!("qq daemon stopped (pid {first})").as_str())
    );
    let second = pid_in(lines.next().unwrap());
    assert_ne!(first, second);
    let after = qq_server::discover_at(&home.server_paths())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.server_info().pid, second);
    assert_eq!(
        after.server_info().server_id,
        before.server_info().server_id,
        "the restarted daemon serves the same store"
    );
    stdout(&home.run(&["daemon", "stop"]).await);
}

#[tokio::test]
async fn sigterm_stops_a_foreground_daemon_and_releases_the_store() {
    let home = Home::new();
    let mut daemon = home.command(&["daemon", "run"]).spawn().unwrap();
    let mut stdout_lines = tokio::io::BufReader::new(daemon.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(
        COMMAND_TIMEOUT,
        tokio::io::AsyncBufReadExt::read_line(&mut stdout_lines, &mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(line.starts_with("qq daemon listening at "), "{line:?}");
    assert!(store_held(&home.store_lock()));
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(daemon.id().unwrap()).unwrap()).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let exited = tokio::time::timeout(COMMAND_TIMEOUT, daemon.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        exited.status.success(),
        "{}",
        String::from_utf8_lossy(&exited.stderr)
    );
    assert!(!store_held(&home.store_lock()));
    assert!(
        qq_server::discover_at(&home.server_paths())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_interactive_server_refuses_to_be_stopped_by_the_daemon_commands() {
    let home = Home::new();
    let paths = home.server_paths();
    struct Unavailable;
    impl qq_server::ServerHandler for Unavailable {}
    let embedded = match qq_server::start(
        std::sync::Arc::new(Unavailable),
        qq_server::ServerIdentity::new(qq_protocol::StoreId::generate().unwrap(), Some("tui")),
        qq_server::ServerOptions::new(paths),
    )
    .await
    .unwrap()
    {
        qq_server::StartOutcome::Started(server) => server,
        qq_server::StartOutcome::Existing(_) => panic!("fixture must own its server"),
    };
    let refused = home.run(&["daemon", "stop"]).await;
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("the running server belongs to an interactive qq; quit that qq to stop it"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    embedded.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_foreground_host_reports_the_running_daemon_at_once() {
    let home = Home::new();
    let pid = pid_in(stdout(&home.run(&["daemon", "run", "--detach"]).await).trim_end());
    let connection = qq_server::discover_at(&home.server_paths())
        .await
        .unwrap()
        .unwrap();
    for (args, expected) in [
        (
            &["daemon", "run"][..],
            format!("qq daemon already running at {}\n", connection.address()),
        ),
        (
            &["serve"][..],
            format!("qq server already running at {}\n", connection.address()),
        ),
    ] {
        let output = tokio::time::timeout(Duration::from_secs(10), home.command(args).output())
            .await
            .unwrap_or_else(|_| panic!("`qq {}` waited on the running daemon", args.join(" ")))
            .unwrap();
        assert_eq!(stdout(&output), expected);
    }
    assert!(alive(pid));
    stdout(&home.run(&["daemon", "stop"]).await);
}

#[tokio::test]
async fn stop_waits_for_a_host_that_is_already_closing() {
    let home = Home::new();
    let paths = home.server_paths();
    let closing = qq_server::acquire_host_lock(&paths, Duration::ZERO)
        .await
        .unwrap();
    let mut stop = home.command(&["daemon", "stop"]).spawn().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stop.wait())
            .await
            .is_err(),
        "`qq daemon stop` returned while a host was still closing"
    );
    drop(closing);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, stop.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stdout(&output), "qq daemon stopped\n");
}

fn advertise_protocol(home: &Home, protocol: u16) {
    let path = home.server_paths().metadata_file().to_path_buf();
    let current = std::fs::read_to_string(&path).unwrap();
    let ours = format!("protocol_version:{}", qq_protocol::PROTOCOL_VERSION);
    assert!(current.contains(&ours), "{current}");
    std::fs::write(
        &path,
        current.replace(&ours, &format!("protocol_version:{protocol}")),
    )
    .unwrap();
}

#[tokio::test]
async fn restart_replaces_a_daemon_from_another_protocol_version() {
    let home = Home::new();
    let first = pid_in(stdout(&home.run(&["daemon", "run", "--detach"]).await).trim_end());
    let before = qq_server::discover_at(&home.server_paths())
        .await
        .unwrap()
        .unwrap();
    advertise_protocol(&home, qq_protocol::PROTOCOL_VERSION - 1);

    let restarted = stdout(&home.run(&["daemon", "restart"]).await);
    let mut lines = restarted.lines();
    assert_eq!(
        lines.next(),
        Some(format!("qq daemon stopped (pid {first})").as_str())
    );
    let second = pid_in(lines.next().unwrap());
    assert_ne!(first, second);
    assert!(!alive(first));
    let after = qq_server::discover_at(&home.server_paths())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.server_info().pid, second);
    assert_eq!(
        after.server_info().server_id,
        before.server_info().server_id
    );

    advertise_protocol(&home, qq_protocol::PROTOCOL_VERSION + 1);
    assert_eq!(
        stdout(&home.run(&["daemon", "stop"]).await),
        format!("qq daemon stopped (pid {second})\n")
    );
    assert!(!store_held(&home.store_lock()));
    assert!(home.server_paths().metadata_file().exists());
    assert_eq!(
        stdout(&home.run(&["daemon", "stop"]).await),
        "qq daemon is not running\n"
    );
    let started = stdout(&home.run(&["daemon", "run", "--detach"]).await);
    assert!(started.starts_with("qq daemon started at "), "{started}");
    stdout(&home.run(&["daemon", "stop"]).await);
}
