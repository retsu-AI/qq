use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, sync::mpsc};

use crate::{
    runtime::{BASE_ENV, BuiltinPreference, MAX_SHELL_ENV_NAMES, ShellPolicy, builtin_alternative},
    workspace::Workspace,
};

use super::{
    dispatch::{ResultDisposition, ToolCancellation, ToolOutput},
    output::{Bounds, Header},
};

/// Combined output captured (head+tail) and streamed per command. The full
/// capture is what a spill store keeps; the model sees [`SHELL_BOUNDS`].
pub(super) const MAX_SHELL_OUTPUT_BYTES: usize = 128 * 1024;
/// Model-facing bound: the head of a command's output and its tail, where the
/// verdict lives, are what the model reads; the middle of a long build log
/// is noise it should page into deliberately.
pub(super) const SHELL_BOUNDS: Bounds = Bounds::new(16 * 1024, 4_000);
const SHELL_READ_CHUNK_BYTES: usize = 8 * 1024;
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;

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

#[cfg(test)]
static READ_FAULTS: std::sync::Mutex<Vec<(std::path::PathBuf, bool)>> =
    std::sync::Mutex::new(Vec::new());

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellArgs {
    pub(super) command: String,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) timeout_seconds: Option<u64>,
    /// Environment variable names to pass through from the server's
    /// environment. The child otherwise starts from [`BASE_ENV`] only.
    #[serde(default)]
    pub(super) env: Vec<String>,
}

/// `exec`: one program with an argument list and no shell between the model
/// and the process — no quoting, globbing, `$`, or pipes, so the classifier
/// sees exact argv. Same bounds, capture, spill, and approval path as `shell`.
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
    #[serde(default)]
    pub(super) env: Vec<String>,
}

pub(super) const MAX_EXEC_ARGS: usize = 64;
pub(super) const MAX_EXEC_ARG_BYTES: usize = 4096;
pub(super) const MAX_EXEC_PROGRAM_BYTES: usize = 256;
pub(super) const MAX_EXEC_STDIN_BYTES: usize = 64 * 1024;

/// What to start: a shell string or an exact argv. Everything after spawn
/// is shared.
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
        let timeout = self.timeout_seconds().unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS);
        if timeout == 0
            || std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(timeout))
                .is_none()
        {
            return Err(InvalidLaunch::Timeout);
        }
        if self.env().len() > MAX_SHELL_ENV_NAMES
            || self
                .env()
                .iter()
                .any(|name| !crate::runtime::valid_env_name(name))
        {
            return Err(InvalidLaunch::Env);
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

    fn timeout_seconds(&self) -> Option<u64> {
        match self {
            Self::Shell(args) => args.timeout_seconds,
            Self::Exec(args) => args.timeout_seconds,
        }
    }

    fn env(&self) -> &[String] {
        match self {
            Self::Shell(args) => &args.env,
            Self::Exec(args) => &args.env,
        }
    }

    /// The text the built-in preference judges: the shell string, or the
    /// argv joined so `exec grep …` gets the same hint as `shell grep …`.
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
    #[error("invalid_env: at most 16 valid environment names per call")]
    Env,
    #[error("cwd must be a nonempty relative workspace path without NUL")]
    Cwd,
}

/// The fate of one supervised shell command.
enum ShellOutcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    ReadFailed {
        stream: &'static str,
        source: std::io::Error,
    },
    TimedOut,
    Cancelled,
}

