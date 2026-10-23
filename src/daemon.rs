//! The per-user daemon: one long-lived `qq` process that owns the session
//! store and serves every client. `qq daemon run` hosts it in the
//! foreground or, with `--detach`, starts it in the background and returns
//! once it serves; `status`, `stop` and `restart` act on whichever daemon is
//! running. `qq serve` is the same host without the background start.
//!
//! A host stops when it is asked to over the loopback API
//! (`POST /v1/daemon/shutdown`), or on SIGTERM, SIGINT or Ctrl-C. Stopping
//! settles every run first: queued and running work is cancelled and
//! recorded, exactly as a server that loses its client would have done.

use std::{
    error::Error,
    io,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    sync::Arc,
    time::Duration,
};

use qq_host::runtime;
use qq_server as server;

use crate::{CliOverrides, cli, config, reject_read_approvals_for_existing_server};

/// How long `--detach` waits for the background daemon to serve. Opening the
/// store and recovering interrupted runs happen before it does.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `stop` waits for a daemon to exit after it accepted the request.
/// Covers the runtime's own 30 s settlement bound plus closing the store.
const STOP_TIMEOUT: Duration = Duration::from_secs(45);
/// How long a new host waits for a previous one that is still closing.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(45);
/// Probe interval while a detached daemon starts.
const START_POLL: Duration = Duration::from_millis(25);
/// Name of the background daemon's log in the server state directory.
const LOG_FILE_NAME: &str = "daemon.log";

#[derive(Debug, Clone)]
pub struct Listen {
    pub bind: std::net::SocketAddr,
    pub allow_origins: Vec<String>,
}

impl From<cli::DaemonListenArgs> for Listen {
    fn from(args: cli::DaemonListenArgs) -> Self {
        Self {
            bind: args.bind,
            allow_origins: args.allow_origins,
        }
    }
}

