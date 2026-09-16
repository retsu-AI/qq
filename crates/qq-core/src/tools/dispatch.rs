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
    RunCancellation,
    workspace::{FileState, FileStateUpdate, Workspace, blocking_permits},
};

use super::{
    edit::edit_file,
    output::{Bounds, MAX_SPILL_ITEM_BYTES, bound_text, mask_secrets},
    read::read_file,
    search::search,
    shell::{ExecArgs, Launch, ShellArgs, run_shell},
    specs::BuiltInTool,
    tree::tree,
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

/// Why a tool call should stop: the run was cancelled, or this call's own
/// future was dropped (the caller gave up on it). Both halves wake waiters;
/// blocking tools check [`Self::is_cancelled`] between steps and the shell
/// awaits [`Self::cancelled`] alongside its child process.
pub(super) struct ToolCancellation {
    run: RunCancellation,
    call: Arc<CallCancellation>,
}

#[derive(Default)]
struct CallCancellation {
    cancelled: AtomicBool,
    wake: Notify,
}

impl ToolCancellation {
    pub(super) fn new(run: RunCancellation) -> Self {
        Self {
            run,
            call: Arc::new(CallCancellation::default()),
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.run.is_cancelled() || self.call.cancelled.load(Ordering::Acquire)
    }

    /// Resolves when either the run is cancelled or the call was dropped.
    pub(super) async fn cancelled(&self) {
        let caller_dropped = async {
            loop {
                let wake = self.call.wake.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.call.cancelled.load(Ordering::Acquire) {
                    return;
                }
                wake.await;
            }
        };
        tokio::select! {
            () = self.run.cancelled() => {}
            () = caller_dropped => {}
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

#[expect(
    clippy::too_many_arguments,
    reason = "one dispatch per call; every argument is a distinct capability the tool receives"
)]
pub(crate) async fn execute(
    workspace: Workspace,
    file_state: Arc<FileState>,
    name: String,
    arguments: String,
    cancelled: RunCancellation,
    output: Option<mpsc::Sender<String>>,
    tasks: ToolTasks,
    shell_policy: Arc<crate::runtime::ShellPolicy>,
    network_policy: Arc<super::network::NetworkPolicy>,
) -> ToolOutput {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return ToolOutput::error("tool arguments exceed the 64 KiB limit");
    }
    let cancelled = ToolCancellation::new(cancelled);
    let _cancel_on_drop = CancelCallOnDrop(Arc::clone(&cancelled.call));
    // Fetch awaits sockets, not files: it runs on the async runtime like
    // shell, under the same task lease, never on a blocking permit.
    if BuiltInTool::from_name(&name) == Some(BuiltInTool::Fetch) {
        let args: super::fetch::FetchArgs = match serde_json::from_str(&arguments) {
            Ok(args) => args,
            Err(error) => return ToolOutput::error(format!("invalid arguments: {error}")),
        };
        let lease = tasks.enter();
        return match tokio::spawn(async move {
            let _lease = lease;
            super::fetch::fetch(args, network_policy, &cancelled).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => ToolOutput::error("tool execution stopped unexpectedly"),
        };
    }
    // Shell executes on the async runtime directly: it awaits a child process
    // rather than doing blocking filesystem work, so it must neither occupy a
    // blocking permit for its full (possibly 120 s) lifetime nor block a
    // worker thread.
    let launch_tool = BuiltInTool::from_name(&name);
    if matches!(launch_tool, Some(BuiltInTool::Shell | BuiltInTool::Exec)) {
        let is_exec = launch_tool == Some(BuiltInTool::Exec);
        let shell_arguments = match (is_exec, serde_json::from_str::<ShellArgs>(&arguments)) {
            (false, Ok(arguments)) => Some(arguments),
            (false, Err(error)) => {
                return ToolOutput::error(format!("invalid arguments: {error}"));
            }
            (true, _) => None,
        };
        let exec_arguments = match (is_exec, serde_json::from_str::<ExecArgs>(&arguments)) {
            (true, Ok(arguments)) => Some(arguments),
            (true, Err(error)) => {
                return ToolOutput::error(format!("invalid arguments: {error}"));
            }
            (false, _) => None,
        };
        let lease = tasks.enter();
        return match tokio::spawn(async move {
            let mut lease = lease;
            let launch = match (&shell_arguments, &exec_arguments) {
                (Some(shell), _) => Launch::Shell(shell),
                (_, Some(exec)) => Launch::Exec(exec),
                (None, None) => unreachable!("one launch shape was parsed"),
            };
            run_shell(
                &workspace,
                &shell_policy,
                launch,
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
    /// Every file whose content hash the execution (re)recorded, so the
    /// session store can persist the file-state map alongside the result.
    /// Reads and writes record one; a batch edit records each file touched.
    pub(crate) file_states: Vec<FileStateUpdate>,
    /// The complete masked text when bounding cut any of it, for the session
    /// store to keep under a handle the marker names. `None` when the model
    /// text is complete or the text exceeds what the store keeps.
    pub(crate) spill: Option<SpillRecord>,
}

/// A complete tool output awaiting storage: the text, its SHA-256 (the
/// handle's digest), and the line the bounded text omits from so the marker
/// can point at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpillRecord {
    pub(crate) text: String,
    pub(crate) digest: String,
    pub(crate) omitted_from_line: usize,
}

impl SpillRecord {
    /// The marker token for this record under `call`:
    /// `t:<tool>:<call8>:<digest8>`.
    pub(crate) fn handle(&self, tool: &str, call: qq_protocol::ToolCallId) -> String {
        let call = call.to_string();
        format!("t:{tool}:{}:{}", &call[..8], &self.digest[..8])
    }
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
        // The complete text is kept only when the size alone says a cut may
        // happen: cloning every small result would cost on the hot path for
        // nothing. It is kept unmasked — an explicit `read_tool_result` is
        // the model asking for exact bytes of something it produced — while
        // the inline preview is masked as before.
        let spill_source =
            (text.len() > bounds.max_bytes || text.len() > bounds.max_lines).then(|| text.clone());
        let bounded = bound_text(mask_secrets(text), bounds, None);
        let spill = spill_source
            .filter(|text| bounded.omitted_bytes > 0 && text.len() <= MAX_SPILL_ITEM_BYTES)
            .map(|text| SpillRecord {
                digest: crate::workspace::content_hash(text.as_bytes()),
                text,
                omitted_from_line: bounded.omitted_from_line,
            });
        Self {
            model_text: bounded.text,
            is_error,
            ui_payload: None,
            file_states: Vec::new(),
            spill,
        }
    }

    /// A bounded but unmasked result: an explicit read of stored output the
    /// model already produced, where masking would defeat the read. Never
    /// spills — the page is itself bounded on whole lines by its renderer.
    pub(crate) fn exact(text: String, bounds: &Bounds) -> Self {
        Self {
            model_text: bound_text(text, bounds, None).text,
            is_error: false,
            ui_payload: None,
            file_states: Vec::new(),
            spill: None,
        }
    }

    /// A result that bypasses masking and bounding: fixed runtime messages
    /// (denials, interruption) whose text is a constant.
    pub(crate) fn verbatim_error(message: String) -> Self {
        Self {
            model_text: message,
            is_error: true,
            ui_payload: None,
            file_states: Vec::new(),
            spill: None,
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
        Some(BuiltInTool::Tree) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| tree(workspace, args, cancelled)),
        Some(BuiltInTool::Search) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| search(workspace, args, cancelled)),
        // The applied change travels as a UI payload (the unified diff of what
        // actually changed on disk); the model gets the header summary.
        Some(BuiltInTool::EditFile) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| {
                edit_file(workspace, file_state, &args, cancelled)
            }),
        Some(BuiltInTool::WriteFile) => deserialize(arguments)
            .map_or_else(ToolOutput::error, |args| {
                write_file(workspace, file_state, &args, cancelled)
            }),
        Some(BuiltInTool::Shell | BuiltInTool::Exec | BuiltInTool::Fetch) => {
            ToolOutput::error("network and shell tools must execute asynchronously")
        }
        // A well-formed `ask_user` never reaches dispatch: the gate holds it
        // and the answer is its result. Only malformed calls fall through,
        // so this is where the model learns what was wrong.
        Some(BuiltInTool::AskUser) => match super::ask::parse(arguments) {
            Ok(_) => ToolOutput::error("ask_user was not put to the user"),
            Err(error) => ToolOutput::error(error.to_string()),
        },
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