/// Executes one bounded shell command on the async runtime: `sh -c` in its own
/// process group with the workspace (or a contained subdirectory) as its
/// working directory, combined stdout+stderr captured head+tail within the
/// output budget, live chunks forwarded to `output`, and the whole process
/// group killed on timeout or cancellation. The dispatch task owns this
/// future through kill and reap even when its result waiter disappears.
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
    output: Option<&mpsc::Sender<super::ToolUpdate>>,
    process_pending: &mut bool,
) -> ToolOutput {
    if cancelled.is_cancelled() {
        return ToolOutput::interrupted("tool execution was cancelled");
    }
    if let Err(error) = launch.validate() {
        return ToolOutput::refused(error.to_string());
    }
    let env_names = launch.env();
    for name in env_names {
        if !policy.permits_env(name) {
            return ToolOutput::refused(format!(
                "env_not_allowed: {name} is not in policy.shell_env; the child sees only {} plus allowlisted names",
                BASE_ENV.join(" ")
            ));
        }
    }
    // The prefer-built-in nudge: strict mode is refused before the gate
    // (see approval); here it can only be a hint appended to the result.
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
    let timeout_seconds = launch
        .timeout_seconds()
        .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS);
    let Some(preparation_deadline) =
        std::time::Instant::now().checked_add(std::time::Duration::from_secs(timeout_seconds))
    else {
        return ToolOutput::refused(InvalidLaunch::Timeout.to_string());
    };
    let permit = tokio::select! {
        biased;
        () = cancelled.cancelled() => return ToolOutput::interrupted("tool execution was cancelled"),
        result = crate::workspace::blocking_permits().acquire_owned() => match result {
            Ok(permit) => permit,
            Err(_) => return ToolOutput::unobserved("tool executor is unavailable"),
        },
        () = tokio::time::sleep(std::time::Duration::from_secs(timeout_seconds)) => return ToolOutput::refused("process preparation timed out"),
    };
    let owned = match &launch {
        Launch::Shell(args) => super::process_context::OwnedLaunch::Shell((*args).clone()),
        Launch::Exec(args) => super::process_context::OwnedLaunch::Exec((*args).clone()),
    };
    let preparation_cancelled = cancelled.clone();
    let preparation_workspace = workspace.clone();
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
    let (mut child, receipt) = match prepared.spawn(stdin_text.is_some()) {
        Ok(started) => started,
        Err(error) => return ToolOutput::refused(format!("could not start the command: {error}")),
    };
    *process_pending = true;
    // The child leads its own process group (pgid == pid); the guard kills the
    // entire group on every abnormal exit path — timeout, cancellation, and
    // this future being dropped mid-flight — so no descendant is orphaned.
    let mut guard = ProcessGroupGuard::new(child.id());
    if let (Some(output), Some(receipt), Some(_)) = (output, receipt.as_ref(), call_id)
        && output
            .try_send(super::ToolUpdate::ProcessStarted(receipt.clone()))
            .is_err()
    {
        return match stop_shell(&mut child, &mut guard, process_pending).await {
            Ok(()) => ToolOutput::interrupted(
                "the command's process observer did not accept its start receipt",
            ),
            Err(message) => ToolOutput::unobserved(message),
        };
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
    // Stdin is written by its own task so a program that never reads it
    // cannot stall the capture loop; the write end closes when done.
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
    let deadline = tokio::time::Instant::from_std(preparation_deadline);
    let mut capture = BoundedCapture::new(MAX_SHELL_OUTPUT_BYTES);
    let mut identity = ShellResultIdentity::default();
    let mut streamed = 0_usize;
    let mut stdout_buffer = vec![0_u8; SHELL_READ_CHUNK_BYTES];
    let mut stderr_buffer = vec![0_u8; SHELL_READ_CHUNK_BYTES];
    // One timer and one cancellation wait for the whole call: this loop wakes
    // once per output chunk, and re-creating either per iteration would
    // re-register on each of them.
    let mut timeout = std::pin::pin!(tokio::time::sleep_until(deadline));
    let mut cancelled = std::pin::pin!(cancelled.cancelled());

    let outcome = loop {
        tokio::select! {
            biased;
            () = &mut cancelled => break ShellOutcome::Cancelled,
            () = &mut timeout => break ShellOutcome::TimedOut,
            read = read_from(&mut stdout, &mut stdout_buffer), if stdout.is_some() => {
                #[cfg(test)]
                let read = if read_fault == Some(false) {
                    read_fault = None;
                    Err(std::io::Error::other("injected stdout read failure"))
                } else { read };
                match read {
                    Ok(0) => stdout = None,
                    Err(source) => break ShellOutcome::ReadFailed { stream: "stdout", source },
                    Ok(read) => {
                        identity.stdout.update(&stdout_buffer[..read]);
                        identity.stdout_bytes += read as u64;
                        forward_shell_chunk(output, &mut streamed, &stdout_buffer[..read]);
                        capture.push(&stdout_buffer[..read]);
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
                    Err(source) => break ShellOutcome::ReadFailed { stream: "stderr", source },
                    Ok(read) => {
                        identity.stderr.update(&stderr_buffer[..read]);
                        identity.stderr_bytes += read as u64;
                        forward_shell_chunk(output, &mut streamed, &stderr_buffer[..read]);
                        capture.push(&stderr_buffer[..read]);
                    }
                }
            }
            // The command is done only when both pipes reached end-of-file and
            // the child was reaped; a grandchild that inherited a pipe keeps
            // the call running (until the timeout) so its output is captured.
            status = child.wait(), if stdout.is_none() && stderr.is_none() => {
                // The child is reaped, so its pid (the group id) may be
                // recycled: killing the group now could hit an innocent
                // process. Surviving descendants closed their pipes, which is
                // as detached as the timeout policy requires.
                if status.is_ok() {
                    guard.disarm();
                    *process_pending = false;
                }
                break ShellOutcome::Exited(status);
            }
        }
    };

    let elapsed = started.elapsed();
    let mut result = async {
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
                if let Err(message) = stop_shell(&mut child, &mut guard, process_pending).await {
                    return ToolOutput::unobserved(format!(
                        "could not read command {stream}: {source}; {message}"
                    ));
                }
                let mut content = Header::new(launch.header_tool(), None)
                    .field("exit", "unobserved")
                    .field("elapsed", format_args!("{:.1}", elapsed.as_secs_f64()))
                    .field("bytes", capture.total())
                    .into_line();
                content.push_str(&capture.into_output());
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(&format!(
                    "could not read command {stream}: {source}; its process group was killed"
                ));
                let mut result = ToolOutput::bounded(content, &SHELL_BOUNDS, true);
                result.disposition = ResultDisposition::Unobserved;
                result
            }
            ShellOutcome::TimedOut => {
                if let Err(message) = stop_shell(&mut child, &mut guard, process_pending).await {
                    return ToolOutput::unobserved(message);
                }
                let mut content = Header::new(launch.header_tool(), None)
                    .field("exit", "timeout")
                    .field("elapsed", format_args!("{:.1}", elapsed.as_secs_f64()))
                    .field("bytes", capture.total())
                    .into_line();
                content.push_str(&capture.into_output());
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(&format!(
                    "command timed out after {timeout_seconds} s; its process group was killed"
                ));
                let mut result = ToolOutput::bounded(content, &SHELL_BOUNDS, true);
                result.result_digest = identity.finish(None, None, true);
                result
            }
            ShellOutcome::Cancelled => {
                if let Err(message) = stop_shell(&mut child, &mut guard, process_pending).await {
                    return ToolOutput::unobserved(message);
                }
                ToolOutput::interrupted("tool execution was cancelled")
            }
        }
    }
    .await;
    result.execution_context = receipt;
    drop(prepared);
    result
}

