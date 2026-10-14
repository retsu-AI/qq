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

    pub(crate) fn enter(&self) -> ToolTaskLease {
        self.0.active.fetch_add(1, Ordering::AcqRel);
        ToolTaskLease {
            tasks: Arc::clone(&self.0),
            process_pending: false,
        }
    }
}

pub(crate) struct ToolTaskLease {
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
#[derive(Clone)]
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

#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "default-policy test entry point")]
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
    execute_with_images(
        workspace,
        file_state,
        name,
        arguments,
        cancelled,
        output,
        tasks,
        shell_policy,
        network_policy,
        Arc::default(),
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "one dispatch per call; every argument is a distinct capability the tool receives"
)]
pub(crate) async fn execute_with_images(
    workspace: Workspace,
    file_state: Arc<FileState>,
    name: String,
    arguments: String,
    cancelled: RunCancellation,
    output: Option<mpsc::Sender<String>>,
    tasks: ToolTasks,
    shell_policy: Arc<crate::runtime::ShellPolicy>,
    network_policy: Arc<super::network::NetworkPolicy>,
    approved_read_images: Arc<crate::runtime::ApprovedReadImages>,
) -> ToolOutput {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return ToolOutput::error("tool arguments exceed the 64 KiB limit");
    }
    let cancelled = ToolCancellation::new(cancelled);
    let _cancel_on_drop = CancelCallOnDrop(Arc::clone(&cancelled.call));
    // Fetch awaits sockets, not files: it runs on the async runtime like
    // shell, under the same task lease, never on a blocking permit.
    #[cfg(not(feature = "tool-fetch"))]
    let _ = network_policy;
    #[cfg(feature = "tool-fetch")]
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
                approved_read_images,
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
    pub(crate) execution_context: Option<Box<super::process_context::ExecutionContextReceipt>>,
}

/// Complete read-side domain output. Dispatch adds any argument-repair
/// note before masking and bounding once; a clipped preview is never treated
/// as complete input for another bound.
pub(super) struct ReadSideOutput {
    text: String,
    bounds: Bounds,
    pub(super) is_error: bool,
    pub(super) ui_payload: Option<ToolCallDisplay>,
    pub(super) file_states: Vec<FileStateUpdate>,
}

impl ReadSideOutput {
    #[cfg(test)]
    pub(super) fn with_test_notes(self, notes: &[&str]) -> ToolOutput {
        with_argument_notes(self, notes)
    }

    pub(super) fn success(text: String) -> Self {
        Self::with_bounds(text, &Bounds::DEFAULT, false)
    }

    pub(super) fn error(message: impl Into<String>) -> Self {
        Self::with_bounds(message.into(), &Bounds::DEFAULT, true)
    }

    pub(super) fn with_bounds(text: String, bounds: &Bounds, is_error: bool) -> Self {
        Self {
            text,
            bounds: *bounds,
            is_error,
            ui_payload: None,
            file_states: Vec::new(),
        }
    }
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
        super::output::result_handle(tool, call, &self.digest)
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
            execution_context: None,
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
            execution_context: None,
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
            execution_context: None,
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
        Some(BuiltInTool::ReadFile) => deserialize_read_only(name, arguments).map_or_else(
            ToolOutput::error,
            |(args, notes)| {
                let note_bytes = if notes.is_empty() {
                    0
                } else {
                    6 + notes.iter().map(|note| note.len()).sum::<usize>() + notes.len() - 1
                };
                with_argument_notes(
                    read_file(workspace, file_state, args, cancelled, note_bytes),
                    &notes,
                )
            },
        ),
        Some(BuiltInTool::Tree) => deserialize_read_only(name, arguments)
            .map_or_else(ToolOutput::error, |(args, notes)| {
                with_argument_notes(tree(workspace, args, cancelled), &notes)
            }),
        Some(BuiltInTool::Search) => deserialize_read_only(name, arguments)
            .map_or_else(ToolOutput::error, |(args, notes)| {
                with_argument_notes(search(workspace, args, cancelled), &notes)
            }),
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
        #[cfg(feature = "tool-fetch")]
        Some(BuiltInTool::Fetch) => ToolOutput::error("network tools must execute asynchronously"),
        Some(BuiltInTool::Shell | BuiltInTool::Exec) => {
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

// Value's default object decoder overwrites duplicate keys. A failed strict
// decode must never turn an ambiguous path or query into an accepted call.
struct ReadOnlyObject(serde_json::Map<String, serde_json::Value>);

impl<'de> serde::Deserialize<'de> for ReadOnlyObject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;

        impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
            type Value = ReadOnlyObject;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique argument fields")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut access: M,
            ) -> Result<Self::Value, M::Error> {
                let mut object = serde_json::Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    if object.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate argument field {key:?}"
                        )));
                    }
                    object.insert(key, access.next_value()?);
                }
                Ok(ReadOnlyObject(object))
            }
        }

        deserializer.deserialize_map(ObjectVisitor)
    }
}

