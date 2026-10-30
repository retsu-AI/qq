use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, sync::mpsc};

mod child_exit;
#[cfg(unix)]
mod descendants;

#[cfg(windows)]
pub(crate) type NativeChild = super::windows_job::Child;
#[cfg(not(windows))]
pub(crate) type NativeChild = tokio::process::Child;

use crate::{
    runtime::{BuiltinPreference, ShellPolicy, builtin_alternative},
    workspace::Workspace,
};

use super::{
    dispatch::{ResultDisposition, ToolCancellation, ToolOutput},
    output::{Bounds, Header},
};

const MAX_LIVE_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_CAPTURE_BYTES: usize = super::output::MAX_SPILL_ITEM_BYTES - 64 * 1024;
const EXIT_DRAIN: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const SHELL_BOUNDS: Bounds = Bounds::new(16 * 1024, 4_000);
const SHELL_READ_CHUNK_BYTES: usize = 8 * 1024;
const OUTPUT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const PREPARATION_TIMEOUT_SECS: u64 = 120;

#[cfg(test)]
struct SpawnHook {
    workspace: std::path::PathBuf,
    entered: tokio::sync::oneshot::Sender<u32>,
    panic: bool,
}

#[cfg(test)]
static SPAWN_HOOKS: std::sync::Mutex<Vec<SpawnHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn observe_shell_spawn(
    workspace: &std::path::Path,
    panic: bool,
) -> tokio::sync::oneshot::Receiver<u32> {
    let (entered, receiver) = tokio::sync::oneshot::channel();
    SPAWN_HOOKS.lock().unwrap().push(SpawnHook {
        workspace: workspace.to_owned(),
        entered,
        panic,
    });
    receiver
}

#[cfg(all(test, unix))]
struct ReceiptHook {
    workspace: std::path::PathBuf,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(all(test, unix))]
static RECEIPT_HOOKS: std::sync::Mutex<Vec<ReceiptHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(all(test, unix))]
pub(crate) fn hold_shell_receipt(
    workspace: &std::path::Path,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, receiver) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    RECEIPT_HOOKS.lock().unwrap().push(ReceiptHook {
        workspace: workspace.to_owned(),
        entered,
        release: resume,
    });
    (receiver, release)
}

#[cfg(test)]
static READ_FAULTS: std::sync::Mutex<Vec<(std::path::PathBuf, bool)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static STOP_OBSERVERS: std::sync::Mutex<Vec<(u32, tokio::sync::oneshot::Sender<()>)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn observe_shell_termination(pid: u32) -> tokio::sync::oneshot::Receiver<()> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    STOP_OBSERVERS.lock().unwrap().push((pid, sender));
    receiver
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellArgs {
    pub(super) command: String,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) yield_after_seconds: Option<u64>,
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecArgs {
    pub(super) program: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    yield_after_seconds: Option<u64>,
}

pub(super) const MAX_EXEC_ARGS: usize = 64;
pub(super) const MAX_EXEC_ARG_BYTES: usize = 4096;
pub(super) const MAX_EXEC_PROGRAM_BYTES: usize = 256;
pub(super) const MAX_EXEC_STDIN_BYTES: usize = 64 * 1024;

pub(super) enum Launch<'a> {
    Shell(&'a ShellArgs),
    Exec(&'a ExecArgs),
}

impl Launch<'_> {
    pub(super) fn validate(&self) -> Result<(), InvalidLaunch> {
        match self {
            Self::Shell(args) => {
                if args.command.trim().is_empty() || args.command.contains('\0') {
                    return Err(InvalidLaunch::Command);
                }
            }
            Self::Exec(args) => {
                if args.program.trim().is_empty()
                    || args.program.len() > MAX_EXEC_PROGRAM_BYTES
                    || args.program.contains('\0')
                {
                    return Err(InvalidLaunch::Program);
                }
                if args.args.len() > MAX_EXEC_ARGS
                    || args
                        .args
                        .iter()
                        .any(|arg| arg.len() > MAX_EXEC_ARG_BYTES || arg.contains('\0'))
                {
                    return Err(InvalidLaunch::Args);
                }
                if args
                    .stdin
                    .as_ref()
                    .is_some_and(|text| text.len() > MAX_EXEC_STDIN_BYTES)
                {
                    return Err(InvalidLaunch::Stdin);
                }
            }
        }
        if let Some(timeout) = self.timeout_seconds()
            && (timeout == 0
                || std::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(timeout))
                    .is_none())
        {
            return Err(InvalidLaunch::Timeout);
        }
        if let Some(after) = self.yield_after_seconds()
            && (after == 0
                || std::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(after))
                    .is_none())
        {
            return Err(InvalidLaunch::YieldAfter);
        }
        let cwd = match self {
            Self::Shell(args) => args.cwd.as_deref(),
            Self::Exec(args) => args.cwd.as_deref(),
        };
        if cwd.is_some_and(|cwd| {
            cwd.is_empty() || cwd.contains('\0') || std::path::Path::new(cwd).is_absolute()
        }) {
            return Err(InvalidLaunch::Cwd);
        }
        if let Some(cwd) = cwd {
            let mut depth = 0_usize;
            for component in std::path::Path::new(cwd).components() {
                match component {
                    std::path::Component::Normal(_) => depth += 1,
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir if depth > 0 => depth -= 1,
                    _ => return Err(InvalidLaunch::Cwd),
                }
            }
        }
        Ok(())
    }

    pub(super) fn timeout_seconds(&self) -> Option<u64> {
        match self {
            Self::Shell(args) => args.timeout_seconds,
            Self::Exec(args) => args.timeout_seconds,
        }
    }

    fn yield_after_seconds(&self) -> Option<u64> {
        match self {
            Self::Shell(args) => args.yield_after_seconds,
            Self::Exec(args) => args.yield_after_seconds,
        }
    }

    fn nudge_text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Shell(args) => std::borrow::Cow::Borrowed(&args.command),
            Self::Exec(args) => {
                let mut text = args.program.clone();
                for arg in &args.args {
                    text.push(' ');
                    text.push_str(arg);
                }
                std::borrow::Cow::Owned(text)
            }
        }
    }

    fn header_tool(&self) -> &'static str {
        match self {
            Self::Shell(_) => "shell",
            Self::Exec(_) => "exec",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum InvalidLaunch {
    #[error("command must not be empty or contain NUL")]
    Command,
    #[error("invalid_program: program must be 1 to 256 bytes without NUL")]
    Program,
    #[error("invalid_args: at most 64 arguments of 4096 bytes without NUL")]
    Args,
    #[error("invalid_stdin: stdin exceeds 65536 bytes")]
    Stdin,
    #[error("timeout_seconds must be positive and fit a monotonic deadline")]
    Timeout,
    #[error("yield_after_seconds must be positive and fit a monotonic deadline")]
    YieldAfter,
    #[error("cwd must be a nonempty relative workspace path without NUL")]
    Cwd,
}

enum ShellOutcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    ReadFailed {
        stream: &'static str,
        source: std::io::Error,
    },
    TimedOut,
    Cancelled,
}