async fn stop_shell(
    child: &mut tokio::process::Child,
    guard: &mut ProcessGroupGuard,
    process_pending: &mut bool,
) -> Result<(), String> {
    if let Err(error) = guard.kill() {
        return Err(format!(
            "could not terminate the command process group: {error}"
        ));
    }
    #[cfg(not(unix))]
    if let Err(error) = child.start_kill() {
        return Err(format!("could not terminate the command: {error}"));
    }
    match child.wait().await {
        Ok(_) => {
            *process_pending = false;
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

#[cfg(not(unix))]
pub(super) fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

/// Reads from a pipe that may already be closed; a closed side never resolves,
/// letting `select!` disable it without re-arming.
async fn read_from<R>(reader: &mut Option<R>, buffer: &mut [u8]) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match reader.as_mut() {
        Some(reader) => reader.read(buffer).await,
        None => std::future::pending().await,
    }
}

/// Forwards one raw output chunk as a lossy UTF-8 delta, bounded by the same
/// budget as the captured result so a runaway command cannot flood clients.
fn forward_shell_chunk(
    output: Option<&mpsc::Sender<super::ToolUpdate>>,
    streamed: &mut usize,
    bytes: &[u8],
) {
    let Some(sender) = output else {
        return;
    };
    if *streamed >= MAX_SHELL_OUTPUT_BYTES {
        return;
    }
    let remaining = MAX_SHELL_OUTPUT_BYTES - *streamed;
    let bytes = &bytes[..bytes.len().min(remaining)];
    let chunk = String::from_utf8_lossy(bytes).into_owned();
    match sender.try_send(super::ToolUpdate::Output(chunk)) {
        Ok(()) => *streamed += bytes.len(),
        // Live output is a best-effort view of the same bounded bytes carried
        // by the terminal result. A slow renderer may miss deltas, but it can
        // never stall process timeout, cancellation, or final persistence.
        Err(mpsc::error::TrySendError::Full(_)) => *streamed += bytes.len(),
        Err(mpsc::error::TrySendError::Closed(_)) => *streamed = MAX_SHELL_OUTPUT_BYTES,
    }
}

/// `shell exit=<code> elapsed=<s> bytes=<n>` then the captured output. The
/// header carries the verdict so a pruned stub, a head-only glance, or a
/// truncated tail all still tell the model how the command ended.
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
    result
}

/// Kills the process group on task destruction, unless disarmed after a
/// normal exit. Ordinary cancellation keeps the owning task alive to reap
/// the child; this guard also covers unwinding of that task.
pub(crate) struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
}