pub async fn command(
    command: cli::DaemonCommand,
    overrides: &CliOverrides,
) -> Result<ExitCode, Box<dyn Error>> {
    let paths = server::ServerPaths::for_user()?;
    match command {
        cli::DaemonCommand::Run(args) if args.detach => {
            let listen = Listen::from(args.listen);
            match start_detached(&paths, &listen, overrides).await? {
                Started::New(connection) => println!(
                    "qq daemon started at {} (pid {})",
                    connection.address(),
                    connection.server_info().pid
                ),
                Started::Existing(connection) => {
                    reject_read_approvals_for_existing_server(&overrides.approved_read_images)?;
                    println!(
                        "qq daemon already running at {} (pid {})",
                        connection.address(),
                        connection.server_info().pid
                    );
                }
            }
        }
        cli::DaemonCommand::Run(args) => {
            host(&paths, &Listen::from(args.listen), overrides, Host::Daemon).await?;
        }
        cli::DaemonCommand::Status => match server::discover_at(&paths).await? {
            Some(connection) => {
                let info = connection.server_info();
                println!("qq daemon running at {}", connection.address());
                println!("  pid       {}", info.pid);
                println!("  version   {}", info.version);
                println!("  protocol  {}", info.protocol_version);
                println!("  server id {}", info.server_id);
                println!("  log       {}", log_path(&paths).display());
            }
            None => {
                println!("qq daemon is not running");
                return Ok(ExitCode::FAILURE);
            }
        },
        cli::DaemonCommand::Stop => match stop(&paths).await? {
            Some(pid) => println!("qq daemon stopped (pid {pid})"),
            None => println!("qq daemon is not running"),
        },
        cli::DaemonCommand::Restart(listen) => {
            if let Some(pid) = stop(&paths).await? {
                println!("qq daemon stopped (pid {pid})");
            }
            match start_detached(&paths, &Listen::from(listen), overrides).await? {
                Started::New(connection) | Started::Existing(connection) => println!(
                    "qq daemon started at {} (pid {})",
                    connection.address(),
                    connection.server_info().pid
                ),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Which command hosts the server. `qq serve` and `qq daemon run` differ
/// only in what they print.
#[derive(Debug, Clone, Copy)]
pub enum Host {
    Serve,
    Daemon,
}

impl Host {
    fn already_running(self, connection: &server::ServerConnection) {
        match self {
            Self::Serve => println!("qq server already running at {}", connection.address()),
            Self::Daemon => println!("qq daemon already running at {}", connection.address()),
        }
    }

    fn listening(self, connection: &server::ServerConnection) {
        match self {
            Self::Serve => println!("qq server listening at {}", connection.address()),
            Self::Daemon => println!("qq daemon listening at {}", connection.address()),
        }
    }
}

/// Serves the user-scoped server from this process until it is asked to stop
/// or receives SIGTERM, SIGINT or Ctrl-C.
pub async fn host(
    paths: &server::ServerPaths,
    listen: &Listen,
    overrides: &CliOverrides,
    kind: Host,
) -> Result<(), Box<dyn Error>> {
    // Signals are installed before the store opens so a stop that arrives
    // while recovery runs is not lost; it is acted on once serving starts.
    let mut stop_signal = StopSignal::install()?;
    let options = server::ServerOptions::new(paths.clone())
        .with_bind_address(listen.bind)
        .with_version(cli::BUILD_VERSION)
        .with_allowed_origins(server::AllowedOrigins::new(&listen.allow_origins)?)
        .with_stop_route();
    // A previous host may have released the listener but still be closing
    // its store; wait for it rather than failing with a busy store.
    let host_lock = server::acquire_host_lock(paths, HANDOFF_TIMEOUT).await?;
    let reservation = match server::reserve(options).await? {
        server::ReserveOutcome::Existing(connection) => {
            reject_read_approvals_for_existing_server(&overrides.approved_read_images)?;
            kind.already_running(&connection);
            return Ok(());
        }
        server::ReserveOutcome::Reserved(reservation) => reservation,
    };
    let factory = runtime::RuntimeFactory::system_with_read_images(
        cli::HOST_BUILD,
        overrides.approved_read_images.clone(),
    )?;
    let approval_timeout = approval_timeout(&factory).await?;
    let handler = Arc::new(runtime::RuntimeHandler::open_owned(factory, approval_timeout).await?);
    let identity = handler.server_identity(None);
    let mut server = match reservation.start(handler.clone(), identity) {
        Ok(server) => server,
        Err(error) => {
            // The runtime opened but never served; settle it so the store
            // closes cleanly before reporting the failure.
            let _ = handler.shutdown().await;
            return Err(error.into());
        }
    };
    kind.listening(server.connection());
    let stop_requests = server.stop_requests();
    let stopped = tokio::select! {
        result = stop_signal.recv() => result,
        () = stop_requests.wait() => Ok(()),
        // The listener only ends by itself on an I/O failure; settle the
        // runtime and report it below.
        result = server.finished() => result.map_err(io::Error::other),
    };
    let shutdown = crate::EmbeddedRuntime { server, handler }.shutdown().await;
    drop(host_lock);
    stopped?;
    shutdown?;
    Ok(())
}

/// The approval wait comes from global and managed configuration only. A
/// server serves many workspaces, so the directory it was started from must
/// not decide a server-wide control. Absent means no deadline; a
/// configuration that does not load yet (no model) still serves.
async fn approval_timeout(
    factory: &runtime::RuntimeFactory,
) -> Result<Option<Duration>, Box<dyn Error>> {
    let request = config::LoadRequest::from_process_env(neutral_directory(), None)?;
    let factory = factory.clone();
    Ok(tokio::task::spawn_blocking(move || {
        factory
            .load_for_client(&request)
            .ok()
            .and_then(|snapshot| snapshot.approval_timeout())
    })
    .await?)
}

/// A directory with no project configuration above it, so loading from it
/// yields only the global and managed layers.
fn neutral_directory() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\")
    } else {
        PathBuf::from("/")
    }
}

enum Started {
    New(server::ServerConnection),
    Existing(server::ServerConnection),
}

/// Starts this binary as a background daemon and waits until it serves.
/// Returns the daemon that was already running instead, if there is one.
async fn start_detached(
    paths: &server::ServerPaths,
    listen: &Listen,
    overrides: &CliOverrides,
) -> Result<Started, Box<dyn Error>> {
    if let Some(connection) = server::discover_at(paths).await? {
        return Ok(Started::Existing(connection));
    }
    paths.create_directory()?;
    let log = log_path(paths);
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command.args(daemon_arguments(listen, overrides));
    // The daemon is independent of the directory and terminal that started
    // it: it must not hold a mount busy, read the terminal or die with it.
    command
        .current_dir(neutral_directory())
        .stdin(Stdio::null())
        .stdout(Stdio::from(open_log(&log)?))
        .stderr(Stdio::from(open_log(&log)?));
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | NEW_PROCESS_GROUP.
    let mut child = command.spawn()?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        // Readiness is the same check every client makes: discovery
        // metadata is published only once the server has an identity, and
        // the health probe proves it answers.
        if let Some(connection) = server::discover_at(paths).await? {
            if connection.server_info().pid == child.id().unwrap_or_default() {
                // The daemon outlives this process; reaping it is the
                // operating system's job once this process exits.
                drop(child);
                return Ok(Started::New(connection));
            }
            // Another starter won the race; ours exits on its own after it
            // reports the existing daemon.
            let _ = child.wait().await;
            return Ok(Started::Existing(connection));
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "qq daemon exited before serving ({status}); see {}",
                log.display()
            ))
            .into());
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            return Err(io::Error::other(format!(
                "qq daemon did not start serving within {}s; see {}",
                START_TIMEOUT.as_secs(),
                log.display()
            ))
            .into());
        }
        tokio::time::sleep(START_POLL).await;
    }
}