pub(super) struct Yielding {
    pub(super) tasks: super::ToolTasks,
    pub(super) call: std::sync::Arc<super::dispatch::CallCancellation>,
    pub(super) early: Option<tokio::sync::oneshot::Sender<ToolOutput>>,
    pub(super) command: Option<std::sync::Arc<super::commands::Command>>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "process identity, producer, image approvals and supervision are distinct capabilities"
)]
pub(super) async fn run_shell(
    workspace: &Workspace,
    policy: &ShellPolicy,
    approved_read_images: std::sync::Arc<crate::runtime::ApprovedReadImages>,
    process_producer: std::sync::Arc<crate::ProcessProducer>,
    call_id: Option<qq_protocol::ToolCallId>,
    launch: Launch<'_>,
    cancelled: &ToolCancellation,
    output: Option<&mpsc::Sender<String>>,
    process: &mut super::ToolTaskLease,
    mut yielding: Option<&mut Yielding>,
) -> ToolOutput {
    if cancelled.is_cancelled() {
        return ToolOutput::interrupted("tool execution was cancelled");
    }
    if let Err(error) = launch.validate() {
        return ToolOutput::refused(error.to_string());
    }
    let hint = match (
        policy.builtin_preference,
        builtin_alternative(&launch.nudge_text()),
    ) {
        (BuiltinPreference::Off, _) | (_, None) => None,
        (BuiltinPreference::Hint | BuiltinPreference::Strict, Some(alternative)) => Some(format!(
            "hint: use {} instead of {}; it is bounded, ignore-aware, and needs no approval",
            alternative.builtin, alternative.program
        )),
    };
    let timeout_seconds = launch.timeout_seconds();
    let preparation_started = std::time::Instant::now();
    let command_deadline = match timeout_seconds {
        Some(seconds) => {
            match preparation_started.checked_add(std::time::Duration::from_secs(seconds)) {
                Some(deadline) => Some(deadline),
                None => return ToolOutput::refused(InvalidLaunch::Timeout.to_string()),
            }
        }
        None => None,
    };
    let Some(preparation_deadline) = preparation_started.checked_add(
        std::time::Duration::from_secs(timeout_seconds.unwrap_or(PREPARATION_TIMEOUT_SECS)),
    ) else {
        return ToolOutput::refused(InvalidLaunch::Timeout.to_string());
    };
    let permit = tokio::select! {
        biased;
        () = cancelled.cancelled() => return ToolOutput::interrupted("tool execution was cancelled"),
        result = crate::workspace::blocking_permits().acquire_owned() => match result {
            Ok(permit) => permit,
            Err(_) => return ToolOutput::unobserved("tool executor is unavailable"),
        },
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(preparation_deadline)) => return ToolOutput::refused("process preparation timed out"),
    };
    let owned = match &launch {
        Launch::Shell(args) => super::process_context::OwnedLaunch::Shell((*args).clone()),
        Launch::Exec(args) => super::process_context::OwnedLaunch::Exec((*args).clone()),
    };
    let preparation_cancelled = cancelled.clone();
    let preparation_workspace = workspace.clone();
    use tracing::Instrument as _;
    let mut prepared = match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        super::process_context::prepare(
            preparation_workspace,
            owned,
            approved_read_images,
            process_producer,
            call_id,
            &preparation_cancelled,
            preparation_deadline,
        )
    })
    .instrument(tracing::info_span!(target: "qq::diagnostic", "native_prepare"))
    .await
    {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) if cancelled.is_cancelled() => {
            return ToolOutput::interrupted(error.to_string());
        }
        Ok(Err(error)) => return ToolOutput::refused(error.to_string()),
        Err(_) => return ToolOutput::unobserved("process preparation stopped unexpectedly"),
    };
    if cancelled.is_cancelled() {
        return ToolOutput::interrupted("tool execution was cancelled before spawn");
    }
    if std::time::Instant::now() >= preparation_deadline {
        return ToolOutput::refused("process preparation timed out before spawn");
    }
    let stdin_text = match &launch {
        Launch::Exec(arguments) => arguments.stdin.clone(),
        Launch::Shell(_) => None,
    };
    let mut child_exit = match child_exit::ChildExit::new() {
        Ok(exit) => exit,
        Err(error) => {
            return ToolOutput::refused(format!("could not observe command exits: {error}"));
        }
    };
    let (mut child, mut guard, receipt) = match prepared
        .spawn(
            stdin_text.is_some(),
            process,
            cancelled,
            preparation_deadline,
        )
        .instrument(tracing::info_span!(target: "qq::diagnostic", "native_start"))
        .await
    {
        Ok(started) => started,
        Err(error) if cancelled.is_cancelled() => {
            return ToolOutput::interrupted(error.to_string());
        }
        Err(error) => return ToolOutput::refused(format!("could not start the command: {error}")),
    };
    process.process_started();
    #[cfg(all(test, unix))]
    {
        let hook = {
            let mut hooks = RECEIPT_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.workspace == workspace.path())
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
        }
    }
    if let (Some(id), Some(receipt)) = (call_id, receipt.as_ref()) {
        let mut write = process.record_process(id, receipt.clone());
        let interrupted = tokio::select! {
            biased;
            () = cancelled.cancelled() => Some("tool execution was cancelled"),
            () = wait_for_command_deadline(command_deadline) => Some("command timed out"),
            result = &mut write => {
                if let Err(error) = result {
                    return match stop_shell(&mut child, &mut guard, process).await {
                        Ok(()) => ToolOutput::unobserved(format!("could not persist the command process: {error}")),
                        Err(message) => ToolOutput::unobserved(message),
                    };
                }
                None
            }
        };
        if let Some(reason) = interrupted {
            let stopped = stop_shell(&mut child, &mut guard, process).await;
            let recorded = write.await;
            let mut output = match (stopped, recorded) {
                (Err(message), _) => ToolOutput::unobserved(message),
                (_, Err(error)) => ToolOutput::unobserved(format!(
                    "could not persist the command process: {error}"
                )),
                (Ok(()), Ok(())) => ToolOutput::interrupted(reason),
            };
            output.execution_context = receipt.clone().into();
            return output;
        }
    }
    #[cfg(test)]
    {
        let hook = {
            let mut hooks = SPAWN_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.workspace == workspace.path())
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(child.id().unwrap());
            assert!(!hook.panic, "injected shell task panic after process spawn");
        }
    }
    if let (Some(text), Some(mut stdin)) = (stdin_text, child.stdin.take()) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(text.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    #[cfg(test)]
    let mut read_fault = {
        let mut faults = READ_FAULTS.lock().unwrap();
        faults
            .iter()
            .position(|(path, _)| path == workspace.path())
            .map(|index| faults.remove(index).1)
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let started = tokio::time::Instant::now();
    let capture = std::sync::Arc::new(std::sync::Mutex::new(BoundedCapture::new(
        MAX_CAPTURE_BYTES,
    )));
    let mut identity = ShellResultIdentity::default();
    let mut streamed = LiveOutput::default();
    let mut buffered_output = Vec::with_capacity(SHELL_READ_CHUNK_BYTES);
    let mut output_flush = std::pin::pin!(tokio::time::sleep(OUTPUT_FLUSH_INTERVAL));
    let read_chunk_bytes = SHELL_READ_CHUNK_BYTES;
    let mut stdout_buffer = vec![0_u8; read_chunk_bytes];
    let mut stderr_buffer = vec![0_u8; read_chunk_bytes];
    let mut timeout = std::pin::pin!(wait_for_command_deadline(command_deadline));
    let cancellation = cancelled;
    let mut cancelled = std::pin::pin!(cancelled.cancelled());
    let yield_after = yielding.is_some().then(|| {
        std::time::Duration::from_secs(
            launch
                .yield_after_seconds()
                .unwrap_or(super::commands::DEFAULT_YIELD_AFTER_SECS),
        )
    });
    let mut yield_timer = std::pin::pin!(tokio::time::sleep_until(
        yield_after
            .and_then(|after| started.checked_add(after))
            .unwrap_or(started)
    ));
    let mut yield_armed = yield_after.is_some();
    let mut drain = std::pin::pin!(tokio::time::sleep(EXIT_DRAIN));
    let mut exited = None;
    let push = |capture: &std::sync::Mutex<BoundedCapture>, bytes: &[u8]| {
        capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes);
    };

    let ended = {
        let mut exit = std::pin::pin!(child_exit.wait(&mut child, &mut guard));
        loop {
            if exited.is_some() && stdout.is_none() && stderr.is_none() {
                break None;
            }
            if !buffered_output.is_empty()
                && (buffered_output.len() >= SHELL_READ_CHUNK_BYTES
                    || (stdout.is_none() && stderr.is_none()))
            {
                streamed.forward(output, &mut buffered_output, false);
            }
            tokio::select! {
                biased;
                () = &mut cancelled => break exited.is_none().then_some(ShellOutcome::Cancelled),
                () = &mut timeout => break exited.is_none().then_some(ShellOutcome::TimedOut),
                () = &mut drain, if exited.is_some() => break None,
                () = &mut yield_timer, if yield_armed && exited.is_none() => {
                    yield_armed = false;
                    if let Some(yielding) = yielding.as_deref_mut()
                        && let Some(command) = yielding.tasks.commands().register(
                            call_id,
                            launch.header_tool(),
                            started,
                            std::sync::Arc::clone(&capture),
                            std::sync::Arc::clone(&yielding.call),
                        )
                    {
                        let mut result = yielded_result(launch.header_tool(), &command.id, started.elapsed(), &capture);
                        result.execution_context.clone_from(&receipt);
                        process.yielded();
                        if let Some(id) = call_id {
                            process.save_finished(id, &result);
                        }
                        if let Some(early) = yielding.early.take() {
                            let _ = early.send(result);
                        }
                        yielding.command = Some(command);
                    }
                }
                () = &mut output_flush, if !buffered_output.is_empty() => {
                    streamed.forward(output, &mut buffered_output, false);
                }
                read = read_from(&mut stdout, &mut stdout_buffer), if stdout.is_some() => {
                    #[cfg(test)]
                    let read = if read_fault == Some(false) {
                        read_fault = None;
                        Err(std::io::Error::other("injected stdout read failure"))
                    } else { read };
                    match read {
                        Ok(0) => stdout = None,
                        Err(source) => break Some(ShellOutcome::ReadFailed { stream: "stdout", source }),
                        Ok(read) => {
                            identity.stdout.update(&stdout_buffer[..read]);
                            identity.stdout_bytes += read as u64;
                            push(&capture, &stdout_buffer[..read]);
                            if buffered_output.is_empty() {
                                output_flush.as_mut().reset(tokio::time::Instant::now() + OUTPUT_FLUSH_INTERVAL);
                            }
                            let remaining = MAX_LIVE_OUTPUT_BYTES.saturating_sub(streamed.sent + buffered_output.len());
                            buffered_output.extend_from_slice(&stdout_buffer[..read.min(remaining)]);
                        }
                    }
                }
                read = read_from(&mut stderr, &mut stderr_buffer), if stderr.is_some() => {
                    #[cfg(test)]
                    let read = if read_fault == Some(true) {
                        read_fault = None;
                        Err(std::io::Error::other("injected stderr read failure"))
                    } else { read };
                    match read {
                        Ok(0) => stderr = None,
                        Err(source) => break Some(ShellOutcome::ReadFailed { stream: "stderr", source }),
                        Ok(read) => {
                            identity.stderr.update(&stderr_buffer[..read]);
                            identity.stderr_bytes += read as u64;
                            push(&capture, &stderr_buffer[..read]);
                            if buffered_output.is_empty() {
                                output_flush.as_mut().reset(tokio::time::Instant::now() + OUTPUT_FLUSH_INTERVAL);
                            }
                            let remaining = MAX_LIVE_OUTPUT_BYTES.saturating_sub(streamed.sent + buffered_output.len());
                            buffered_output.extend_from_slice(&stderr_buffer[..read.min(remaining)]);
                        }
                    }
                }
                status = &mut exit, if exited.is_none() => {
                    exited = Some(status);
                    drain.as_mut().reset(tokio::time::Instant::now() + EXIT_DRAIN);
                }
            }
        }
    };
    drop((stdout, stderr));
    let outcome = match (ended, exited) {
        (Some(outcome), _) => outcome,
        (None, Some(status)) => {
            ShellOutcome::Exited(confirm_child_exit(status, &mut guard, process).await)
        }
        (None, None) => unreachable!("the loop ends without an outcome only after the exit"),
    };
    let capture = std::mem::replace(
        &mut *capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        BoundedCapture::new(0),
    );

    let run_expired = cancellation.timed_out();
    let outcome = match outcome {
        ShellOutcome::Cancelled if run_expired => ShellOutcome::TimedOut,
        outcome => outcome,
    };
    let elapsed = started.elapsed();
    let result = async {
        match outcome {
            ShellOutcome::Exited(Ok(status)) => shell_result(
                launch.header_tool(),
                capture,
                identity,
                status,
                elapsed,
                hint.as_deref(),
            ),
            ShellOutcome::Exited(Err(error)) => {
                ToolOutput::unobserved(format!("could not observe the command exit: {error}"))
            }
            ShellOutcome::ReadFailed { stream, source } => {
                if let Err(message) = stop_shell(&mut child, &mut guard, process).await {
                    return ToolOutput::unobserved(format!(
                        "could not read command {stream}: {source}; {message}"
                    ));
                }
                let mut result = ToolOutput::bounded(
                    stopped_result(
                        launch.header_tool(),
                        "unobserved",
                        elapsed,
                        capture,
                        &format!(
                            "could not read command {stream}: {source}; its process group was killed"
                        ),
                    ),
                    &SHELL_BOUNDS,
                    true,
                );
                result.disposition = ResultDisposition::Unobserved;
                result
            }
            ShellOutcome::TimedOut => {
                if let Err(message) = stop_shell(&mut child, &mut guard, process).await {
                    return ToolOutput::unobserved(message);
                }
                let reason = match timeout_seconds.filter(|_| !run_expired) {
                    Some(seconds) => format!(
                        "command timed out after its timeout_seconds={seconds} limit; its process group was killed. Retry with a larger timeout_seconds, or let it run in the background: it returns a command id after yield_after_seconds, and command_status waits on it"
                    ),
                    None => "the task deadline expired while the command ran; its process group was killed".to_owned(),
                };
                let mut result = ToolOutput::bounded(
                    stopped_result(launch.header_tool(), "timeout", elapsed, capture, &reason),
                    &SHELL_BOUNDS,
                    true,
                );
                result.result_digest = identity.finish(None, None, true);
                result.native_outcome = Some(super::dispatch::CommandOutcome::TimedOut);
                result
            }
            ShellOutcome::Cancelled => {
                if let Err(message) = stop_shell(&mut child, &mut guard, process).await {
                    return ToolOutput::unobserved(message);
                }
                let mut result = ToolOutput::bounded(
                    stopped_result(
                        launch.header_tool(),
                        "cancelled",
                        elapsed,
                        capture,
                        "tool execution was cancelled; its process group was killed",
                    ),
                    &SHELL_BOUNDS,
                    true,
                );
                result.disposition = ResultDisposition::Interrupted;
                result.result_digest = identity.finish(None, None, false);
                result.native_outcome = Some(super::dispatch::CommandOutcome::Cancelled);
                result
            }
        }
    }
    .await;
    streamed.forward(output, &mut buffered_output, true);
    let mut result = result;
    result.execution_context = receipt;
    drop(prepared);
    match yielding.and_then(|yielding| yielding.command.as_ref()) {
        Some(command) => {
            command.finish(result);
            ToolOutput::unobserved("the command result went to command_status")
        }
        None => result,
    }
}

