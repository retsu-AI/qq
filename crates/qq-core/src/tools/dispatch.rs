#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[cfg(test)]
use serde::Deserialize;
use tokio::sync::{Notify, mpsc};

use qq_protocol::ToolCallDisplay;

use crate::{
    approval::edit_result_display,
    workspace::{FileState, FileStateUpdate, Workspace, blocking_permits},
};

use super::{
    edit::edit_file,
    list::list_dir,
    output::{Bounds, bound_text, mask_secrets},
    read::read_file,
    search::search,
    shell::{ShellArgs, run_shell},
    specs::BuiltInTool,
    write::write_file,
};

const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
#[cfg(test)]
static TEST_EXECUTIONS_STARTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_EXECUTION_BARRIER: OnceLock<std::sync::Barrier> = OnceLock::new();

/// Tracks actual local execution, including work whose result waiter was
/// dropped. Stop dispatching into the scope before awaiting its drain.
#[derive(Clone, Default)]
pub(crate) struct ToolTasks(Arc<ToolTaskState>);

#[derive(Default)]
struct ToolTaskState {
    active: AtomicUsize,
    idle: Notify,
    unconfirmed_process_exit: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ToolDrainError {
    #[error("local shell process termination could not be confirmed")]
    UnconfirmedProcessExit,
}

impl ToolTasks {
    pub(crate) fn check(&self) -> Result<(), ToolDrainError> {
        if self.0.unconfirmed_process_exit.load(Ordering::Acquire) {
            Err(ToolDrainError::UnconfirmedProcessExit)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn drain(&self) -> Result<(), ToolDrainError> {
        loop {
            let idle = self.0.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.0.active.load(Ordering::Acquire) == 0 {
                return self.check();
            }
            idle.await;
        }
    }

    fn enter(&self) -> ToolTaskLease {
        self.0.active.fetch_add(1, Ordering::AcqRel);
        ToolTaskLease {
            tasks: Arc::clone(&self.0),
            process_pending: false,
        }
    }
}

struct ToolTaskLease {
    tasks: Arc<ToolTaskState>,
    process_pending: bool,
}

impl Drop for ToolTaskLease {
    fn drop(&mut self) {
        if self.process_pending {
            self.tasks
                .unconfirmed_process_exit
                .store(true, Ordering::Release);
        }
        if self.tasks.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tasks.idle.notify_waiters();
        }
    }
}

pub(super) struct ToolCancellation {
    run: Arc<AtomicBool>,
    call: Arc<CallCancellation>,
}

#[derive(Default)]
struct CallCancellation {
    cancelled: AtomicBool,
    wake: Notify,
}

impl ToolCancellation {
    pub(super) fn new(run: Arc<AtomicBool>) -> Self {
        Self {
            run,
            call: Arc::new(CallCancellation::default()),
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.run.load(Ordering::Acquire) || self.call.cancelled.load(Ordering::Acquire)
    }

    pub(super) async fn caller_dropped(&self) {
        loop {
            let wake = self.call.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.call.cancelled.load(Ordering::Acquire) {
                return;
            }
            wake.await;
        }
    }
}

struct CancelCallOnDrop(Arc<CallCancellation>);

impl Drop for CancelCallOnDrop {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_waiters();
    }
}

pub(crate) async fn execute(
    workspace: Workspace,
    file_state: Arc<FileState>,
    name: String,
    arguments: String,
    cancelled: Arc<AtomicBool>,
    output: Option<mpsc::Sender<String>>,
    tasks: ToolTasks,
) -> ToolOutput {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return ToolOutput::error("tool arguments exceed the 64 KiB limit");
    }
    let cancelled = ToolCancellation::new(cancelled);
    let _cancel_on_drop = CancelCallOnDrop(Arc::clone(&cancelled.call));
    // Shell executes on the async runtime directly: it awaits a child process
    // rather than doing blocking filesystem work, so it must neither occupy a
    // blocking permit for its full (possibly 120 s) lifetime nor block a
    // worker thread.
    if matches!(BuiltInTool::from_name(&name), Some(BuiltInTool::Shell)) {
        let arguments = match serde_json::from_str::<ShellArgs>(&arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolOutput::error(format!("invalid arguments: {error}"));
            }
        };
        let lease = tasks.enter();
        return match tokio::spawn(async move {
            let mut lease = lease;
            run_shell(
                &workspace,
                &arguments,
                &cancelled,
                output.as_ref(),
                &mut lease.process_pending,
            )
            .await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => ToolOutput::error("tool execution stopped unexpectedly"),
        };
    }
    let permit = match blocking_permits().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return ToolOutput::error("tool executor is unavailable"),
    };
    let lease = tasks.enter();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _lease = lease;
        execute_blocking(&workspace, &file_state, &name, &arguments, &cancelled)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => ToolOutput::error("tool execution stopped unexpectedly"),
    }
}

