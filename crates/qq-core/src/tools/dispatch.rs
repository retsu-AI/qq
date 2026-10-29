use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};

use qq_protocol::ToolCallDisplay;
use sha2::{Digest, Sha256};

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

#[derive(Clone, Default)]
pub(crate) struct ToolTasks(Arc<ToolTaskState>);

#[derive(Default)]
struct ToolTaskState {
    process_recorder: OnceLock<ProcessRecorder>,
    finished: std::sync::Mutex<std::collections::HashMap<qq_protocol::ToolCallId, ToolOutput>>,
    commands: super::commands::Commands,
    unrecorded_process: AtomicBool,
    active: AtomicUsize,
    calls: AtomicUsize,
    idle: Notify,
    unconfirmed_process_exit: AtomicBool,
}

pub(super) type RecordFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), crate::sessions::SessionRuntimeError>> + Send>,
>;
type ProcessRecorder = Box<
    dyn Fn(
            qq_protocol::ToolCallId,
            Box<super::process_context::ExecutionContextReceipt>,
        ) -> RecordFuture
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ToolDrainError {
    #[error("the command process receipt could not be persisted")]
    UnrecordedProcess,
    #[error("local shell process termination could not be confirmed")]
    UnconfirmedProcessExit,
}

impl ToolTasks {
    pub(crate) fn finished_result(&self, id: qq_protocol::ToolCallId) -> Option<ToolOutput> {
        self.0
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    pub(crate) fn forget_finished(&self, id: qq_protocol::ToolCallId) {
        self.0
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    pub(crate) fn take_finished(&self) -> Vec<(qq_protocol::ToolCallId, ToolOutput)> {
        self.0
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .collect()
    }

    pub(crate) fn set_process_recorder<F, Fut>(&self, recorder: F) -> Result<(), ToolDrainError>
    where
        F: Fn(qq_protocol::ToolCallId, Box<super::process_context::ExecutionContextReceipt>) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: std::future::Future<Output = Result<(), crate::sessions::SessionRuntimeError>>
            + Send
            + 'static,
    {
        self.0
            .process_recorder
            .set(Box::new(move |id, receipt| Box::pin(recorder(id, receipt))))
            .map_err(|_| ToolDrainError::UnrecordedProcess)
    }

    pub(crate) fn check(&self) -> Result<(), ToolDrainError> {
        if self.0.unconfirmed_process_exit.load(Ordering::Acquire) {
            Err(ToolDrainError::UnconfirmedProcessExit)
        } else if self.0.unrecorded_process.load(Ordering::Acquire) {
            Err(ToolDrainError::UnrecordedProcess)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn drain(&self) -> Result<(), ToolDrainError> {
        self.0.commands.cancel_all();
        self.wait_until(|state| state.active.load(Ordering::Acquire) == 0)
            .await
    }

    pub(crate) async fn drain_calls(&self) -> Result<(), ToolDrainError> {
        self.wait_until(|state| state.calls.load(Ordering::Acquire) == 0)
            .await
    }

    async fn wait_until(
        &self,
        done: impl Fn(&ToolTaskState) -> bool,
    ) -> Result<(), ToolDrainError> {
        loop {
            let idle = self.0.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if done(&self.0) {
                return self.check();
            }
            idle.await;
        }
    }

    pub(crate) fn commands(&self) -> &super::commands::Commands {
        &self.0.commands
    }

    pub(crate) fn enter(&self) -> ToolTaskLease {
        self.0.active.fetch_add(1, Ordering::AcqRel);
        self.0.calls.fetch_add(1, Ordering::AcqRel);
        ToolTaskLease {
            tasks: Arc::clone(&self.0),
            process_pending: false,
            call: true,
        }
    }
}

pub(crate) struct ToolTaskLease {
    tasks: Arc<ToolTaskState>,
    process_pending: bool,
    call: bool,
}

impl ToolTaskLease {
    #[cfg(windows)]
    pub(super) fn fork(&self) -> Self {
        self.tasks.active.fetch_add(1, Ordering::AcqRel);
        Self {
            tasks: Arc::clone(&self.tasks),
            process_pending: false,
            call: false,
        }
    }

    pub(super) fn yielded(&mut self) {
        if std::mem::take(&mut self.call) {
            self.tasks.calls.fetch_sub(1, Ordering::AcqRel);
            self.tasks.idle.notify_waiters();
        }
    }

    pub(super) fn save_finished(&self, id: qq_protocol::ToolCallId, result: &ToolOutput) {
        self.tasks
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, result.clone());
    }

    pub(crate) fn record_process(
        &self,
        id: qq_protocol::ToolCallId,
        receipt: Box<super::process_context::ExecutionContextReceipt>,
    ) -> RecordFuture {
        let Some(recorder) = self.tasks.process_recorder.get() else {
            return Box::pin(async { Ok(()) });
        };
        self.record(recorder(id, receipt))
    }

    fn record(&self, write: RecordFuture) -> RecordFuture {
        let mut record = RecordGuard {
            tasks: Arc::clone(&self.tasks),
            completed: false,
        };
        Box::pin(async move {
            let result = write.await;
            record.completed = result.is_ok();
            drop(record);
            result
        })
    }

    pub(crate) fn process_started(&mut self) {
        self.process_pending = true;
    }
    pub(crate) fn process_reaped(&mut self) {
        self.process_pending = false;
    }
}

struct RecordGuard {
    tasks: Arc<ToolTaskState>,
    completed: bool,
}
impl Drop for RecordGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.tasks.unrecorded_process.store(true, Ordering::Release);
        }
    }
}