fn yielded_result(
    tool: &str,
    id: &str,
    elapsed: std::time::Duration,
    capture: &std::sync::Mutex<BoundedCapture>,
) -> ToolOutput {
    let capture = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut content = Header::new(tool, None)
        .field("exit", "running")
        .field("id", id)
        .field("elapsed", format_args!("{:.1}", elapsed.as_secs_f64()))
        .field("bytes", capture.total())
        .into_line();
    content.push_str(&capture.into_output());
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "still running as command {id}; call command_status with id={id} to wait for it, read its output, or cancel it"
    ));
    ToolOutput::bounded(content, &SHELL_BOUNDS, false)
}

pub(super) async fn confirm_child_exit(
    status: std::io::Result<std::process::ExitStatus>,
    guard: &mut ProcessGroupGuard,
    process: &mut super::ToolTaskLease,
) -> std::io::Result<std::process::ExitStatus> {
    if status.is_ok() {
        guard.confirm_stopped().await?;
        process.process_reaped();
    }
    status
}

async fn stop_shell(
    child: &mut NativeChild,
    guard: &mut ProcessGroupGuard,
    process: &mut super::ToolTaskLease,
) -> Result<(), String> {
    #[cfg(test)]
    let observed_pid = child.id();
    if let Err(error) = guard.kill().await {
        return Err(format!(
            "could not terminate the command process group: {error}"
        ));
    }
    if let Err(error) = child.start_kill() {
        return Err(format!("could not terminate the command: {error}"));
    }
    let status = child.wait().await;
    match confirm_child_exit(status, guard, process).await {
        Ok(_) => {
            #[cfg(test)]
            if let Some(pid) = observed_pid {
                let sender = {
                    let mut observers = STOP_OBSERVERS.lock().unwrap();
                    observers
                        .iter()
                        .position(|(expected, _)| *expected == pid)
                        .map(|index| observers.remove(index).1)
                };
                if let Some(sender) = sender {
                    let _ = sender.send(());
                }
            }
            Ok(())
        }
        Err(error) => Err(format!("could not confirm the command exit: {error}")),
    }
}

