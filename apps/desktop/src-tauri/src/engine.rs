//! Finds the user's QQ server, starting one when none is running.
//!
//! The desktop app is a client like the TUI: it never owns sessions. It
//! attaches to the user-scoped server that `qq`, `qq serve`, and other
//! clients discover, so work started in the terminal shows up here and the
//! other way round. When nothing is running it supervises its own
//! `qq serve --exit-on-stdin-close`, holding the stdin pipe so the server
//! exits with the app even when the app is killed.

use std::{
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use qq_client::Connection;

/// How long a freshly started server has to publish its discovery record.
const START_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("could not find the qq engine binary; set QQ_DESKTOP_ENGINE or install qq on PATH")]
    BinaryNotFound,
    #[error("could not start the qq engine: {0}")]
    Spawn(std::io::Error),
    #[error("the qq engine exited before it was ready ({0})")]
    Exited(std::process::ExitStatus),
    #[error("the qq engine did not become ready within {0:?}")]
    Timeout(Duration),
    #[error("qq server discovery failed: {0}")]
    Discovery(String),
}

/// A server this app started. Dropping it closes stdin, which stops the
/// server; the child is then reaped.
struct Supervised {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Drop for Supervised {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct Engine {
    supervised: Mutex<Option<Supervised>>,
    /// Serializes `ensure` so concurrent projects start one server, not many.
    starting: tokio::sync::Mutex<()>,
}

impl Engine {
    /// A connection to the user's server, starting one if necessary.
    pub async fn ensure(&self) -> Result<Connection, EngineError> {
        let _guard = self.starting.lock().await;
        // A discovery error is not final. `discover` judges the recorded
        // metadata before probing it, so a record left by an older build
        // that exited uncleanly reads as a protocol mismatch although no
        // server runs. `qq serve` decides by the instance lock instead: it
        // replaces a stale record, or exits when a live server holds the
        // lock, in which case the discovery error is the real answer.
        let mut last_error = match discover().await {
            Ok(Some(connection)) => return Ok(connection),
            Ok(None) => None,
            Err(error) => Some(error),
        };
        self.spawn()?;
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match discover().await {
                Ok(Some(connection)) => return Ok(connection),
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
            if let Some(status) = self.exited() {
                return Err(last_error.unwrap_or(EngineError::Exited(status)));
            }
            if Instant::now() >= deadline {
                return Err(last_error.unwrap_or(EngineError::Timeout(START_TIMEOUT)));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Whether this app is running the server itself rather than attached
    /// to one the terminal started.
    pub fn owns_server(&self) -> bool {
        self.supervised
            .lock()
            .is_ok_and(|supervised| supervised.is_some())
    }

    /// Stop the server this app started, if any.
    pub fn shutdown(&self) {
        if let Ok(mut supervised) = self.supervised.lock() {
            supervised.take();
        }
    }

    fn exited(&self) -> Option<std::process::ExitStatus> {
        let mut supervised = self.supervised.lock().ok()?;
        let status = supervised.as_mut()?.child.try_wait().ok()??;
        supervised.take();
        Some(status)
    }

    fn spawn(&self) -> Result<(), EngineError> {
        let mut supervised = self
            .supervised
            .lock()
            .map_err(|_| EngineError::Discovery("engine state poisoned".to_owned()))?;
        if let Some(existing) = supervised.as_mut()
            && matches!(existing.child.try_wait(), Ok(None))
        {
            return Ok(());
        }
        let binary = engine_binary().ok_or(EngineError::BinaryNotFound)?;
        let mut command = Command::new(binary);
        command
            .args(["serve", "--exit-on-stdin-close"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(EngineError::Spawn)?;
        let stdin = child.stdin.take();
        *supervised = Some(Supervised { child, stdin });
        Ok(())
    }
}

async fn discover() -> Result<Option<Connection>, EngineError> {
    qq_server::discover()
        .await
        .map(|connection| connection.map(Connection::from))
        .map_err(|error| EngineError::Discovery(error.to_string()))
}

/// The `qq` binary to run as the server, in order: an explicit override, the
/// copy bundled next to this executable, a workspace build (development),
/// then `PATH`.
fn engine_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) { "qq.exe" } else { "qq" };
    if let Some(path) = std::env::var_os("QQ_DESKTOP_ENGINE").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)));
    if let Some(path) = beside.filter(|path| path.is_file()) {
        return Some(path);
    }
    // Development builds live in the desktop workspace's own target
    // directory, so the engine from `cargo build --bin qq` at the repository
    // root is not beside them.
    if cfg!(debug_assertions) {
        let repository_build = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/debug")
            .join(name);
        if repository_build.is_file() {
            return Some(repository_build);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    })
}