impl Drop for ToolTaskLease {
    fn drop(&mut self) {
        if self.process_pending {
            self.tasks
                .unconfirmed_process_exit
                .store(true, Ordering::Release);
        }
        if self.call {
            self.tasks.calls.fetch_sub(1, Ordering::AcqRel);
        }
        self.tasks.active.fetch_sub(1, Ordering::AcqRel);
        self.tasks.idle.notify_waiters();
    }
}

#[derive(Clone)]
pub(crate) struct ToolCancellation {
    run: RunCancellation,
    call: Arc<CallCancellation>,
}

#[derive(Default)]
pub(super) struct CallCancellation {
    cancelled: AtomicBool,
    wake: Notify,
}

impl CallCancellation {
    #[cfg(test)]
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }
}

impl ToolCancellation {
    pub(crate) fn new(run: RunCancellation) -> Self {
        Self {
            run,
            call: Arc::new(CallCancellation::default()),
        }
    }

    pub(crate) fn timed_out(&self) -> bool {
        self.run.timed_out()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.run.is_cancelled() || self.call.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
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

struct CancelCallOnDrop(Option<Arc<CallCancellation>>);

impl Drop for CancelCallOnDrop {
    fn drop(&mut self) {
        if let Some(call) = &self.0 {
            call.cancel();
        }
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
        Arc::default(),
        None,
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
    process_producer: Arc<crate::ProcessProducer>,
    call_id: Option<qq_protocol::ToolCallId>,
) -> ToolOutput {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return ToolOutput::refused("tool arguments exceed the 64 KiB limit");
    }
    let cancelled = ToolCancellation::new(cancelled);
    let mut cancel_on_drop = CancelCallOnDrop(Some(Arc::clone(&cancelled.call)));
    #[cfg(not(feature = "tool-fetch"))]
    let _ = network_policy;
    #[cfg(feature = "tool-fetch")]
    if BuiltInTool::from_name(&name) == Some(BuiltInTool::Fetch) {
        let args: super::fetch::FetchArgs = match serde_json::from_str(&arguments) {
            Ok(args) => args,
            Err(error) => return ToolOutput::refused(format!("invalid arguments: {error}")),
        };
        let lease = tasks.enter();
        return match tokio::spawn(async move {
            let _lease = lease;
            super::fetch::fetch(args, network_policy, &cancelled).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => ToolOutput::unobserved("tool execution stopped unexpectedly"),
        };
    }
    let launch_tool = BuiltInTool::from_name(&name);
    if matches!(launch_tool, Some(BuiltInTool::Shell | BuiltInTool::Exec)) {
        let is_exec = launch_tool == Some(BuiltInTool::Exec);
        let shell_arguments = match (is_exec, serde_json::from_str::<ShellArgs>(&arguments)) {
            (false, Ok(arguments)) => Some(arguments),
            (false, Err(error)) => {
                return ToolOutput::refused(format!("invalid arguments: {error}"));
            }
            (true, _) => None,
        };
        let exec_arguments = match (is_exec, serde_json::from_str::<ExecArgs>(&arguments)) {
            (true, Ok(arguments)) => Some(arguments),
            (true, Err(error)) => {
                return ToolOutput::refused(format!("invalid arguments: {error}"));
            }
            (false, _) => None,
        };
        let lease = tasks.enter();
        let (early, mut yielded) = tokio::sync::oneshot::channel();
        let mut yielding = super::shell::Yielding {
            tasks: tasks.clone(),
            call: Arc::clone(&cancelled.call),
            early: Some(early),
            command: None,
        };
        let execution = tokio::spawn(async move {
            let mut lease = lease;
            let launch = match (&shell_arguments, &exec_arguments) {
                (Some(shell), _) => Launch::Shell(shell),
                (_, Some(exec)) => Launch::Exec(exec),
                (None, None) => unreachable!("one launch shape was parsed"),
            };
            let result = run_shell(
                &workspace,
                &shell_policy,
                approved_read_images,
                process_producer,
                call_id,
                launch,
                &cancelled,
                output.as_ref(),
                &mut lease,
                Some(&mut yielding),
            )
            .await;
            match &yielding.command {
                Some(_) => {}
                None => {
                    if let Some(id) = call_id
                        && result.native_outcome.is_some()
                    {
                        lease.save_finished(id, &result);
                    }
                }
            }
            result
        });
        tokio::pin!(execution);
        return tokio::select! {
            biased;
            Ok(result) = &mut yielded => {
                cancel_on_drop.0 = None;
                result
            }
            result = &mut execution => match result {
                Ok(result) => result,
                Err(_) => ToolOutput::unobserved("tool execution stopped unexpectedly"),
            },
        };
    }
    let permit = match blocking_permits().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return ToolOutput::refused("tool executor is unavailable"),
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
        Err(_) => ToolOutput::unobserved("tool execution stopped unexpectedly"),
    }
}

pub(crate) fn bounded_result(content: String, is_error: bool) -> ToolOutput {
    if is_error {
        ToolOutput::error(content)
    } else {
        ToolOutput::success(content)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CommandOutcome {
    Exited { code: Option<i32> },
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ResultDisposition {
    Observed,
    Refused,
    Unobserved,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) native_outcome: Option<CommandOutcome>,
    pub(crate) disposition: ResultDisposition,
    pub(crate) result_digest: [u8; 32],
    pub(crate) model_text: String,
    pub(crate) is_error: bool,
    pub(crate) ui_payload: Option<ToolCallDisplay>,
    pub(crate) file_states: Vec<FileStateUpdate>,
    pub(crate) spill: Option<SpillRecord>,
    pub(crate) execution_context: Option<Box<super::process_context::ExecutionContextReceipt>>,
}

pub(super) struct ReadSideOutput {
    pub(super) result_digest: Option<[u8; 32]>,
    disposition: ResultDisposition,
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

    pub(super) fn refused(message: impl Into<String>) -> Self {
        let mut result = Self::error(message);
        result.disposition = ResultDisposition::Refused;
        result
    }

    pub(super) fn interrupted(message: impl Into<String>) -> Self {
        let mut result = Self::error(message);
        result.disposition = ResultDisposition::Interrupted;
        result
    }

    pub(super) fn with_bounds(text: String, bounds: &Bounds, is_error: bool) -> Self {
        Self {
            result_digest: None,
            disposition: ResultDisposition::Observed,
            text,
            bounds: *bounds,
            is_error,
            ui_payload: None,
            file_states: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SpillRecord {
    pub(crate) text: String,
    pub(crate) digest: String,
    pub(crate) omitted_from_line: usize,
}

impl SpillRecord {
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

    pub(crate) fn refused(message: impl Into<String>) -> Self {
        let mut result = Self::error(message);
        result.disposition = ResultDisposition::Refused;
        result
    }

    pub(crate) fn unobserved(message: impl Into<String>) -> Self {
        let mut result = Self::error(message);
        result.disposition = ResultDisposition::Unobserved;
        result
    }

    pub(crate) fn interrupted(message: impl Into<String>) -> Self {
        let mut result = Self::error(message);
        result.disposition = ResultDisposition::Interrupted;
        result
    }

    pub(super) fn bounded(text: String, bounds: &Bounds, is_error: bool) -> Self {
        let digest = Sha256::digest(text.as_bytes());
        let spill_source =
            (text.len() > bounds.max_bytes || text.len() > bounds.max_lines).then(|| text.clone());
        let bounded = bound_text(mask_secrets(text), bounds, None);
        let spill = spill_source
            .filter(|text| bounded.omitted_bytes > 0 && text.len() <= MAX_SPILL_ITEM_BYTES)
            .map(|text| SpillRecord {
                digest: format!("{digest:x}"),
                text,
                omitted_from_line: bounded.omitted_from_line,
            });
        Self {
            disposition: ResultDisposition::Observed,
            result_digest: digest.into(),
            model_text: bounded.text,
            is_error,
            ui_payload: None,
            file_states: Vec::new(),
            spill,
            execution_context: None,
            native_outcome: None,
        }
    }

    pub(crate) fn exact(text: String, bounds: &Bounds) -> Self {
        Self {
            disposition: ResultDisposition::Observed,
            result_digest: Sha256::digest(text.as_bytes()).into(),
            model_text: bound_text(text, bounds, None).text,
            is_error: false,
            ui_payload: None,
            file_states: Vec::new(),
            spill: None,
            execution_context: None,
            native_outcome: None,
        }
    }

    pub(crate) fn verbatim_error(message: String) -> Self {
        Self {
            disposition: ResultDisposition::Refused,
            result_digest: Sha256::digest(message.as_bytes()).into(),
            model_text: message,
            is_error: true,
            ui_payload: None,
            file_states: Vec::new(),
            spill: None,
            execution_context: None,
            native_outcome: None,
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
        return ToolOutput::interrupted("tool execution was cancelled");
    }
    match BuiltInTool::from_name(name) {
        Some(BuiltInTool::ReadFile) => deserialize_read_only(name, arguments).map_or_else(
            ToolOutput::refused,
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
            .map_or_else(ToolOutput::refused, |(args, notes)| {
                with_argument_notes(tree(workspace, args, cancelled), &notes)
            }),
        Some(BuiltInTool::Search) => deserialize_read_only(name, arguments)
            .map_or_else(ToolOutput::refused, |(args, notes)| {
                with_argument_notes(search(workspace, args, cancelled), &notes)
            }),
        Some(BuiltInTool::EditFile) => deserialize(arguments)
            .map_or_else(ToolOutput::refused, |args| {
                edit_file(workspace, file_state, &args, cancelled)
            }),
        Some(BuiltInTool::WriteFile) => deserialize(arguments)
            .map_or_else(ToolOutput::refused, |args| {
                write_file(workspace, file_state, &args, cancelled)
            }),
        #[cfg(feature = "tool-fetch")]
        Some(BuiltInTool::Fetch) => {
            ToolOutput::refused("network tools must execute asynchronously")
        }
        Some(BuiltInTool::Shell | BuiltInTool::Exec) => {
            ToolOutput::refused("network and shell tools must execute asynchronously")
        }
        Some(BuiltInTool::AskUser) => match super::ask::parse(arguments) {
            Ok(_) => ToolOutput::refused("ask_user was not put to the user"),
            Err(error) => ToolOutput::refused(error.to_string()),
        },
        #[cfg(test)]
        Some(BuiltInTool::TestDelay) => {
            let arguments: TestDelayArgs = match deserialize(arguments) {
                Ok(arguments) => arguments,
                Err(error) => return ToolOutput::refused(error),
            };
            TEST_EXECUTIONS_STARTED.fetch_add(1, Ordering::Release);
            if arguments.synchronize {
                TEST_EXECUTION_BARRIER
                    .get_or_init(|| std::sync::Barrier::new(2))
                    .wait();
            }
            for _ in 0..arguments.delay_ms {
                if cancelled.is_cancelled() {
                    return ToolOutput::interrupted("tool execution was cancelled");
                }
                // lint: allow(sleep) the test tool's work is its blocking delay
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ToolOutput::success(arguments.result)
        }
        #[cfg(test)]
        Some(BuiltInTool::TestMutate) => {
            let arguments: TestMutateArgs = match deserialize(arguments) {
                Ok(arguments) => arguments,
                Err(error) => return ToolOutput::refused(error),
            };
            TEST_EXECUTIONS_STARTED.fetch_add(1, Ordering::Release);
            for _ in 0..arguments.delay_ms {
                if cancelled.is_cancelled() {
                    return ToolOutput::interrupted("tool execution was cancelled");
                }
                // lint: allow(sleep) the test tool's work is its blocking delay
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ToolOutput::success(arguments.result.unwrap_or_else(|| "mutated".to_owned()))
        }
        #[cfg(test)]
        Some(BuiltInTool::TestShell) => ToolOutput::success("shell ran".to_owned()),
        None => ToolOutput::refused(format!("unknown tool {name:?}")),
    }
}

fn deserialize<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

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
    result.disposition = output.disposition;
    if let Some(digest) = output.result_digest {
        result.result_digest = digest;
    }
    result.ui_payload = output.ui_payload;
    result.file_states = output.file_states;
    result
}