#[cfg(unix)]
pub(super) fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("/bin/sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(windows)]
pub(super) fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("cmd");
    shell.raw_arg(cmd_arguments(command));
    shell
}

#[cfg(any(windows, test))]
fn cmd_arguments(command: &str) -> String {
    format!("/D /S /C \"{command}\"")
}

#[cfg(not(any(unix, windows)))]
pub(super) fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

async fn wait_for_command_deadline(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

async fn read_from<R>(reader: &mut Option<R>, buffer: &mut [u8]) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match reader.as_mut() {
        Some(reader) => reader.read(buffer).await,
        None => std::future::pending().await,
    }
}

#[derive(Default)]
struct LiveOutput {
    sent: usize,
    carry: Vec<u8>,
}

impl LiveOutput {
    fn forward(
        &mut self,
        output: Option<&mpsc::Sender<String>>,
        buffered: &mut Vec<u8>,
        last: bool,
    ) {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.append(buffered);
        if !last {
            self.carry = bytes.split_off(incomplete_utf8_start(&bytes));
        }
        forward_shell_chunk(output, &mut self.sent, &bytes);
    }
}

pub(super) fn forward_shell_chunk(
    output: Option<&mpsc::Sender<String>>,
    streamed: &mut usize,
    bytes: &[u8],
) {
    let Some(sender) = output else {
        return;
    };
    if *streamed >= MAX_LIVE_OUTPUT_BYTES || bytes.is_empty() {
        return;
    }
    let remaining = MAX_LIVE_OUTPUT_BYTES - *streamed;
    let bytes = &bytes[..bytes.len().min(remaining)];
    let chunk = String::from_utf8_lossy(bytes).into_owned();
    match sender.try_send(chunk) {
        Ok(()) => *streamed += bytes.len(),
        Err(mpsc::error::TrySendError::Full(_)) => *streamed += bytes.len(),
        Err(mpsc::error::TrySendError::Closed(_)) => *streamed = MAX_LIVE_OUTPUT_BYTES,
    }
}

