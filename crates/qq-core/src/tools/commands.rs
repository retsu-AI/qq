//! Commands that outlive their call. A `shell` or `exec` call still running
//! after `yield_after_seconds` returns a command id; `command_status` then
//! waits on it, reads its output so far, or cancels it. The dispatch task
//! still owns the process through kill and reap, under its task lease, so a
//! run's teardown stops every yielded command before it settles.

use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use tokio::sync::watch;

use super::{
    dispatch::{CallCancellation, ToolCancellation, ToolOutput},
    output::Header,
    shell::{BoundedCapture, SHELL_BOUNDS},
};

/// How long a command runs before its call returns a command id.
pub(super) const DEFAULT_YIELD_AFTER_SECS: u64 = 30;
/// The longest one `command_status` call blocks its turn.
pub(crate) const MAX_COMMAND_WAIT_SECS: u64 = 600;
pub(super) const DEFAULT_COMMAND_WAIT_SECS: u64 = 30;
/// Yielded commands one run keeps at once, running or finished but unread.
/// A full table forgets its oldest unread finished command; when all are
/// running, a new command runs to completion in its call instead.
const MAX_YIELDED_COMMANDS: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandStatusArgs {
    id: String,
    #[serde(default = "default_wait")]
    wait_seconds: u64,
    #[serde(default)]
    cancel: bool,
}

const fn default_wait() -> u64 {
    DEFAULT_COMMAND_WAIT_SECS
}

/// One run's yielded commands, by the id their call returned.
#[derive(Default)]
pub(crate) struct Commands(Mutex<Table>);

#[derive(Default)]
struct Table {
    /// Numbers commands that have no call id (direct test executions).
    unnamed: u64,
    commands: Vec<(String, Arc<Command>)>,
}

pub(super) struct Command {
    pub(super) id: String,
    tool: &'static str,
    started: tokio::time::Instant,
    capture: Arc<Mutex<BoundedCapture>>,
    cancel: Arc<CallCancellation>,
    result: watch::Sender<Option<ToolOutput>>,
}

impl Command {
    /// Publishes the command's terminal result for `command_status`.
    pub(super) fn finish(&self, mut result: ToolOutput) {
        // The receipt is bound to the call that started the command and was
        // persisted with its yield result; a status call carries none.
        result.execution_context = None;
        result.native_outcome = None;
        self.result.send_replace(Some(result));
    }

    #[cfg(test)]
    pub(super) fn cancel_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn running(&self) -> ToolOutput {
        let capture = self
            .capture
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut content = Header::new("command_status", None)
            .field("id", &self.id)
            .field("state", "running")
            .field(
                "elapsed",
                format_args!("{:.1}", self.started.elapsed().as_secs_f64()),
            )
            .field("bytes", capture.total())
            .into_line();
        content.push_str(&capture.into_output());
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!(
            "{} {} is still running; call command_status again to keep waiting, or with cancel=true to stop it",
            self.tool, self.id
        ));
        ToolOutput::bounded(content, &SHELL_BOUNDS, false)
    }
}

impl Commands {
    /// Registers a running command, or `None` when the table is full. The
    /// id is `c` and the call id's first eight hex digits, so an id from an
    /// earlier run of the session never names a command of this one.
    pub(super) fn register(
        &self,
        call: Option<qq_protocol::ToolCallId>,
        tool: &'static str,
        started: tokio::time::Instant,
        capture: Arc<Mutex<BoundedCapture>>,
        cancel: Arc<CallCancellation>,
    ) -> Option<Arc<Command>> {
        let mut table = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if table.commands.len() >= MAX_YIELDED_COMMANDS {
            let finished = table
                .commands
                .iter()
                .position(|(_, command)| command.result.borrow().is_some())?;
            table.commands.remove(finished);
        }
        let id = match call {
            Some(call) => format!("c{}", &call.to_string()[..8]),
            None => {
                table.unnamed += 1;
                format!("c{}", table.unnamed)
            }
        };
        if table.commands.iter().any(|(known, _)| *known == id) {
            return None;
        }
        let command = Arc::new(Command {
            id: id.clone(),
            tool,
            started,
            capture,
            cancel,
            result: watch::Sender::new(None),
        });
        table.commands.push((id, Arc::clone(&command)));
        Some(command)
    }

    pub(super) fn forget(&self, id: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .commands
            .retain(|(known, _)| known != id);
    }

    #[cfg(test)]
    pub(crate) async fn all_finished(&self) {
        let commands = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .commands
            .iter()
            .map(|(_, command)| command.result.subscribe())
            .collect::<Vec<_>>();
        for mut result in commands {
            let _ = result.wait_for(Option::is_some).await;
        }
    }

    fn find(&self, id: &str) -> Option<Arc<Command>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .commands
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, command)| Arc::clone(command))
    }

    /// Stops every yielded command; their tasks still kill and reap them.
    pub(super) fn cancel_all(&self) {
        for (_, command) in &self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .commands
        {
            command.cancel.cancel();
        }
    }

    pub(super) async fn status(&self, arguments: &str, cancelled: &ToolCancellation) -> ToolOutput {
        let arguments: CommandStatusArgs = match serde_json::from_str(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutput::refused(format!("invalid arguments: {error}")),
        };
        if arguments.wait_seconds > MAX_COMMAND_WAIT_SECS {
            return ToolOutput::refused(format!(
                "wait_seconds must be at most {MAX_COMMAND_WAIT_SECS}"
            ));
        }
        let id = arguments.id.trim();
        let Some(command) = self.find(id) else {
            return ToolOutput::refused(format!(
                "unknown_command: no running command {id:?} in this run; ids come from a shell or exec result that says it is still running"
            ));
        };
        // A cancel waits for the kill and reap, still bounded: a task that
        // never finishes must not hold the turn.
        let wait = if arguments.cancel {
            command.cancel.cancel();
            arguments.wait_seconds.max(DEFAULT_COMMAND_WAIT_SECS)
        } else {
            arguments.wait_seconds
        };
        let mut result = command.result.subscribe();
        let finished = tokio::select! {
            biased;
            () = cancelled.cancelled() => return ToolOutput::interrupted("tool execution was cancelled"),
            finished = result.wait_for(Option::is_some) => finished.ok().and_then(|result| result.clone()),
            () = tokio::time::sleep(std::time::Duration::from_secs(wait)) => None,
        };
        match finished {
            Some(result) => {
                self.forget(id);
                result
            }
            None => command.running(),
        }
    }
}
