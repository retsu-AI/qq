pub(crate) mod ask;
pub(crate) mod commands;
pub(crate) mod dispatch;
mod edit;
#[cfg(feature = "tool-fetch")]
pub(crate) mod fetch;
pub(crate) mod goal;
mod lang;
mod matching;
#[cfg(not(target_os = "linux"))]
mod native_exit;
pub(crate) mod network;
pub mod output;
pub(crate) mod process_context;
#[cfg(target_os = "linux")]
pub(crate) mod process_group;
mod read;
mod search;
mod shell;
mod specs;
mod tree;
pub(crate) mod walk;
#[cfg(windows)]
pub(crate) mod windows_job;
mod write;

pub(crate) use dispatch::{ToolCancellation, ToolTaskLease};

pub(crate) async fn command_status(
    tasks: &ToolTasks,
    arguments: &str,
    cancelled: crate::RunCancellation,
) -> ToolOutput {
    tasks
        .commands()
        .status(arguments, &ToolCancellation::new(cancelled))
        .await
}

pub(crate) fn parsed_shell_command(arguments: &str) -> Option<String> {
    process_context::arguments_digest("shell", arguments)?;
    match serde_json::from_str::<shell::ShellArgs>(arguments) {
        Ok(arguments) => Some(arguments.command),
        Err(_) => None,
    }
}

#[cfg(test)]
pub(crate) use dispatch::{execute, test_executions_started};

#[cfg(any(not(unix), test))]
pub(crate) async fn run_goal_check_shell(
    workspace: &crate::workspace::Workspace,
    command: String,
    deadline: tokio::time::Instant,
    cancelled: crate::RunCancellation,
    producer: std::sync::Arc<crate::ProcessProducer>,
    process: &mut ToolTaskLease,
) -> (Option<dispatch::CommandOutcome>, String) {
    let arguments = shell::ShellArgs {
        command,
        cwd: None,
        timeout_seconds: None,
        yield_after_seconds: None,
    };
    let stop = crate::RunCancellation::new();
    let policy = crate::runtime::ShellPolicy::default();
    let stopped = ToolCancellation::new(stop.clone());
    let run = shell::run_shell(
        workspace,
        &policy,
        std::sync::Arc::default(),
        producer,
        None,
        shell::Launch::Shell(&arguments),
        &stopped,
        None,
        process,
        None,
    );
    tokio::pin!(run);
    let mut expired = false;
    let result = tokio::select! {
        result = &mut run => result,
        () = cancelled.cancelled() => { stop.cancel(); run.await }
        () = tokio::time::sleep_until(deadline) => { expired = true; stop.cancel(); run.await }
    };
    let outcome = match result.native_outcome {
        Some(dispatch::CommandOutcome::Cancelled) if expired => {
            Some(dispatch::CommandOutcome::TimedOut)
        }
        outcome => outcome,
    };
    (outcome, result.model_text)
}

pub mod bench_support {
    use std::path::Path;

    use crate::{
        RunCancellation,
        workspace::{FileState, Workspace},
    };

    pub fn run_tool(workspace_root: &Path, name: &str, arguments: &str) -> String {
        let workspace = Workspace::open(workspace_root).expect("workspace must open");
        super::dispatch::execute_blocking(
            &workspace,
            &FileState::default(),
            name,
            arguments,
            &super::dispatch::ToolCancellation::new(RunCancellation::new()),
        )
        .model_text
    }

    pub struct Session {
        workspace: Workspace,
        state: FileState,
    }

    impl Session {
        pub fn open(workspace_root: &Path) -> Self {
            Self {
                workspace: Workspace::open(workspace_root).expect("workspace must open"),
                state: FileState::default(),
            }
        }

        pub fn run(&self, name: &str, arguments: &str) -> (bool, String) {
            let output = super::dispatch::execute_blocking(
                &self.workspace,
                &self.state,
                name,
                arguments,
                &super::dispatch::ToolCancellation::new(RunCancellation::new()),
            );
            (output.is_error, output.model_text)
        }
    }
}
pub(crate) use dispatch::{
    ResultDisposition, SpillRecord, ToolDrainError, ToolOutput, ToolTasks, bounded_result,
    execute_with_images,
};
#[cfg(test)]
pub(crate) use edit::hold_tool_apply;
pub(crate) use edit::normalize_edits;
#[cfg(test)]
pub(crate) use output::MAX_MODEL_TEXT_BYTES;
pub(crate) use output::{ResultRecall, TurnOutputBudget, finalize_spill_marker, header_line};
#[cfg(test)]
pub(crate) use read::hold_tool_read;
pub(crate) use specs::{
    CancelAgentArgs, MAX_SPAWN_AGENT_SCHEMA_BYTES, MAX_WAIT_AGENTS_SECS, SPAWN_AGENT_TOOL,
    SpawnAgentArgs, WaitAgentsArgs, cancel_agent_spec, command_status_spec, minimal_call,
    spawn_agent_spec, spawn_model_routes, static_tools, wait_agents_spec,
};
#[cfg(test)]
pub(crate) use specs::{specs, test_tool_effect};