fn incomplete_utf8_start(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let width = match byte {
            0b1100_0000..=0b1101_1111 => 2,
            0b1110_0000..=0b1110_1111 => 3,
            0b1111_0000..=0b1111_0111 => 4,
            _ => 1,
        };
        return if width > back {
            bytes.len() - back
        } else {
            bytes.len()
        };
    }
    bytes.len()
}

fn stopped_result(
    tool: &str,
    exit: &str,
    elapsed: std::time::Duration,
    capture: BoundedCapture,
    reason: &str,
) -> String {
    let mut content = Header::new(tool, None)
        .field("exit", exit)
        .field("elapsed", format_args!("{:.1}", elapsed.as_secs_f64()))
        .field("bytes", capture.total())
        .into_line();
    content.push_str(&capture.into_output());
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(reason);
    content
}

fn shell_result(
    tool: &str,
    capture: BoundedCapture,
    identity: ShellResultIdentity,
    status: std::process::ExitStatus,
    elapsed: std::time::Duration,
    hint: Option<&str>,
) -> ToolOutput {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut header = Header::new(tool, None);
    header = match (status.code(), signal) {
        (Some(code), _) => header.field("exit", code),
        (None, Some(signal)) => header.field("exit", format_args!("signal:{signal}")),
        (None, None) => header.field("exit", "unknown"),
    };
    let mut content = header
        .field("elapsed", format_args!("{:.1}", elapsed.as_secs_f64()))
        .field("bytes", capture.total())
        .into_line();
    content.push_str(&capture.into_output());
    if let Some(hint) = hint {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(hint);
        content.push('\n');
    }
    let mut result = match status.code() {
        Some(0) => ToolOutput::bounded(content, &SHELL_BOUNDS, false),
        Some(_) => ToolOutput::bounded(content, &SHELL_BOUNDS, true),
        None => {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&match signal {
                Some(signal) => format!("command was terminated by signal {signal}"),
                None => "command was terminated without an exit code".to_owned(),
            });
            ToolOutput::bounded(content, &SHELL_BOUNDS, true)
        }
    };
    result.result_digest = identity.finish(status.code(), signal, false);
    result.native_outcome = Some(super::dispatch::CommandOutcome::Exited {
        code: status.code(),
    });
    result
}

pub(crate) struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
    #[cfg(unix)]
    killed_pgid: Option<rustix::process::Pid>,
    #[cfg(unix)]
    leader: Option<rustix::process::Pid>,
    #[cfg(unix)]
    descendants: Vec<descendants::Tracked>,
    #[cfg(unix)]
    watcher: Option<tokio::process::Child>,
}