impl ProcessGroupGuard {
    pub(crate) fn new(child_id: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self {
                pgid: child_id
                    .and_then(|id| i32::try_from(id).ok())
                    .and_then(rustix::process::Pid::from_raw),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child_id;
            Self {}
        }
    }

    /// Forgets the group after a normal exit; the reaped leader's pid may be
    /// recycled, so killing the group then would be unsound.
    pub(crate) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }

    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            match rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => self.pgid = None,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // Destruction is a best-effort fallback. The task lease still records
        // an unconfirmed exit unless the owned Child was successfully reaped.
        let _ = self.kill();
    }
}

/// Stable identity of observed process output, independent of capture bounds,
/// pipe scheduling, elapsed time and advisory text. Each stream retains only
/// its incremental hash and length, even when output exceeds spill retention.
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

/// Bounded head+tail capture of combined command output: the first half of
/// the budget keeps the start of the output, the second half keeps a rolling
/// window of its end, and everything between is counted as omitted.
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

    /// Every byte the command wrote, including bytes the capture dropped.
    pub(super) fn total(&self) -> u64 {
        self.total
    }

    /// The captured bytes as text. When the capture itself dropped bytes the
    /// gap is marked here (a capture limit, distinct from output bounding);
    /// the model-facing bound is applied afterwards by dispatch.
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
                let mut process_pending = false;
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
                        &mut process_pending,
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
                assert!(
                    !process_pending,
                    "the owned child must be reaped before settlement"
                );
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
        let mut first_capture = BoundedCapture::new(MAX_SHELL_OUTPUT_BYTES);
        let mut second_capture = BoundedCapture::new(MAX_SHELL_OUTPUT_BYTES);
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
            let mut capture = BoundedCapture::new(MAX_SHELL_OUTPUT_BYTES);
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