#[cfg(test)]
use crate::RunCancellation;
#[cfg(test)]
use crate::workspace::{FileState, Workspace, content_hash};
#[cfg(test)]
use dispatch::execute_blocking;
#[cfg(test)]
use output::{MARKER_PREFIX, escaped_len};
#[cfg(test)]
use read::MAX_READ_SCAN_BYTES;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use shell::BoundedCapture;
#[cfg(all(test, unix))]
use shell::SHELL_BOUNDS;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::mpsc;

#[cfg(all(test, unix))]
pub(crate) use shell::hold_shell_receipt;
#[cfg(all(test, any(unix, windows)))]
pub(crate) use shell::observe_shell_spawn;

#[cfg(all(test, unix))]
pub(crate) async fn create_test_fifo(path: &std::path::Path) {
    let mut child = tokio::process::Command::new("mkfifo")
        .args(["-m", "600"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the native FIFO fixture utility");
    let status = match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
        Ok(status) => status.expect("wait for the FIFO fixture utility"),
        Err(_) => {
            child
                .kill()
                .await
                .expect("reap the timed-out FIFO fixture utility");
            panic!("FIFO fixture creation timed out");
        }
    };
    assert!(status.success(), "the native FIFO fixture utility failed");
}

#[cfg(all(test, unix))]
pub(crate) const PANIC_SHELL_ARGUMENTS: &str = r#"{"command":"sleep 300"}"#;
#[cfg(all(test, windows))]
pub(crate) const PANIC_SHELL_ARGUMENTS: &str =
    r#"{"command":"for /L %i in (1,1,2147483647) do @rem waiting"}"#;

#[cfg(all(test, unix))]
pub(crate) async fn assert_panicked_process_exits(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
            Ok(Some((reaped, status))) => {
                assert_eq!(reaped, pid);
                assert!(status.exited() || status.signaled());
                return;
            }
            Err(rustix::io::Errno::CHILD) => return,
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => panic!("cannot wait for the panicked fixture's child: {error}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the process guard must still send termination on panic"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(all(test, windows))]
pub(crate) async fn assert_panicked_process_exits(pid: u32) {
    tokio::task::spawn_blocking(move || {
        let process = match win_custody::Process::open(pid, win_custody::ProcessAccess::Synchronize)
        {
            Ok(process) => process,
            Err(error) if error.raw_os_error() == Some(87) => return,
            Err(error) => panic!("cannot observe the panicked fixture's process: {error}"),
        };
        assert_eq!(
            process
                .wait(Some(std::time::Duration::from_secs(10)))
                .unwrap(),
            win_custody::WaitOutcome::Signaled,
            "the owned process must still exit after panic",
        );
    })
    .await
    .unwrap();
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod goal_check_shell_tests {
    #[tokio::test]
    async fn goal_check_shell_keeps_the_absolute_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::workspace::Workspace::open(directory.path()).unwrap();
        let tasks = super::ToolTasks::default();
        for (command, budget, expected) in [
            (
                "exit 3",
                5_000,
                super::dispatch::CommandOutcome::Exited { code: Some(3) },
            ),
            (
                "exec tail -f /dev/null",
                300,
                super::dispatch::CommandOutcome::TimedOut,
            ),
        ] {
            let mut lease = tasks.enter();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(budget);
            let (outcome, _) = super::run_goal_check_shell(
                &workspace,
                command.to_owned(),
                deadline,
                crate::RunCancellation::new(),
                std::sync::Arc::new(crate::ProcessProducer::new("test")),
                &mut lease,
            )
            .await;
            assert_eq!(outcome, Some(expected), "{command}");
            assert!(
                tokio::time::Instant::now() < deadline + std::time::Duration::from_millis(500),
                "{command} outlived its absolute deadline"
            );
        }
        tasks.drain().await.unwrap();
    }
}