#[cfg(unix)]
pub(crate) async fn spawn_watcher(
    deadline: std::time::Instant,
) -> std::io::Result<tokio::process::Child> {
    static PIPE: std::sync::OnceLock<
        Result<(std::io::PipeReader, std::io::PipeWriter), std::io::ErrorKind>,
    > = std::sync::OnceLock::new();
    let (reader, _) = PIPE
        .get_or_init(|| std::io::pipe().map_err(|error| error.kind()))
        .as_ref()
        .map_err(|kind| std::io::Error::from(*kind))?;
    let mut watcher = tokio::process::Command::new("/bin/sh");
    watcher
        .args([
            "-c",
            "trap '' HUP INT QUIT TERM; echo; exec >/dev/null; while read -r _; do :; done; kill -s KILL 0",
        ])
        .env_clear()
        .current_dir("/")
        .stdin(std::process::Stdio::from(reader.try_clone()?))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut watcher = watcher.spawn()?;
    let mut ready = watcher
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("command watcher has no stdout"))?;
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout_at(deadline.into(), ready.read(&mut byte)).await;
    if !matches!(read, Ok(Ok(1))) {
        let _ = watcher.start_kill();
        let _ = watcher.wait().await;
        return Err(match read {
            Ok(Err(error)) => error,
            Ok(Ok(_)) => std::io::Error::other("command watcher exited before it was ready"),
            Err(_) => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "command watcher did not start",
            ),
        });
    }
    Ok(watcher)
}

impl ProcessGroupGuard {
    pub(crate) fn new(child_id: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self {
                pgid: child_id
                    .and_then(|id| i32::try_from(id).ok())
                    .and_then(rustix::process::Pid::from_raw),
                killed_pgid: None,
                leader: None,
                descendants: Vec::new(),
                watcher: None,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child_id;
            Self {}
        }
    }

    #[cfg(unix)]
    pub(crate) fn watch(&mut self, watcher: tokio::process::Child, leader: Option<u32>) {
        self.watcher = Some(watcher);
        self.leader = leader
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
    }

    #[cfg(unix)]
    pub(crate) fn group(&self) -> Option<rustix::process::Pid> {
        self.pgid
    }

    #[cfg(unix)]
    pub(crate) async fn reap_watcher(&mut self) -> std::io::Result<()> {
        if let Some(mut watcher) = self.watcher.take() {
            let _ = watcher.start_kill();
            tokio::time::timeout(std::time::Duration::from_secs(2), watcher.wait())
                .await
                .map_err(|_| std::io::Error::other("command watcher did not exit"))??;
        }
        Ok(())
    }

    pub(crate) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
            self.killed_pgid = None;
        }
    }

    pub(crate) async fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            let leader = self.leader;
            let collected =
                tokio::task::spawn_blocking(move || descendants::stop_and_kill(pgid, leader))
                    .await
                    .map_err(std::io::Error::other)
                    .and_then(|collected| collected);
            let killed = self.kill_group();
            self.descendants = collected?;
            #[cfg(target_os = "macos")]
            if killed.as_ref().is_err_and(|error| {
                error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error())
            }) {
                self.killed_pgid = self.pgid.take();
                return Ok(());
            }
            return killed;
        }
        self.kill_group()
    }

    fn kill_group(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            match rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
                Ok(()) => {
                    self.killed_pgid = Some(pgid);
                    self.pgid = None;
                }
                Err(rustix::io::Errno::SRCH) => self.pgid = None,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    async fn confirm_stopped(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if self.killed_pgid.is_some() {
            self.reap_watcher().await?;
        }
        #[cfg(target_os = "linux")]
        if let Some(pgid) = self.killed_pgid {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let tracked = std::mem::take(&mut self.descendants);
            return tokio::task::spawn_blocking(move || {
                descendants::confirm(tracked, deadline)?;
                super::process_group::confirm_linux_group_exit(pgid, deadline)
            })
            .await
            .map_err(std::io::Error::other)?;
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        if let Some(pgid) = self.killed_pgid {
            let tracked = std::mem::take(&mut self.descendants);
            let result = super::native_exit::confirm(move || {
                if !descendants::gone(&tracked)? {
                    return Ok(false);
                }
                match rustix::process::test_kill_process_group(pgid) {
                    Err(rustix::io::Errno::SRCH) => Ok(true),
                    Err(rustix::io::Errno::PERM) if cfg!(target_os = "macos") => Ok(false),
                    Err(error) => Err(error.into()),
                    Ok(()) => Ok(false),
                }
            })
            .await;
            return result;
        }
        Ok(())
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let _ = self.kill_group();
    }
}

#[derive(Default)]
struct ShellResultIdentity {
    stdout: Sha256,
    stderr: Sha256,
    stdout_bytes: u64,
    stderr_bytes: u64,
}

impl ShellResultIdentity {
    fn finish(self, code: Option<i32>, signal: Option<i32>, timed_out: bool) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"qq-shell-result-v1\0");
        digest.update([u8::from(timed_out)]);
        for value in [code, signal] {
            digest.update([u8::from(value.is_some())]);
            digest.update(value.unwrap_or_default().to_le_bytes());
        }
        digest.update(self.stdout_bytes.to_le_bytes());
        digest.update(self.stdout.finalize());
        digest.update(self.stderr_bytes.to_le_bytes());
        digest.update(self.stderr.finalize());
        digest.finalize().into()
    }
}

#[derive(Clone)]
pub(super) struct BoundedCapture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
    half: usize,
}

