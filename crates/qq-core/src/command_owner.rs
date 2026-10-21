//! Native execution owned independently of its observers.
//!
//! Only a trusted host may submit an already-authorized command. This module
//! records execution lifetime; it does not grant authority or claim containment.
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use qq_protocol::{StoreId, ToolCallId};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};

use crate::tools::{ToolOutput, ToolTasks, process_context::ExecutionContextReceipt};
use crate::{ApprovedReadImage, ApprovedReadImages, ProcessProducer, RunCancellation, ShellPolicy};

mod environment;
mod journal;
pub use environment::NativeEnvironment;
#[cfg(windows)]
mod windows_state;
use journal::Journal;

pub use crate::tools::dispatch::ToolDrainError as CommandCleanupError;

const MAX_ACTIVE: u32 = 128;
const MAX_RETAINED: usize = 10_000;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_LIVE_OUTPUT_BYTES: usize = crate::tools::MAX_SHELL_OUTPUT_BYTES;

/// Trusted entry point that runs QQ's native launch helper before any payload.
/// Embedders may supply their own argument prefix calling `execute_native_launch`.
/// Linux captures a sealed ELF image; the entry point must call the helper
/// directly rather than delegate through a script or mutable executable path.
#[derive(Debug, Clone)]
pub struct CommandLauncher {
    pub executable: PathBuf,
    pub arguments: Vec<std::ffi::OsString>,
}

impl CommandLauncher {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            arguments: vec!["_command-exec".into()],
        }
    }
}

/// Trusted Linux monitor/PID-1 entry point, called before starting a runtime.
#[cfg(target_os = "linux")]
pub fn execute_native_launch(encoded: &str) -> std::io::Result<u8> {
    crate::tools::linux_owner::execute(encoded)
}

/// An authorized invocation. Its ID is an idempotency key, not a new attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCommand {
    pub environment: NativeEnvironment,
    pub id: ToolCallId,
    /// Canonical absolute root, opened without following symlink components.
    pub workspace: PathBuf,
    pub tool: String,
    pub arguments: String,
    pub shell_policy: ShellPolicy,
    pub approved_images: Vec<ApprovedReadImage>,
    /// Absolute task deadline, captured by the admitting host. Reconnecting
    /// cannot grant a fresh duration. The owner converts it to a monotonic wait.
    pub not_after_ms: Option<u64>,
}

