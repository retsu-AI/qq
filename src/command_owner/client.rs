//! Private command transport. Transport loss never authorizes host execution.
use qq_core::command_owner::{
    CommandOwnerError, CommandSnapshot, NativeCommand, NativeCommandFuture, NativeCommandService,
};
use qq_protocol::{StoreId, ToolCallId};
use qq_server::{ServerConnection, ServerPaths};
use serde::de::DeserializeOwned;
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};

#[derive(Clone)]
pub(crate) struct CommandWorker(Arc<State>);
struct State {
    directory: PathBuf,
    executable: PathBuf,
    expected_build: String,
    http: reqwest::Client,
    connection: Mutex<Option<ServerConnection>>,
    store: Mutex<Option<StoreId>>,
    startup: tokio::sync::Semaphore,
    #[cfg(test)]
    stop: Mutex<Option<tokio::sync::oneshot::Sender<tokio::sync::oneshot::Sender<()>>>>,
}

impl CommandWorker {
    pub(crate) fn new(
        directory: PathBuf,
        executable: PathBuf,
        expected_build: &str,
    ) -> Result<Self, CommandOwnerError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(io_error)?;
        Ok(Self(Arc::new(State {
            directory,
            executable,
            expected_build: expected_build.to_owned(),
            http,
            connection: Mutex::new(None),
            store: Mutex::new(None),
            startup: tokio::sync::Semaphore::new(1),
            #[cfg(test)]
            stop: Mutex::new(None),
        })))
    }

    #[cfg(test)]
    #[allow(
        dead_code,
        reason = "owned-process cleanup for native integration fixtures"
    )]
    pub(crate) async fn stop_spawned(&self) {
        let stop = self.0.stop.lock().unwrap().take();
        if let Some(stop) = stop {
            let (ack, done) = tokio::sync::oneshot::channel();
            if stop.send(ack).is_ok() {
                let _ = done.await;
            }
        }
    }

    async fn connection(&self) -> Result<ServerConnection, CommandOwnerError> {
        if let Some(connection) = self
            .0
            .connection
            .lock()
            .map_err(|_| CommandOwnerError::Unavailable)?
            .clone()
        {
            return Ok(connection);
        }
        let _startup = self.0.startup.acquire().await.map_err(io_error)?;
        if let Some(connection) = self
            .0
            .connection
            .lock()
            .map_err(|_| CommandOwnerError::Unavailable)?
            .clone()
        {
            return Ok(connection);
        }
        let paths = ServerPaths::new(&self.0.directory);
        let connection = match qq_server::command_owner::discover_at(&paths)
            .await
            .map_err(io_error)?
        {
            Some(connection) => connection,
            None => {
                #[cfg(windows)]
                tokio::task::spawn_blocking(qq_core::command_owner::prepare_independent_worker)
                    .await
                    .map_err(io_error)??;
                let mut command = tokio::process::Command::new(&self.0.executable);
                command.env_clear();
                for name in qq_core::BASE_ENV.into_iter().chain([
                    "SYSTEMROOT",
                    "WINDIR",
                    "USERPROFILE",
                    "LOCALAPPDATA",
                    "APPDATA",
                    "TEMP",
                    "TMP",
                ]) {
                    if let Some(value) = std::env::var_os(name) {
                        command.env(name, value);
                    }
                }
                #[cfg(unix)]
                command.process_group(0);
                #[cfg(windows)]
                command.creation_flags(0x0100_0000 | 0x0000_0200); // BREAKAWAY_FROM_JOB | NEW_PROCESS_GROUP.
                let mut child = command
                    .arg("_command-owner")
                    .arg("--state-dir")
                    .arg(&self.0.directory)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    // The worker outlives the coordinator. Inheriting its stderr
                    // would keep piped CLI output open after the CLI exits.
                    .stderr(Stdio::null())
                    .spawn()?;
                let mut stdout =
                    BufReader::new(child.stdout.take().ok_or(CommandOwnerError::Unavailable)?)
                        .take(1025);
                // Reaping is independent of callers; dropping an observer does
                // not kill the worker or the commands whose journal it owns.
                #[cfg(not(test))]
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                #[cfg(test)]
                {
                    let (stop, stopped) =
                        tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();
                    *self
                        .0
                        .stop
                        .lock()
                        .map_err(|_| CommandOwnerError::Unavailable)? = Some(stop);
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = child.wait() => {},
                            stop = stopped => match stop {
                                Ok(ack) => { let _ = child.kill().await; let _ = child.wait().await; let _ = ack.send(()); },
                                Err(_) => { let _ = child.wait().await; },
                            }
                        }
                    });
                }
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(15), stdout.read_line(&mut line))
                    .await
                    .map_err(io_error)??;
                if line.len() > 1024 {
                    return Err(CommandOwnerError::Unavailable);
                }
                if !line.starts_with("qq command owner listening at ")
                    && !line.starts_with("qq command owner already running at ")
                {
                    return Err(CommandOwnerError::Unavailable);
                }
                qq_server::command_owner::discover_at(&paths)
                    .await
                    .map_err(io_error)?
                    .ok_or(CommandOwnerError::Unavailable)?
            }
        };
        if connection.server_info().version != self.0.expected_build {
            return Err(CommandOwnerError::IncompatibleBuild {
                expected: self.0.expected_build.clone(),
                actual: connection.server_info().version.clone(),
            });
        }
        {
            let mut expected = self
                .0
                .store
                .lock()
                .map_err(|_| CommandOwnerError::Unavailable)?;
            match *expected {
                Some(expected) if expected != connection.server_info().server_id => {
                    return Err(CommandOwnerError::Unavailable);
                }
                Some(_) => {}
                None => *expected = Some(connection.server_info().server_id),
            }
        }
        *self
            .0
            .connection
            .lock()
            .map_err(|_| CommandOwnerError::Unavailable)? = Some(connection.clone());
        Ok(connection)
    }

    fn forget(&self, connection: &ServerConnection) -> Result<(), CommandOwnerError> {
        let mut current = self
            .0
            .connection
            .lock()
            .map_err(|_| CommandOwnerError::Unavailable)?;
        if current.as_ref().is_some_and(|current| {
            current.address() == connection.address()
                && current.expose_bearer_token() == connection.expose_bearer_token()
        }) {
            *current = None;
        }
        Ok(())
    }

    async fn post<T: DeserializeOwned>(
        &self,
        operation: &str,
        body: Vec<u8>,
    ) -> Result<T, CommandOwnerError> {
        let mut retried = false;
        let mut uncertain = false;
        let capacity_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut capacity_delay = Duration::from_millis(10);
        'request: loop {
            let connection = self.connection().await?;
            let response = self
                .0
                .http
                .post(format!(
                    "http://{}/internal/command-owner/v3/{operation}",
                    connection.address()
                ))
                .bearer_auth(connection.expose_bearer_token())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .timeout(Duration::from_secs(if operation == "wait" {
                    60
                } else {
                    10
                }))
                .send()
                .await;
            let mut response = match response {
                Ok(response) => response,
                Err(error) if operation == "wait" && error.is_timeout() && !error.is_connect() => {
                    continue;
                }
                Err(error) => {
                    if retried {
                        return Err(io_error(error));
                    }
                    uncertain = true;
                    retried = true;
                    self.forget(&connection)?;
                    continue;
                }
            };
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !retried {
                retried = true;
                self.forget(&connection)?;
                continue;
            }
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                && matches!(operation, "cancel" | "wait")
            {
                let remaining =
                    capacity_deadline.saturating_duration_since(tokio::time::Instant::now());
                if !remaining.is_zero() {
                    // These idempotent controls were not admitted by the
                    // transport. A burst must not abandon a live command.
                    // lint: allow(sleep) production transport backoff, not a test wait.
                    tokio::time::sleep(capacity_delay.min(remaining)).await;
                    capacity_delay = (capacity_delay * 2).min(Duration::from_millis(250));
                    continue;
                }
            }
            if !response.status().is_success() {
                if uncertain && operation == "submit" {
                    return Err(io_error(
                        "native admission outcome is unknown after reconnect",
                    ));
                }
                return Err(match response.status().as_u16() {
                    400 => CommandOwnerError::Invalid,
                    404 => CommandOwnerError::NotFound,
                    409 => CommandOwnerError::Conflict,
                    429 => CommandOwnerError::Full,
                    _ => CommandOwnerError::Unavailable,
                });
            }
            let mut bytes = Vec::new();
            loop {
                let chunk = match response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(error) => {
                        if retried {
                            return Err(io_error(error));
                        }
                        uncertain = true;
                        retried = true;
                        self.forget(&connection)?;
                        continue 'request;
                    }
                };
                if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
                    return Err(io_error("native command response exceeds its bound"));
                }
                bytes.extend_from_slice(&chunk);
            }
            return serde_json::from_slice(&bytes).map_err(CommandOwnerError::Codec);
        }
    }
}

impl NativeCommandService for CommandWorker {
    fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
        let client = self.clone();
        Box::pin(async move { client.post("submit", serde_json::to_vec(&request)?).await })
    }
    fn wait(&self, id: ToolCallId, after: u64) -> NativeCommandFuture<CommandSnapshot> {
        let client = self.clone();
        Box::pin(async move {
            client
                .post(
                    "wait",
                    serde_json::to_vec(&serde_json::json!({"id":id,"after":after}))?,
                )
                .await
        })
    }
    fn cancel(&self, id: ToolCallId) -> NativeCommandFuture<()> {
        let client = self.clone();
        Box::pin(async move {
            client
                .post("cancel", serde_json::to_vec(&serde_json::json!({"id":id}))?)
                .await
        })
    }
}

fn io_error(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> CommandOwnerError {
    CommandOwnerError::Io(std::io::Error::other(error))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