impl BoundedCapture {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
            half: budget / 2,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let head_room = self.half.saturating_sub(self.head.len());
        let take = head_room.min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        let rest = &bytes[take..];
        if rest.is_empty() {
            return;
        }
        self.tail.extend(rest.iter().copied());
        if self.tail.len() > self.half {
            let excess = self.tail.len() - self.half;
            self.tail.drain(..excess);
        }
    }

    pub(super) fn total(&self) -> u64 {
        self.total
    }

    pub(super) fn into_output(self) -> String {
        let omitted = self.total - self.head.len() as u64 - self.tail.len() as u64;
        let tail = self.tail.into_iter().collect::<Vec<_>>();
        if omitted == 0 {
            let mut bytes = self.head;
            bytes.extend_from_slice(&tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        format!(
            "{}\n…[qq: {omitted} bytes not captured]…\n{}",
            String::from_utf8_lossy(&self.head),
            String::from_utf8_lossy(&tail),
        )
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use sha2::Digest;

    fn identity(stdout: &[u8], stderr: &[u8]) -> ShellResultIdentity {
        let mut identity = ShellResultIdentity::default();
        identity.stdout.update(stdout);
        identity.stdout_bytes = stdout.len() as u64;
        identity.stderr.update(stderr);
        identity.stderr_bytes = stderr.len() as u64;
        identity
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_read_failure_is_unobserved_and_confirms_process_cleanup() {
        for stderr in [false, true] {
            for live in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let workspace = Workspace::open(directory.path()).unwrap();
                READ_FAULTS
                    .lock()
                    .unwrap()
                    .push((workspace.path().to_owned(), stderr));
                let spawned = observe_shell_spawn(workspace.path(), false);
                let command = if live {
                    "printf stdout; printf stderr >&2; sleep 30"
                } else {
                    "printf stdout; printf stderr >&2"
                };
                let arguments =
                    serde_json::from_value(serde_json::json!({"command":command})).unwrap();
                let cancelled = ToolCancellation::new(crate::RunCancellation::new());
                let tasks = super::super::ToolTasks::default();
                let mut process = tasks.enter();
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    run_shell(
                        &workspace,
                        &ShellPolicy::default(),
                        std::sync::Arc::default(),
                        std::sync::Arc::default(),
                        None,
                        Launch::Shell(&arguments),
                        &cancelled,
                        None,
                        &mut process,
                        None,
                    ),
                )
                .await
                .expect("pipe failure must stop and reap the process promptly");
                assert_eq!(
                    result.disposition,
                    super::super::ResultDisposition::Unobserved,
                    "stderr={stderr} live={live}: a failed pipe read must not become EOF"
                );
                assert!(result.is_error);
                assert!(
                    result.model_text.contains("exit=unobserved"),
                    "{}",
                    result.model_text
                );
                assert!(
                    result.model_text.contains(if stderr {
                        "could not read command stderr"
                    } else {
                        "could not read command stdout"
                    }),
                    "{}",
                    result.model_text
                );
                drop(process);
                tasks
                    .drain()
                    .await
                    .expect("the owned child must be reaped before settlement");
                let pid = spawned.await.unwrap();
                let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
                assert!(rustix::process::test_kill_process(pid).is_err());
            }
        }
    }

    #[test]
    fn shell_identity_preserves_hidden_bytes_and_stream_boundaries() {
        let a = vec![b'x'; super::super::output::MAX_SPILL_ITEM_BYTES + 1024];
        let mut b = a.clone();
        b[a.len() / 2] = b'y';
        let mut first_capture = BoundedCapture::new(MAX_CAPTURE_BYTES);
        let mut second_capture = BoundedCapture::new(MAX_CAPTURE_BYTES);
        first_capture.push(&a);
        second_capture.push(&b);
        assert_eq!(first_capture.into_output(), second_capture.into_output());
        assert_ne!(
            identity(&a, b"").finish(Some(0), None, false),
            identity(&b, b"").finish(Some(0), None, false)
        );
        assert_ne!(
            identity(b"ab", b"c").finish(Some(0), None, false),
            identity(b"a", b"bc").finish(Some(0), None, false)
        );
        assert_ne!(
            identity(b"out", b"err").finish(Some(0), None, false),
            identity(b"err", b"out").finish(Some(0), None, false)
        );
    }

    #[test]
    fn shell_identity_ignores_chunks_and_distinguishes_terminal_status() {
        let mut split = ShellResultIdentity::default();
        split.stderr.update(b"er");
        split.stdout.update(b"o");
        split.stderr.update(b"r");
        split.stdout.update(b"ut");
        split.stdout_bytes = 3;
        split.stderr_bytes = 3;
        let expected = identity(b"out", b"err").finish(Some(0), None, false);
        assert_eq!(split.finish(Some(0), None, false), expected);
        for (code, signal, timeout) in [
            (Some(1), None, false),
            (None, Some(9), false),
            (None, None, true),
            (None, None, false),
        ] {
            assert_ne!(
                identity(b"out", b"err").finish(code, signal, timeout),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_identity_excludes_elapsed_and_hint_but_preserves_stored_hash() {
        use std::os::unix::process::ExitStatusExt;
        let output = |elapsed, hint| {
            let mut capture = BoundedCapture::new(MAX_CAPTURE_BYTES);
            let bytes = b"line\n".repeat(10_000);
            capture.push(&bytes);
            shell_result(
                "shell",
                capture,
                identity(&bytes, b""),
                std::process::ExitStatus::from_raw(0),
                elapsed,
                hint,
            )
        };
        let first = output(std::time::Duration::from_millis(10), None);
        let second = output(
            std::time::Duration::from_millis(210),
            Some("use a built-in tool"),
        );
        assert_ne!(first.model_text, second.model_text);
        assert_eq!(first.result_digest, second.result_digest);
        for result in [first, second] {
            let spill = result.spill.unwrap();
            assert_eq!(
                spill.digest,
                format!("{:x}", sha2::Sha256::digest(spill.text.as_bytes()))
            );
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod watcher_tests {
    #[tokio::test]
    async fn the_watcher_ignores_group_signals_before_a_command_joins() {
        let mut watcher =
            super::spawn_watcher(std::time::Instant::now() + std::time::Duration::from_secs(10))
                .await
                .unwrap();
        let status = tokio::fs::read_to_string(format!("/proc/{}/status", watcher.id().unwrap()))
            .await
            .unwrap();
        watcher.start_kill().unwrap();
        watcher.wait().await.unwrap();
        let ignored = status
            .lines()
            .find_map(|line| line.strip_prefix("SigIgn:"))
            .map(|mask| u64::from_str_radix(mask.trim(), 16).unwrap())
            .unwrap();
        for signal in [
            rustix::process::Signal::HUP,
            rustix::process::Signal::INT,
            rustix::process::Signal::QUIT,
            rustix::process::Signal::TERM,
        ] {
            let bit = 1_u64 << (signal.as_raw() - 1);
            assert_ne!(ignored & bit, 0, "watcher does not yet ignore {signal:?}");
        }
    }
}

#[cfg(all(test, unix))]
mod orphan_tests {
    #[test]
    fn a_killed_server_takes_its_commands_with_it() {
        if let Some(report) = std::env::var_os("QQ_COMMAND_ORPHAN_WORKER") {
            let directory = tempfile::tempdir().unwrap();
            let workspace = crate::workspace::Workspace::open(directory.path()).unwrap();
            let command = format!(
                "exec >/dev/null 2>&1; trap '' TERM; kill -s TERM 0; tail -f /dev/null & echo $$ $! > '{}'; exec tail -f /dev/null",
                std::path::Path::new(&report).display()
            );
            let arguments = super::ShellArgs {
                command,
                cwd: None,
                timeout_seconds: None,
                yield_after_seconds: None,
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let tasks = crate::tools::ToolTasks::default();
                    let mut lease = tasks.enter();
                    super::run_shell(
                        &workspace,
                        &crate::runtime::ShellPolicy::default(),
                        std::sync::Arc::default(),
                        std::sync::Arc::default(),
                        None,
                        super::Launch::Shell(&arguments),
                        &super::ToolCancellation::new(crate::RunCancellation::new()),
                        None,
                        &mut lease,
                        None,
                    )
                    .await
                });
            unreachable!("the command never ends; the parent kills this process");
        }
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("pids");
        let mut server = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::shell::orphan_tests::a_killed_server_takes_its_commands_with_it",
                "--nocapture",
            ])
            .env("QQ_COMMAND_ORPHAN_WORKER", &report)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let pids = loop {
            if let Ok(text) = std::fs::read_to_string(&report)
                && text.ends_with('\n')
            {
                break text;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "command never started"
            );
            std::thread::park_timeout(std::time::Duration::from_millis(10));
        };
        let mut pids = pids
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().unwrap());
        let leader = rustix::process::Pid::from_raw(pids.next().unwrap()).unwrap();
        let background = rustix::process::Pid::from_raw(pids.next().unwrap()).unwrap();
        server.kill().unwrap();
        server.wait().unwrap();
        let stopped = std::time::Instant::now();
        for pid in [leader, background] {
            while rustix::process::test_kill_process(pid).is_ok() {
                assert!(
                    stopped.elapsed() < std::time::Duration::from_secs(10),
                    "the command outlived the process that started it"
                );
                std::thread::park_timeout(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod cmd_text_tests {
    #[test]
    fn cmd_gets_the_shell_text_unescaped_without_autorun() {
        assert_eq!(
            super::cmd_arguments(r#"echo "a b" > "out file.txt""#),
            r#"/D /S /C "echo "a b" > "out file.txt"""#
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn quoted_redirection_reaches_cmd_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let status = super::shell_command(r#"echo quoted> "out file.txt""#)
            .current_dir(directory.path())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("out file.txt")).unwrap(),
            "quoted\r\n"
        );
    }
}

#[cfg(test)]
mod live_output_tests {
    #[test]
    fn a_character_split_across_reads_arrives_whole() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let mut live = super::LiveOutput::default();
        let mut buffered = "a é".as_bytes()[..3].to_vec();
        live.forward(Some(&sender), &mut buffered, false);
        assert_eq!(receiver.try_recv().unwrap(), "a ");
        assert!(buffered.is_empty());
        buffered.extend_from_slice(&[0xa9, b'!']);
        live.forward(Some(&sender), &mut buffered, false);
        assert_eq!(receiver.try_recv().unwrap(), "é!");
        buffered.push(0xe2);
        live.forward(Some(&sender), &mut buffered, false);
        assert!(receiver.try_recv().is_err());
        live.forward(Some(&sender), &mut buffered, true);
        assert_eq!(receiver.try_recv().unwrap(), "\u{fffd}");
    }
}

#[cfg(all(test, unix))]
mod split_character_tests {
    #[tokio::test]
    async fn a_paused_partial_character_does_not_stall_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::workspace::Workspace::open(directory.path()).unwrap();
        let arguments = super::ShellArgs {
            command: r"printf '\303'; sleep 0.2; printf '\251done'".to_owned(),
            cwd: None,
            timeout_seconds: Some(30),
            yield_after_seconds: None,
        };
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let tasks = crate::tools::ToolTasks::default();
        let mut lease = tasks.enter();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::run_shell(
                &workspace,
                &crate::runtime::ShellPolicy::default(),
                std::sync::Arc::default(),
                std::sync::Arc::default(),
                None,
                super::Launch::Shell(&arguments),
                &super::ToolCancellation::new(crate::RunCancellation::new()),
                Some(&sender),
                &mut lease,
                None,
            ),
        )
        .await
        .expect("a held partial character must not stall the command");
        drop(lease);
        assert!(
            result.model_text.starts_with("shell exit=0 "),
            "{}",
            result.model_text
        );
        assert!(result.model_text.contains("édone"), "{}", result.model_text);
        let mut live = String::new();
        while let Ok(chunk) = receiver.try_recv() {
            live.push_str(&chunk);
        }
        assert_eq!(live, "édone");
        tasks.drain().await.unwrap();
    }
}