/// Wraps an externally produced result (an MCP call outcome) in the same
/// masking and bounding as built-in tool executions.
pub(crate) fn bounded_result(content: String, is_error: bool) -> ToolOutput {
    if is_error {
        ToolOutput::error(content)
    } else {
        ToolOutput::success(content)
    }
}

/// What one tool call produced. `model_text` is the only part that enters
/// model context; `ui_payload` is persisted for clients and never sent back
/// to the model. Constructors are the one bounding boundary: they mask
/// secrets, then bound the text to the tool's [`Bounds`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutput {
    pub(crate) model_text: String,
    pub(crate) is_error: bool,
    pub(crate) ui_payload: Option<ToolCallDisplay>,
    /// Set when the execution (re)recorded a file's content hash, so the
    /// session store can persist the file-state map alongside the result.
    pub(crate) file_state: Option<FileStateUpdate>,
}

impl ToolOutput {
    #[inline]
    pub(super) fn success(text: String) -> Self {
        Self::bounded(text, &Bounds::DEFAULT, false)
    }

    #[inline]
    pub(super) fn error(message: impl Into<String>) -> Self {
        Self::bounded(message.into(), &Bounds::DEFAULT, true)
    }

    pub(super) fn bounded(text: String, bounds: &Bounds, is_error: bool) -> Self {
        Self {
            model_text: bound_text(mask_secrets(text), bounds, None).text,
            is_error,
            ui_payload: None,
            file_state: None,
        }
    }

    /// A result that bypasses masking and bounding: fixed runtime messages
    /// (denials, interruption) whose text is a constant.
    pub(crate) fn verbatim_error(message: String) -> Self {
        Self {
            model_text: message,
            is_error: true,
            ui_payload: None,
            file_state: None,
        }
    }
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestDelayArgs {
    delay_ms: u64,
    result: String,
    #[serde(default)]
    synchronize: bool,
}

#[cfg(test)]
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TestMutateArgs {
    delay_ms: u64,
    result: Option<String>,
}

#[cfg(test)]
pub(crate) fn test_executions_started() -> usize {
    TEST_EXECUTIONS_STARTED.load(Ordering::Acquire)
}

#[inline]
pub(super) fn execute_blocking(
    workspace: &Workspace,
    file_state: &FileState,
    name: &str,
    arguments: &str,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if cancelled.is_cancelled() {
        return ToolOutput::error("tool execution was cancelled");
    }
    match BuiltInTool::from_name(name) {
        Some(BuiltInTool::ReadFile) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| {
                read_file(workspace, file_state, args, cancelled)
            }),
        Some(BuiltInTool::ListDir) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| {
                list_dir(workspace, args, cancelled)
            }),
        Some(BuiltInTool::Search) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| search(workspace, args, cancelled)),
        // The applied change is rendered once as a UI payload; the model gets
        // the one-line summary, never the diff it just wrote.
        Some(BuiltInTool::EditFile) => {
            deserialize(arguments).map_or_else(ToolOutput::error, |args| {
                let mut output = edit_file(workspace, file_state, &args, cancelled);
                if !output.is_error {
                    output.ui_payload = edit_result_display(name, arguments);
                }
                output
            })
        }
        Some(BuiltInTool::WriteFile) => {
            deserialize(arguments).map_or_else(ToolOutput::error, |args| {
                let mut output = write_file(workspace, file_state, &args, cancelled);
                if !output.is_error {
                    output.ui_payload = edit_result_display(name, arguments);
                }
                output
            })
        }
        Some(BuiltInTool::Shell) => ToolOutput::error("shell commands must execute asynchronously"),
        #[cfg(test)]
        Some(BuiltInTool::TestDelay) => {
            let arguments: TestDelayArgs = match deserialize(arguments) {
                Ok(arguments) => arguments,
                Err(error) => return ToolOutput::error(error),
            };
            TEST_EXECUTIONS_STARTED.fetch_add(1, Ordering::Release);
            if arguments.synchronize {
                TEST_EXECUTION_BARRIER
                    .get_or_init(|| std::sync::Barrier::new(2))
                    .wait();
            }
            for _ in 0..arguments.delay_ms {
                if cancelled.is_cancelled() {
                    return ToolOutput::error("tool execution was cancelled");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ToolOutput::success(arguments.result)
        }
        #[cfg(test)]
        Some(BuiltInTool::TestMutate) => {
            let arguments: TestMutateArgs = match deserialize(arguments) {
                Ok(arguments) => arguments,
                Err(error) => return ToolOutput::error(error),
            };
            TEST_EXECUTIONS_STARTED.fetch_add(1, Ordering::Release);
            for _ in 0..arguments.delay_ms {
                if cancelled.is_cancelled() {
                    return ToolOutput::error("tool execution was cancelled");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ToolOutput::success(arguments.result.unwrap_or_else(|| "mutated".to_owned()))
        }
        #[cfg(test)]
        Some(BuiltInTool::TestShell) => ToolOutput::success("shell ran".to_owned()),
        None => ToolOutput::error(format!("unknown tool {name:?}")),
    }
}

fn deserialize<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))
}