impl NativeCommand {
    fn identity(&self) -> Result<String, CommandOwnerError> {
        let mut identity = serde_json::to_value(self)?;
        identity["environment"] = serde_json::Value::String(self.environment.fingerprint()?);
        Ok(serde_json::to_string(&identity)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Queued,
    Running,
    Finished,
    /// The owner stopped without recording an exit. Never replay this ID.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessReceipt(Box<ExecutionContextReceipt>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandResult(ToolOutput);

impl CommandResult {
    pub fn text(&self) -> &str {
        &self.0.model_text
    }
    pub fn is_error(&self) -> bool {
        self.0.is_error
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSnapshot {
    pub id: ToolCallId,
    pub version: u64,
    pub status: CommandStatus,
    pub process: Option<ProcessReceipt>,
    /// UTF-8 view of the persisted byte prefix. Incomplete trailing codepoints
    /// remain buffered while execution is live.
    pub output: String,
    pub result: Option<CommandResult>,
}

#[derive(Debug, thiserror::Error)]
pub enum CommandOwnerError {
    #[error("command observations were reclaimed after acknowledgement")]
    Retired,
    #[error("another command owner holds this journal")]
    Busy,
    #[error("command owner state is not private or is not a regular file")]
    PrivateState,
    #[error("command identity was reused for another invocation")]
    Conflict,
    #[error("command owner capacity is exhausted")]
    Full,
    #[error("command was not found")]
    NotFound,
    #[error("invalid native command")]
    Invalid,
    #[error("command owner is stopping or has lost durable execution state")]
    Unavailable,
    #[error("command owner filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("command journal failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("command journal encoding failed: {0}")]
    Codec(#[from] serde_json::Error),
    #[error("command journal identity failed: {0}")]
    Identity(#[from] qq_protocol::IdError),
    #[error("unsupported command journal schema {0}")]
    Schema(i64),
    #[error("command cleanup failed: {0}")]
    Cleanup(#[source] CommandCleanupError),
    #[error("command owner shutdown failed: {0}")]
    Shutdown(#[source] Arc<CommandOwnerError>),
}

/// Executions retain ownership after observers disconnect. Implicit disposal
/// closes the journal asynchronously; await `shutdown` before immediate reuse.
#[derive(Clone)]
pub struct CommandOwner(Arc<OwnerState>);

type ShutdownResult = Result<(), Arc<CommandOwnerError>>;

struct OwnerState {
    journal: Journal,
    jobs: Arc<Mutex<HashMap<ToolCallId, LiveCommand>>>,
    slots: Arc<Semaphore>,
    controls: Arc<Semaphore>,
    tools: ToolTasks,
    producer: Arc<ProcessProducer>,
    failed: watch::Sender<bool>,
    closing: AtomicBool,
    stopped: watch::Sender<Option<ShutdownResult>>,
}

struct LiveCommand {
    changed: watch::Sender<u64>,
    cancellation: RunCancellation,
}

impl CommandOwner {
    /// Whether this owner is accepting work. Failure and shutdown both make
    /// discovery unavailable; a listening transport alone does not mean healthy.
    pub fn is_available(&self) -> bool {
        self.available().is_ok()
    }

    /// Wait for a fatal execution or journal failure. This is cancellation-safe
    /// and also returns immediately for a failure that preceded subscription.
    pub async fn wait_for_failure(&self) {
        let mut failed = self.0.failed.subscribe();
        let _ = failed.wait_for(|failed| *failed).await;
    }

    /// The journal identity, retained across process and endpoint changes.
    pub fn store_id(&self) -> StoreId {
        self.0.journal.store_id()
    }

    pub async fn open(
        directory: PathBuf,
        source_revision: &str,
        launcher: CommandLauncher,
    ) -> Result<Self, CommandOwnerError> {
        let journal = Journal::open(directory).await?;
        let (failed, _) = watch::channel(false);
        let (stopped, _) = watch::channel(None);
        let jobs = Arc::new(Mutex::new(HashMap::<ToolCallId, LiveCommand>::new()));
        #[cfg(target_os = "linux")]
        let tools =
            blocking(move || ToolTasks::owned_linux(launcher).map_err(CommandOwnerError::Io))
                .await?;
        #[cfg(not(target_os = "linux"))]
        let tools = {
            let _ = launcher;
            ToolTasks::default()
        };
        let record_journal = journal.clone();
        let record_failed = failed.clone();
        let record_jobs = Arc::clone(&jobs);
        tools
            .set_process_recorder(move |id, receipt| {
                let journal = record_journal.clone();
                let failed = record_failed.clone();
                let jobs = Arc::clone(&record_jobs);
                async move {
                    match journal.started(id, receipt).await {
                        Ok(version) => {
                            if let Some(job) = jobs
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .get(&id)
                            {
                                job.changed.send_replace(version);
                            }
                            Ok(())
                        }
                        Err(error) => {
                            tracing::error!(target: "qq::command_owner", error = %error, "could not persist command start");
                            fail(&failed, &jobs);
                            Err(crate::sessions::SessionRuntimeError::Unavailable)
                        }
                    }
                }
            })
            .map_err(|_| CommandOwnerError::Unavailable)?;
        let output_journal = journal.clone();
        let output_failed = failed.clone();
        let output_jobs = Arc::clone(&jobs);
        tools.set_output_recorder(move |id, chunk| {
            let journal = output_journal.clone();
            let failed = output_failed.clone();
            let jobs = Arc::clone(&output_jobs);
            async move {
                match journal.output(id, chunk).await {
                    Ok(version) => {
                        if let Some(job) = jobs.lock().unwrap_or_else(|error| error.into_inner()).get(&id) {
                            job.changed.send_replace(version);
                        }
                        Ok(())
                    }
                    Err(error) => {
                        tracing::error!(target: "qq::command_owner", error = %error, "could not persist command output");
                        fail(&failed, &jobs);
                        Err(crate::sessions::SessionRuntimeError::Unavailable)
                    }
                }
            }
        }).map_err(|_| CommandOwnerError::Unavailable)?;
        Ok(Self(Arc::new(OwnerState {
            journal,
            jobs,
            slots: Arc::new(Semaphore::new(MAX_ACTIVE as usize)),
            controls: Arc::new(Semaphore::new(MAX_ACTIVE as usize)),
            tools,
            producer: Arc::new(ProcessProducer::new(source_revision)),
            failed,
            closing: AtomicBool::new(false),
            stopped,
        })))
    }

    /// Commit admission before starting execution. An identical retry observes
    /// the existing command; it never executes the invocation a second time.
    pub async fn submit(&self, mut request: NativeCommand) -> Result<(), CommandOwnerError> {
        self.available()?;
        request.environment.normalize()?;
        let images = ApprovedReadImages::new(std::mem::take(&mut request.approved_images))
            .map_err(|_| CommandOwnerError::Invalid)?;
        request.approved_images = images.entries().to_vec();
        if !request.workspace.is_absolute()
            || !matches!(request.tool.as_str(), "shell" | "exec")
            || request.arguments.len() > 64 * 1024
            || crate::tools::process_context::arguments_digest(&request.tool, &request.arguments)
                .is_none()
            || request.shell_policy.env_allowlist.len() > crate::MAX_SHELL_ENV_ALLOWLIST
            || request
                .shell_policy
                .env_allowlist
                .iter()
                .any(|name| !crate::runtime::valid_env_name(name))
        {
            return Err(CommandOwnerError::Invalid);
        }
        let task_deadline = request.not_after_ms.map(monotonic_deadline).transpose()?;
        let encoded = request.identity()?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(CommandOwnerError::Invalid);
        }
        let existing = self
            .0
            .journal
            .existing(request.id, encoded.clone())
            .await
            .inspect_err(|error| {
                if !matches!(
                    error,
                    CommandOwnerError::Conflict | CommandOwnerError::Retired
                ) {
                    self.failure();
                }
            })?;
        if existing {
            return Ok(());
        }
        let slot = Arc::clone(&self.0.slots)
            .try_acquire_owned()
            .map_err(|_| CommandOwnerError::Full)?;
        self.available()?;
        let (changed, _) = watch::channel(0);
        let cancellation = RunCancellation::new();
        // Register before admission so a concurrent observer cannot miss the
        // first committed transition. Duplicates retain the original watcher.
        {
            self.0
                .jobs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entry(request.id)
                .or_insert_with(|| LiveCommand {
                    changed,
                    cancellation: cancellation.clone(),
                });
        }
        let (reply, admitted) = tokio::sync::oneshot::channel();
        let owner = self.clone();
        // Admission and execution have the same owner. Losing the response
        // must not strand a committed invocation between these two phases.
        tokio::spawn(async move {
            let mut guard = DriverGuard {
                failed: owner.0.failed.clone(),
                jobs: Arc::clone(&owner.0.jobs),
                completed: false,
            };
            let id = request.id;
            let admission = owner.0.journal.admit(id, encoded).await;
            match admission {
                Ok(false) => {
                    let _ = reply.send(Ok(()));
                    guard.completed = true;
                }
                Err(error @ CommandOwnerError::Retired) => {
                    owner
                        .0
                        .jobs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(&id);
                    let _ = reply.send(Err(error));
                    guard.completed = true;
                }
                Err(error @ CommandOwnerError::Conflict) => {
                    let _ = reply.send(Err(error));
                    guard.completed = true;
                }
                Err(CommandOwnerError::Full) => {
                    owner
                        .0
                        .jobs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(&id);
                    let _ = reply.send(Err(CommandOwnerError::Full));
                    guard.completed = true;
                }
                Err(error) => {
                    tracing::error!(target: "qq::command_owner", error = %error, "could not persist command admission");
                    let _ = reply.send(Err(error));
                    owner.failure();
                }
                Ok(true) => {
                    let cancellation = owner
                        .0
                        .jobs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get(&id)
                        .expect("admitted command has a watcher")
                        .cancellation
                        .clone();
                    if owner.0.closing.load(Ordering::Acquire) || *owner.0.failed.borrow() {
                        cancellation.cancel();
                    }
                    let _ = reply.send(Ok(()));
                    let result = owner
                        .execute(request, images, cancellation, task_deadline)
                        .await;
                    match result {
                        Ok(result) if !*owner.0.failed.borrow() => {
                            match owner.0.journal.finished(id, result).await {
                                Ok(version) => {
                                    if let Some(job) = owner
                                        .0
                                        .jobs
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .get(&id)
                                    {
                                        job.changed.send_replace(version);
                                    }
                                    guard.completed = true;
                                }
                                Err(error) => {
                                    tracing::error!(target: "qq::command_owner", error = %error, "could not persist command completion");
                                    owner.failure();
                                }
                            }
                        }
                        Err(error) => {
                            tracing::error!(target: "qq::command_owner", error = %error, "command execution failed");
                            owner.failure();
                        }
                        Ok(_) => owner.failure(),
                    }
                }
            }
            // Releasing the last execution slot proves the driver has also
            // dropped its journal reference, so shutdown/reopen cannot race it.
            drop(guard);
            drop(owner);
            drop(slot);
        });
        admitted.await.map_err(|_| CommandOwnerError::Unavailable)?
    }

    async fn execute(
        &self,
        request: NativeCommand,
        images: ApprovedReadImages,
        cancellation: RunCancellation,
        task_deadline: Option<tokio::time::Instant>,
    ) -> Result<ToolOutput, CommandOwnerError> {
        let path = request.workspace;
        let workspace = match blocking(move || {
            crate::workspace::Workspace::open(&path).map_err(CommandOwnerError::Io)
        })
        .await
        {
            Ok(workspace) => workspace,
            Err(CommandOwnerError::Io(error)) => {
                return Ok(ToolOutput::refused(format!(
                    "could not open command workspace: {error}"
                )));
            }
            Err(error) => return Err(error),
        };
        let cancelled = self.0.journal.cancel_requested(request.id).await?;
        if cancelled {
            cancellation.cancel();
        }
        if task_deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
            cancellation.cancel();
        }
        let execution = crate::tools::dispatch::execute_with_images(
            Some(Arc::new(request.environment)),
            workspace,
            Arc::default(),
            request.tool,
            request.arguments,
            cancellation.clone(),
            None,
            self.0.tools.clone(),
            Arc::new(request.shell_policy),
            Arc::default(),
            Arc::new(images),
            Arc::clone(&self.0.producer),
            Some(request.id),
        );
        tokio::pin!(execution);
        let expired = async {
            match task_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(expired);
        let output = tokio::select! {
            biased;
            () = &mut expired => { cancellation.cancel(); execution.await }
            output = &mut execution => output,
        };
        self.0.tools.check().map_err(CommandOwnerError::Cleanup)?;
        Ok(output)
    }

    pub async fn snapshot(&self, id: ToolCallId) -> Result<CommandSnapshot, CommandOwnerError> {
        self.0.journal.snapshot(id).await.inspect_err(|error| {
            if !matches!(
                error,
                CommandOwnerError::NotFound | CommandOwnerError::Retired
            ) {
                self.failure();
            }
        })
    }

    /// Wait for a committed transition without polling. Dropping this future
    /// only drops an observer; cancellation is a separate durable operation.
    pub async fn wait(
        &self,
        id: ToolCallId,
        after: u64,
    ) -> Result<CommandSnapshot, CommandOwnerError> {
        let mut failed = self.0.failed.subscribe();
        let mut changed = self
            .0
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&id)
            .map(|job| job.changed.subscribe());
        loop {
            if *failed.borrow() {
                return Err(CommandOwnerError::Unavailable);
            }
            let snapshot = self.snapshot(id).await?;
            if snapshot.version > after
                || matches!(
                    snapshot.status,
                    CommandStatus::Finished | CommandStatus::Lost
                )
            {
                return Ok(snapshot);
            }
            let Some(changed) = &mut changed else {
                return Err(CommandOwnerError::Unavailable);
            };
            tokio::select! {
                result = changed.changed() => if result.is_err() { return Err(CommandOwnerError::Unavailable); },
                result = failed.changed() => if result.is_err() { return Err(CommandOwnerError::Unavailable); },
            }
        }
    }

    pub async fn cancel(&self, id: ToolCallId) -> Result<(), CommandOwnerError> {
        let slot = Arc::clone(&self.0.controls)
            .try_acquire_owned()
            .map_err(|_| CommandOwnerError::Full)?;
        // Enter the drain scope before checking admission, so shutdown cannot
        // return between accepting a cancellation and its durable application.
        let lease = self.0.tools.enter();
        self.available()?;
        // Revocation must not wait behind a stalled durable operation. The
        // owned driver below still retains the write and its acknowledgement.
        self.cancel_live(id);
        let (reply, applied) = tokio::sync::oneshot::channel();
        let owner = self.clone();
        tokio::spawn(async move {
            let mut guard = DriverGuard {
                failed: owner.0.failed.clone(),
                jobs: Arc::clone(&owner.0.jobs),
                completed: false,
            };
            let result = owner.0.journal.cancel(id).await;
            match &result {
                Ok(()) => {
                    // Also cover admission registered after the initial stop.
                    owner.cancel_live(id);
                    guard.completed = true;
                }
                Err(CommandOwnerError::Full) => guard.completed = true,
                Err(error) => {
                    tracing::error!(target: "qq::command_owner", error = %error, "could not persist command cancellation");
                    owner.failure();
                }
            }
            let _ = reply.send(result);
            drop(guard);
            drop(owner);
            drop(lease);
            drop(slot);
        });
        applied.await.map_err(|_| CommandOwnerError::Unavailable)?
    }

    /// Discard terminal observations only after the consumer has saved or deliberately
    /// discarded them. A compact record continues to prevent repeated execution.
    pub async fn acknowledge(&self, ids: Vec<ToolCallId>) -> Result<(), CommandOwnerError> {
        if ids.is_empty() || ids.len() > 128 {
            return Err(CommandOwnerError::Invalid);
        }
        let slot = Arc::clone(&self.0.controls)
            .try_acquire_owned()
            .map_err(|_| CommandOwnerError::Full)?;
        let lease = self.0.tools.enter();
        self.available()?;
        let (reply, applied) = tokio::sync::oneshot::channel();
        let owner = self.clone();
        tokio::spawn(async move {
            let mut guard = DriverGuard {
                failed: owner.0.failed.clone(),
                jobs: Arc::clone(&owner.0.jobs),
                completed: false,
            };
            let result = owner.0.journal.acknowledge(ids.clone()).await;
            match &result {
                Ok(()) => {
                    let mut jobs = owner
                        .0
                        .jobs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    for id in ids {
                        jobs.remove(&id);
                    }
                    guard.completed = true;
                }
                Err(CommandOwnerError::Invalid | CommandOwnerError::NotFound) => {
                    guard.completed = true
                }
                Err(error) => {
                    tracing::error!(target: "qq::command_owner", error = %error, "could not persist command acknowledgement");
                    owner.failure();
                }
            }
            let _ = reply.send(result);
            drop(guard);
            drop(owner);
            drop(lease);
            drop(slot);
        });
        applied.await.map_err(|_| CommandOwnerError::Unavailable)?
    }

    pub async fn shutdown(&self) -> Result<(), CommandOwnerError> {
        let mut stopped = self.0.stopped.subscribe();
        if !self.0.closing.swap(true, Ordering::AcqRel) {
            let owner = self.clone();
            let mut guard = ShutdownGuard {
                stopped: self.0.stopped.clone(),
                completed: false,
            };
            tokio::spawn(async move {
                let result = owner.stop_owned().await.map_err(Arc::new);
                guard.stopped.send_replace(Some(result));
                guard.completed = true;
                drop(guard);
                drop(owner);
            });
        }
        loop {
            let result = stopped.borrow().clone();
            if let Some(result) = result {
                return result.map_err(CommandOwnerError::Shutdown);
            }
            stopped
                .changed()
                .await
                .map_err(|_| CommandOwnerError::Unavailable)?;
        }
    }

    async fn stop_owned(&self) -> Result<(), CommandOwnerError> {
        for job in self
            .0
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            job.cancellation.cancel();
        }
        let _slots = Arc::clone(&self.0.slots)
            .acquire_many_owned(MAX_ACTIVE)
            .await
            .map_err(|_| CommandOwnerError::Unavailable)?;
        self.0
            .tools
            .drain()
            .await
            .map_err(CommandOwnerError::Cleanup)?;
        self.0.journal.close().await?;
        if *self.0.failed.borrow() {
            Err(CommandOwnerError::Unavailable)
        } else {
            Ok(())
        }
    }

    fn available(&self) -> Result<(), CommandOwnerError> {
        if self.0.closing.load(Ordering::Acquire) || *self.0.failed.borrow() {
            Err(CommandOwnerError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn failure(&self) {
        fail(&self.0.failed, &self.0.jobs);
    }

    fn cancel_live(&self, id: ToolCallId) {
        let cancellation = self
            .0
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&id)
            .map(|job| job.cancellation.clone());
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
        }
    }
}

struct ShutdownGuard {
    stopped: watch::Sender<Option<ShutdownResult>>,
    completed: bool,
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.stopped
                .send_replace(Some(Err(Arc::new(CommandOwnerError::Unavailable))));
        }
    }
}

fn monotonic_deadline(deadline: u64) -> Result<tokio::time::Instant, CommandOwnerError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CommandOwnerError::Unavailable)?;
    let now_ms = u64::try_from(now.as_millis()).map_err(|_| CommandOwnerError::Invalid)?;
    tokio::time::Instant::now()
        .checked_add(std::time::Duration::from_millis(
            deadline.saturating_sub(now_ms),
        ))
        .ok_or(CommandOwnerError::Invalid)
}

struct DriverGuard {
    failed: watch::Sender<bool>,
    jobs: Arc<Mutex<HashMap<ToolCallId, LiveCommand>>>,
    completed: bool,
}
impl Drop for DriverGuard {
    fn drop(&mut self) {
        if !self.completed {
            fail(&self.failed, &self.jobs);
        }
    }
}

fn fail(failed: &watch::Sender<bool>, jobs: &Mutex<HashMap<ToolCallId, LiveCommand>>) {
    failed.send_replace(true);
    for job in jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
    {
        job.cancellation.cancel();
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, CommandOwnerError> + Send + 'static,
) -> Result<T, CommandOwnerError> {
    let permit = crate::workspace::blocking_permits()
        .acquire_owned()
        .await
        .map_err(|_| CommandOwnerError::Unavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|_| CommandOwnerError::Unavailable)?
}

#[cfg(test)]
struct AdmissionHook {
    id: ToolCallId,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(test)]
static ADMISSION_HOOKS: Mutex<Vec<AdmissionHook>> = Mutex::new(Vec::new());

#[cfg(test)]
static CANCELLATION_HOOKS: Mutex<Vec<AdmissionHook>> = Mutex::new(Vec::new());

#[cfg(test)]
fn wait_at_commit(hooks: &Mutex<Vec<AdmissionHook>>, id: ToolCallId) {
    let hook = {
        let mut hooks = hooks.lock().unwrap();
        hooks
            .iter()
            .position(|hook| hook.id == id)
            .map(|index| hooks.remove(index))
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.blocking_recv();
    }
}

#[cfg(test)]
static OUTPUT_HOOKS: Mutex<Vec<AdmissionHook>> = Mutex::new(Vec::new());

#[cfg(test)]
async fn wait_before_output(id: ToolCallId) {
    let hook = {
        let mut hooks = OUTPUT_HOOKS.lock().unwrap();
        hooks
            .iter()
            .position(|hook| hook.id == id)
            .map(|index| hooks.remove(index))
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