fn deserialize_read_only<T: serde::de::DeserializeOwned>(
    name: &str,
    arguments: &str,
) -> Result<(T, Vec<&'static str>), String> {
    let original_error = match serde_json::from_str(arguments) {
        Ok(args) => return Ok((args, Vec::new())),
        Err(error) => {
            if !error.is_data() {
                return Err(format!("invalid arguments: {error}"));
            }
            format!("invalid arguments: {error}")
        }
    };
    let ReadOnlyObject(mut object) = match serde_json::from_str(arguments) {
        Ok(object) => object,
        Err(error) => return Err(format!("invalid arguments: {error}")),
    };
    let mut notes = Vec::new();
    let arrays: &[(&str, &str)] = match name {
        "search" => &[
            ("include", "include_array_coerced"),
            ("exclude", "exclude_array_coerced"),
        ],
        "read_file" => &[("ranges", "ranges_array_coerced")],
        "tree" => &[],
        _ => return Err(original_error),
    };
    for &(field, note) in arrays {
        if let Some(serde_json::Value::String(encoded)) = object.get(field) {
            let values: Vec<String> = match serde_json::from_str(encoded) {
                Ok(values) => values,
                Err(_) => return Err(original_error),
            };
            object.insert(field.to_owned(), serde_json::Value::from(values));
            notes.push(note);
        }
    }
    // These unknown values explicitly select the existing workspace root or
    // first search page. Any non-neutral value still fails strict decoding.
    if object
        .get("cwd")
        .is_some_and(|value| value.as_str() == Some("."))
    {
        object.remove("cwd");
        notes.push("cwd_ignored");
    }
    if name == "search"
        && object
            .get("offset")
            .is_some_and(|value| value.as_u64() == Some(0))
    {
        object.remove("offset");
        notes.push("offset_ignored");
    }
    // An ignore-walk flag has no effect on reading a named file. The
    // observed boolean shape is redundant; other types remain invalid.
    if name == "read_file"
        && object
            .get("include_ignored")
            .is_some_and(serde_json::Value::is_boolean)
    {
        object.remove("include_ignored");
        notes.push("include_ignored_ignored");
    }
    if notes.is_empty() {
        return Err(original_error);
    }
    match serde_json::from_value(serde_json::Value::Object(object)) {
        Ok(args) => Ok((args, notes)),
        Err(error) => Err(format!("invalid arguments: {error}")),
    }
}

fn with_argument_notes(mut output: ReadSideOutput, notes: &[&str]) -> ToolOutput {
    if !notes.is_empty() {
        let end = output.text.find('\n').unwrap_or(output.text.len());
        let search = output.text.starts_with("search ");
        let position = if search {
            output.text[..end].rfind(" mode=").unwrap_or(end)
        } else {
            end
        };
        let note = format!(" note={}", notes.join(","));
        let header_cap = if search {
            super::output::MAX_STUB_HEADER_BYTES.min(output.bounds.max_line_bytes)
        } else {
            output.bounds.max_line_bytes
        };
        if end + note.len() > header_cap {
            output
                .text
                .insert_str(end, &format!("\nnote={}", notes.join(",")));
        } else {
            output.text.insert_str(position, &note);
        }
    }
    let mut result = ToolOutput::bounded(output.text, &output.bounds, output.is_error);
    result.ui_payload = output.ui_payload;
    result.file_states = output.file_states;
    result
}
