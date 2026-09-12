use serde::Deserialize;
use tokio::{io::AsyncReadExt, sync::mpsc};

use crate::workspace::Workspace;

use super::{
    dispatch::{ToolCancellation, ToolOutput},
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
pub(super) const MAX_SHELL_TIMEOUT_SECS: u64 = 600;
const SHELL_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// The fate of one supervised shell command.
enum ShellOutcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Executes one bounded shell command on the async runtime: `sh -c` in its own
/// process group with the workspace (or a contained subdirectory) as its
/// working directory, combined stdout+stderr captured head+tail within the
/// output budget, live chunks forwarded to `output`, and the whole process
/// group killed on timeout or cancellation. The dispatch task owns this
/// future through kill and reap even when its result waiter disappears.
pub(super) async fn run_shell(
    workspace: &Workspace,
    arguments: &ShellArgs,
    cancelled: &ToolCancellation,
    output: Option<&mpsc::Sender<String>>,
    process_pending: &mut bool,
) -> ToolOutput {
    if cancelled.is_cancelled() {
        return ToolOutput::error("tool execution was cancelled");
    }
    if arguments.command.trim().is_empty() {
        return ToolOutput::error("command must not be empty");
    }
    let timeout_seconds = arguments
        .timeout_seconds
        .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS);
    if timeout_seconds == 0 || timeout_seconds > MAX_SHELL_TIMEOUT_SECS {
        return ToolOutput::error(format!(
            "timeout_seconds must be between 1 and {MAX_SHELL_TIMEOUT_SECS}"
        ));
    }
    let cwd = match &arguments.cwd {
        None => workspace.path().to_owned(),
        Some(requested) => {
            let relative = match workspace.contained_path(requested) {
                Ok(relative) => relative,
                Err(error) => return ToolOutput::error(error.to_string()),
            };
            if !workspace.root().is_dir(&relative) {
                return ToolOutput::error("cwd is not a directory");
            }
            workspace.path().join(relative)
        }
    };

    let mut command = shell_command(&arguments.command);
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ToolOutput::error(format!("could not start the command: {error}"));
        }
    };
    *process_pending = true;
    // The child leads its own process group (pgid == pid); the guard kills the
    // entire group on every abnormal exit path — timeout, cancellation, and
    // this future being dropped mid-flight — so no descendant is orphaned.
    let mut guard = ProcessGroupGuard::new(child.id());
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
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let started = tokio::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(timeout_seconds);
    let mut capture = BoundedCapture::new(MAX_SHELL_OUTPUT_BYTES);
    let mut streamed = 0_usize;
    let mut stdout_buffer = vec![0_u8; SHELL_READ_CHUNK_BYTES];
    let mut stderr_buffer = vec![0_u8; SHELL_READ_CHUNK_BYTES];
    let mut cancel_poll = tokio::time::interval(SHELL_CANCEL_POLL);
    cancel_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome = loop {
        tokio::select! {
            biased;
            () = cancelled.caller_dropped() => break ShellOutcome::Cancelled,
            () = tokio::time::sleep_until(deadline) => break ShellOutcome::TimedOut,
            _ = cancel_poll.tick() => {
                if cancelled.is_cancelled() {
                    break ShellOutcome::Cancelled;
                }
            }
            read = read_from(&mut stdout, &mut stdout_buffer), if stdout.is_some() => {
                match read {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(read) => {
                        forward_shell_chunk(output, &mut streamed, &stdout_buffer[..read]);
                        capture.push(&stdout_buffer[..read]);
                    }
                }
            }
            read = read_from(&mut stderr, &mut stderr_buffer), if stderr.is_some() => {
                match read {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(read) => {
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
    match outcome {
        ShellOutcome::Exited(Ok(status)) => shell_result(capture, status, elapsed),
        ShellOutcome::Exited(Err(error)) => {
            ToolOutput::error(format!("could not observe the command exit: {error}"))
        }
        ShellOutcome::TimedOut => {
            if let Err(message) = stop_shell(&mut child, &mut guard, process_pending).await {
                return ToolOutput::error(message);
            }
            let mut content = Header::new("shell", None)
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
            ToolOutput::bounded(content, &SHELL_BOUNDS, true)
        }
        ShellOutcome::Cancelled => {
            if let Err(message) = stop_shell(&mut child, &mut guard, process_pending).await {
                return ToolOutput::error(message);
            }
            ToolOutput::error("tool execution was cancelled")
        }
    }
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
fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("/bin/sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(not(unix))]
fn shell_command(command: &str) -> tokio::process::Command {
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
fn forward_shell_chunk(output: Option<&mpsc::Sender<String>>, streamed: &mut usize, bytes: &[u8]) {
    let Some(sender) = output else {
        return;
    };
    if *streamed >= MAX_SHELL_OUTPUT_BYTES {
        return;
    }
    let remaining = MAX_SHELL_OUTPUT_BYTES - *streamed;
    let bytes = &bytes[..bytes.len().min(remaining)];
    let chunk = String::from_utf8_lossy(bytes).into_owned();
    match sender.try_send(chunk) {
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
    capture: BoundedCapture,
    status: std::process::ExitStatus,
    elapsed: std::time::Duration,
) -> ToolOutput {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut header = Header::new("shell", None);
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
    match status.code() {
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
    }
}

/// Kills the process group on task destruction, unless disarmed after a
/// normal exit. Ordinary cancellation keeps the owning task alive to reap
/// the child; this guard also covers unwinding of that task.
struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
}

impl ProcessGroupGuard {
    fn new(child_id: Option<u32>) -> Self {
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
    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            match rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
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