/// The arguments a background daemon is started with. Read-image approvals
/// are host decisions made when the daemon starts, so they are passed on.
fn daemon_arguments(listen: &Listen, overrides: &CliOverrides) -> Vec<String> {
    let mut arguments = vec![
        "daemon".to_owned(),
        "run".to_owned(),
        "--bind".to_owned(),
        listen.bind.to_string(),
    ];
    for origin in &listen.allow_origins {
        arguments.extend(["--allow-origin".to_owned(), origin.clone()]);
    }
    for image in overrides.approved_read_images.iter() {
        arguments.extend([
            "--approve-self-contained-read-image".to_owned(),
            image.to_string(),
        ]);
    }
    arguments
}

/// Asks the running daemon to stop and waits until it has closed its store.
/// Returns its pid, or `None` when no daemon was running.
async fn stop(paths: &server::ServerPaths) -> Result<Option<u32>, Box<dyn Error>> {
    let Some(connection) = server::discover_at(paths).await? else {
        return Ok(None);
    };
    let stopping = qq_client::SessionClient::new(connection)?
        .request_stop()
        .await
        .map_err(|error| match error {
            qq_client::ClientError::ServerMessage { status: 404, .. } => io::Error::other(
                "the running server belongs to an interactive qq; quit that qq to stop it",
            ),
            error => io::Error::other(format!("could not ask the daemon to stop: {error}")),
        })?;
    if !server::wait_until_released(paths, STOP_TIMEOUT).await? {
        return Err(io::Error::other(format!(
            "qq daemon (pid {}) did not stop within {}s",
            stopping.pid,
            STOP_TIMEOUT.as_secs()
        ))
        .into());
    }
    Ok(Some(stopping.pid))
}

fn log_path(paths: &server::ServerPaths) -> PathBuf {
    paths.directory().join(LOG_FILE_NAME)
}

fn open_log(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// SIGTERM, SIGINT and Ctrl-C, as one stream. Installed once per host.
struct StopSignal {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl StopSignal {
    fn install() -> io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(windows)]
            ctrl_c: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn recv(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        tokio::select! {
            _ = self.terminate.recv() => {},
            _ = self.interrupt.recv() => {},
        }
        #[cfg(windows)]
        self.ctrl_c.recv().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_daemon_receives_the_listen_settings_and_read_image_approvals() {
        let hash = "01".repeat(32);
        let overrides = CliOverrides {
            approved_read_images: qq_core::ApprovedReadImages::new(vec![
                format!("grep:{hash}").parse().unwrap(),
            ])
            .unwrap(),
            ..CliOverrides::default()
        };
        let listen = Listen {
            bind: "127.0.0.1:4711".parse().unwrap(),
            allow_origins: vec!["https://app.example.com".to_owned()],
        };
        let arguments = daemon_arguments(&listen, &overrides);
        let mut argv = vec!["qq".to_owned()];
        argv.extend(arguments.iter().cloned());
        let parsed = <cli::Cli as clap::Parser>::try_parse_from(&argv).unwrap();
        assert_eq!(
            qq_core::ApprovedReadImages::new(parsed.approved_read_images).unwrap(),
            overrides.approved_read_images
        );
        let Some(cli::Command::Daemon {
            command: cli::DaemonCommand::Run(run),
        }) = parsed.command
        else {
            panic!("expected `daemon run`: {arguments:?}");
        };
        assert!(
            !run.detach,
            "the background daemon itself runs in the foreground"
        );
        assert_eq!(run.listen.bind, listen.bind);
        assert_eq!(run.listen.allow_origins, listen.allow_origins);
    }

    #[test]
    fn the_neutral_directory_is_outside_every_project() {
        let neutral = neutral_directory();
        assert!(neutral.is_absolute());
        assert_eq!(neutral.parent(), None);
    }
}
