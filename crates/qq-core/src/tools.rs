pub(crate) mod ask;
pub(crate) mod dispatch;
mod edit;
#[cfg(feature = "tool-fetch")]
pub(crate) mod fetch;
pub(crate) mod goal;
mod lang;
#[cfg(target_os = "linux")]
pub(crate) mod linux_owner;
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
#[cfg_attr(
    all(test, target_os = "linux"),
    allow(
        dead_code,
        reason = "compile shared Unix launch interfaces and validate grants without claiming macOS enforcement"
    )
)]
pub(crate) mod macos_owner;
mod matching;
mod native_exit;
pub(crate) mod network;
pub mod output;
pub(crate) mod process_context;
mod read;
mod search;
mod shell;
#[cfg(windows)]
pub(crate) mod windows_owner;
pub(crate) use shell::forward_shell_chunk;
mod specs;
pub(crate) mod task_git;
mod tree;
pub(crate) mod walk;
#[cfg(windows)]
pub(crate) mod windows_job;
mod write;

pub(crate) use dispatch::{ToolCancellation, ToolTaskLease};
pub(crate) use shell::MAX_SHELL_OUTPUT_BYTES;
#[cfg(unix)]
pub(crate) use shell::ProcessGroupGuard;

/// Parse qualification envelopes with the same type used by shell dispatch.
pub(crate) fn parsed_shell_command(arguments: &str) -> Option<(String, bool)> {
    process_context::arguments_digest("shell", arguments)?;
    match serde_json::from_str::<shell::ShellArgs>(arguments) {
        Ok(arguments) => Some((arguments.command, !arguments.env.is_empty())),
        Err(_) => None, // Malformed envelopes remain non-read without exposing their text.
    }
}

#[cfg(test)]
pub(crate) use dispatch::{execute, test_executions_started};

/// Entry points for the `search_walk` and `edit_batch` benches. Not a
/// public API.
pub mod bench_support {
    use std::path::Path;

    use crate::{
        RunCancellation,
        workspace::{FileState, Workspace},
    };

    /// Runs one built-in read-side tool against `workspace_root` and returns
    /// its model-facing text.
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

    /// A session's file-state map for a sequence of tool calls, so a bench
    /// can read then edit the way a run does.
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

        /// Runs one built-in tool and returns `(is_error, model_text)`.
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
#[cfg(test)]
pub(crate) use output::MAX_MODEL_TEXT_BYTES;
pub(crate) use output::{ResultRecall, TurnOutputBudget, finalize_spill_marker, header_line};
#[cfg(test)]
pub(crate) use read::hold_tool_read;
pub(crate) use specs::{
    CancelAgentArgs, MAX_SPAWN_AGENT_SCHEMA_BYTES, MAX_WAIT_AGENTS_SECS, SPAWN_AGENT_TOOL,
    SpawnAgentArgs, WaitAgentsArgs, cancel_agent_spec, spawn_agent_spec, spawn_model_routes,
    static_tools, wait_agents_spec,
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
pub(crate) use shell::{limit_shell_read_chunks, observe_shell_spawn, observe_shell_termination};

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
        // Panic deliberately leaves production cleanup unconfirmed. Reap only
        // this fixture's child, without mistaking a dead zombie for a live process.
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
            Ok(Some((reaped, status))) => {
                assert_eq!(reaped, pid);
                assert!(status.exited() || status.signaled());
                return;
            }
            Err(rustix::io::Errno::CHILD) => return, // Tokio already reaped it.
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
    // Observe the kernel process object directly. Launching PowerShell adds a
    // second process startup to the timeout and cannot strengthen this proof.
    // This fixture observation never confirms production tool quiescence.
    tokio::task::spawn_blocking(move || {
        let process = match win_custody::Process::open(pid, win_custody::ProcessAccess::Synchronize)
        {
            Ok(process) => process,
            // ERROR_INVALID_PARAMETER means the process left the PID table.
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
mod tests {
    use std::fs;

    use super::*;

    fn run_tool(
        workspace: &Workspace,
        state: &FileState,
        name: &str,
        arguments: &str,
    ) -> ToolOutput {
        execute_blocking(
            workspace,
            state,
            name,
            arguments,
            &ToolCancellation::new(RunCancellation::new()),
        )
    }

    #[test]
    fn tool_result_provenance_separates_execution_from_argument_refusal() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "one\ntwo\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for (name, args) in [
            ("read_file", r#"{"path":3}"#),
            ("read_file", r#"{"path":"input.txt","limit":0}"#),
            ("tree", r#"{"path":".","depth":0}"#),
            ("search", r#"{"query":"one","limit":0}"#),
            ("write_file", r#"{"path":"x","content":3}"#),
            ("unknown_tool", "{}"),
        ] {
            let result = run_tool(&workspace, &state, name, args);
            assert!(result.is_error, "{name} {args}");
            assert_eq!(
                result.disposition,
                ResultDisposition::Refused,
                "{name} {args}"
            );
        }
        let missing = run_tool(&workspace, &state, "read_file", r#"{"path":"missing.txt"}"#);
        assert!(missing.is_error);
        assert_eq!(missing.disposition, ResultDisposition::Observed);
        let repaired = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"one","include":"[\"*.txt\"]"}"#,
        );
        assert!(!repaired.is_error);
        assert_eq!(
            repaired.disposition,
            ResultDisposition::Observed,
            "argument-repair transfer must preserve provenance"
        );
        let cancelled = RunCancellation::new();
        cancelled.cancel();
        let stopped = execute_blocking(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"input.txt"}"#,
            &ToolCancellation::new(cancelled),
        );
        assert_eq!(stopped.disposition, ResultDisposition::Interrupted);
    }

    #[test]
    fn mutation_argument_refusals_are_not_execution_observations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.txt");
        fs::write(&path, "alpha\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for (name, arguments) in [
            (
                "write_file",
                r#"{"path":"new.txt","content":"x","if_hash":"bad"}"#,
            ),
            ("edit_file", r#"{"edits":[]}"#),
            (
                "edit_file",
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"alpha"}]}"#,
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"beta","if_hash":"bad"}]}"#,
            ),
        ] {
            let result = run_tool(&workspace, &state, name, arguments);
            assert!(result.is_error, "{name}: {}", result.model_text);
            assert_eq!(
                result.disposition,
                ResultDisposition::Refused,
                "{name} {arguments}"
            );
            assert!(result.file_states.is_empty());
        }
        let arguments = serde_json::json!({
            "edits": vec![serde_json::json!({"path":"a.txt","old":"alpha","new":"beta"}); 33]
        })
        .to_string();
        let result = run_tool(&workspace, &state, "edit_file", &arguments);
        assert_eq!(result.disposition, ResultDisposition::Refused);
        assert_eq!(fs::read(&path).unwrap(), b"alpha\n");
        assert!(!directory.path().join("new.txt").exists());
        let missing = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"missing.txt","old":"alpha","new":"beta"}]}"#,
        );
        assert_eq!(
            missing.disposition,
            ResultDisposition::Observed,
            "a completed missing-file lookup remains an observation"
        );
    }

    #[test]
    fn mutation_leaf_cancellation_is_not_an_execution_observation() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let cancellation = RunCancellation::new();
        cancellation.cancel();
        let cancelled = ToolCancellation::new(cancellation);
        let write_args = serde_json::from_str(r#"{"path":"new.txt","content":"x"}"#).unwrap();
        let write_result = write::write_file(&workspace, &state, &write_args, &cancelled);
        assert_eq!(write_result.disposition, ResultDisposition::Interrupted);
        let edit_args =
            serde_json::from_str(r#"{"edits":[{"path":"a.txt","old":"alpha","new":"beta"}]}"#)
                .unwrap();
        let edit_result = edit::edit_file(&workspace, &state, &edit_args, &cancelled);
        assert_eq!(edit_result.disposition, ResultDisposition::Interrupted);
        assert_eq!(
            fs::read(directory.path().join("a.txt")).unwrap(),
            b"alpha\n"
        );
        assert!(!directory.path().join("new.txt").exists());
    }

    #[test]
    fn host_failures_do_not_invent_observed_tool_results() {
        use crate::hosts::HostCallError;
        for error in [
            HostCallError::Timeout,
            HostCallError::Unavailable("offline".to_owned()),
            HostCallError::Overloaded,
            HostCallError::InvalidResult("bad".to_owned()),
            HostCallError::Refused("policy".to_owned()),
            HostCallError::UnknownTool("missing".to_owned()),
            HostCallError::ShutDown,
        ] {
            let result = crate::hosts::host_error_result(&error);
            assert!(result.is_error);
            let expected = match error {
                HostCallError::Timeout
                | HostCallError::Unavailable(_)
                | HostCallError::InvalidResult(_)
                | HostCallError::ShutDown => ResultDisposition::Unobserved,
                HostCallError::Overloaded
                | HostCallError::Refused(_)
                | HostCallError::UnknownTool(_) => ResultDisposition::Refused,
                HostCallError::Cancelled => unreachable!("cancellation checked separately"),
            };
            assert_eq!(result.disposition, expected, "{error}");
        }
        assert_eq!(
            crate::hosts::host_error_result(&HostCallError::Cancelled).disposition,
            ResultDisposition::Interrupted
        );
        let response = bounded_result("remote tool ran and reported an error".to_owned(), true);
        assert_eq!(response.disposition, ResultDisposition::Observed);
    }

    #[test]
    fn complete_result_identity_survives_identical_previews_and_discarded_spills() {
        use sha2::{Digest, Sha256};
        let bounds = output::Bounds {
            max_bytes: 128,
            max_lines: 8,
            max_line_bytes: 32,
            head_ratio: 50,
        };
        for bytes in [16 * 1024, output::MAX_SPILL_ITEM_BYTES + 1024] {
            let a = format!("head\n{}tail\n", "x\n".repeat(bytes / 2));
            let mut b = a.clone();
            let middle = "head\n".len() + 2 * (bytes / 4);
            b.replace_range(middle..middle + 1, "y");
            let first = ToolOutput::bounded(a.clone(), &bounds, false);
            let second = ToolOutput::bounded(b.clone(), &bounds, false);
            assert_eq!(
                first.model_text, second.model_text,
                "only omitted bytes differ"
            );
            assert_ne!(first.result_digest, second.result_digest);
            assert_eq!(
                first.result_digest,
                <[u8; 32]>::from(Sha256::digest(a.as_bytes()))
            );
            if bytes > output::MAX_SPILL_ITEM_BYTES {
                assert!(first.spill.is_none());
                assert!(second.spill.is_none());
            } else {
                assert_eq!(
                    first.spill.as_ref().unwrap().digest,
                    format!("{:x}", Sha256::digest(a.as_bytes()))
                );
            }
            let differently_bounded = ToolOutput::bounded(a, &output::Bounds::DEFAULT, false);
            assert_eq!(
                first.result_digest, differently_bounded.result_digest,
                "bounds do not define result identity"
            );
        }
    }

    #[test]
    fn readonly_argument_repairs_are_noted_without_changing_the_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "needle\nsecond\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for (name, valid, malformed, note) in [
            (
                "search",
                r#"{"query":"needle","include":["*.txt"]}"#,
                r#"{"query":"needle","include":"[\"*.txt\"]"}"#,
                "include_array_coerced",
            ),
            (
                "search",
                r#"{"query":"needle","exclude":["*.rs"]}"#,
                r#"{"query":"needle","exclude":"[\"*.rs\"]"}"#,
                "exclude_array_coerced",
            ),
            (
                "read_file",
                r#"{"path":"input.txt","ranges":["1-2"]}"#,
                r#"{"path":"input.txt","ranges":"[\"1-2\"]"}"#,
                "ranges_array_coerced",
            ),
            (
                "search",
                r#"{"query":"needle"}"#,
                r#"{"query":"needle","cwd":"."}"#,
                "cwd_ignored",
            ),
            (
                "search",
                r#"{"query":"needle"}"#,
                r#"{"query":"needle","offset":0}"#,
                "offset_ignored",
            ),
            (
                "tree",
                r#"{"path":"."}"#,
                r#"{"path":".","cwd":"."}"#,
                "cwd_ignored",
            ),
            (
                "read_file",
                r#"{"path":"input.txt"}"#,
                r#"{"path":"input.txt","cwd":"."}"#,
                "cwd_ignored",
            ),
            (
                "search",
                r#"{"query":"needle","include":["*.txt"],"context":10}"#,
                r#"{"query":"needle","include":"[\"*.txt\"]","context":10}"#,
                "include_array_coerced",
            ),
        ] {
            let expected = run_tool(&workspace, &state, name, valid);
            let repaired = run_tool(&workspace, &state, name, malformed);
            assert!(!expected.is_error, "{}", expected.model_text);
            assert!(!repaired.is_error, "{name}: {}", repaired.model_text);
            assert_eq!(
                repaired.model_text.replace(&format!(" note={note}"), ""),
                expected.model_text,
                "{name}: repair changed the result"
            );
            assert_eq!(repaired.file_states, expected.file_states);
        }
    }

    #[test]
    fn readonly_argument_repairs_preserve_authority_duplicates_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "needle\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for (name, arguments) in [
            ("search", r#"{"query":"needle","cwd":".."}"#),
            ("search", r#"{"query":"needle","cwd":"sub"}"#),
            ("search", r#"{"query":"needle","offset":1}"#),
            ("search", r#"{"query":"needle","include":"[1]"}"#),
            (
                "search",
                r#"{"query":"needle","include":"[\"*.txt\"]","query":"other"}"#,
            ),
            (
                "search",
                r#"{"include":"[\"*.txt\"]","path":".","path":"..","query":"needle"}"#,
            ),
            (
                "search",
                r#"{"query":"needle","include":"[\"*.txt\"]","max_per_file":51}"#,
            ),
            (
                "search",
                r#"{"query":"needle","include":"[\"*.txt\"]","include_ignored":"true"}"#,
            ),
            (
                "search",
                r#"{"query":"needle","include":"[\"*.txt\"]","approval":"allow"}"#,
            ),
            (
                "read_file",
                r#"{"path":"input.txt","ranges":"[\"1\",\"1\",\"1\",\"1\",\"1\",\"1\",\"1\",\"1\",\"1\"]"}"#,
            ),
            ("read_file", r#"{"path":"../input.txt","ranges":"[\"1\"]"}"#),
            (
                "read_file",
                r#"{"path":"input.txt","include_ignored":"yes"}"#,
            ),
            (
                "exec",
                r#"{"program":"printf","args":"[\"bad\"]","command":"echo bad"}"#,
            ),
            (
                "write_file",
                r#"{"path":"new.txt","content":"bad","cwd":"."}"#,
            ),
        ] {
            let result = run_tool(&workspace, &state, name, arguments);
            assert!(
                result.is_error,
                "accepted {arguments}: {}",
                result.model_text
            );
        }
        assert!(!directory.path().join("new.txt").exists());
        assert_eq!(
            fs::read_to_string(directory.path().join("input.txt")).unwrap(),
            "needle\n"
        );
    }

    #[test]
    fn readonly_argument_repairs_keep_continuation_and_hashes_when_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let contents = ("a".repeat(100) + "\n").repeat(1_000);
        fs::write(directory.path().join("input.txt"), &contents).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let expected = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"input.txt","ranges":["1-1000"]}"#,
        );
        let result = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"input.txt","ranges":"[\"1-1000\"]"}"#,
        );
        assert!(!result.is_error, "{}", result.model_text);
        assert!(result.model_text.len() <= read::READ_BOUNDS.max_bytes);
        assert!(
            result
                .model_text
                .lines()
                .next()
                .unwrap()
                .contains("note=ranges_array_coerced")
        );
        assert!(result.model_text.contains("continue from offset="));
        assert_eq!(
            result.model_text.replace(" note=ranges_array_coerced", ""),
            expected.model_text
        );
        if let Some(spill) = &result.spill {
            assert!(
                spill
                    .text
                    .lines()
                    .next()
                    .unwrap()
                    .contains("note=ranges_array_coerced")
            );
            assert_eq!(spill.digest, content_hash(spill.text.as_bytes()));
        }
        assert_eq!(result.file_states.len(), 1);
        assert_eq!(
            result.file_states[0].hash,
            content_hash(contents.as_bytes())
        );
    }

    #[test]
    fn readonly_argument_repairs_preserve_complete_search_text_and_original_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let query = "needle";
        let nested = directory.path().join(vec!["a".repeat(120); 15].join("/"));
        fs::create_dir_all(&nested).unwrap();
        for index in 0..20 {
            fs::write(
                nested.join(format!("input{index:02}.txt")),
                "needle abcdefgh\n".repeat(50),
            )
            .unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        let baseline = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            &json!({
                "query": query, "limit": 500, "include": ["**/*.txt"], "max_per_file": 50
            })
            .to_string(),
        );
        let arguments = json!({
            "query": query,
            "limit": 500,
            "include": "[\"**/*.txt\"]",
            "max_per_file": 50,
            "cwd": ".",
            "offset": 0,
        })
        .to_string();
        let result = run_tool(&workspace, &FileState::default(), "search", &arguments);
        assert!(!result.is_error, "{}", result.model_text);
        assert!(result.model_text.len() <= search::SEARCH_BOUNDS.max_bytes);
        let header = result.model_text.lines().next().unwrap();
        assert!(header.contains("next="), "{header}");
        assert!(
            result
                .model_text
                .lines()
                .any(|line| line == "note=include_array_coerced,cwd_ignored,offset_ignored"),
            "{}",
            result.model_text
        );
        assert!(result.spill.is_none(), "complete cursor header must fit");
        assert!(baseline.spill.is_none());
        let encoded = header
            .split_once(" next=")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap();
        assert!(search::Cursor::decode(encoded).is_some());
        assert_eq!(
            result.model_text.replace(
                "note=include_array_coerced,cwd_ignored,offset_ignored\n",
                ""
            ),
            baseline.model_text,
            "repair must preserve complete bounded content and its cursor"
        );
    }

    #[test]
    fn readonly_argument_repairs_keep_search_pagination_usable() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "needle\n".repeat(50)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let first = run_tool(
            &workspace,
            &state,
            "search",
            &json!({
                "query": "needle", "include": "[\"*.txt\"]", "max_per_file": 50, "limit": 25
            })
            .to_string(),
        );
        assert!(!first.is_error, "{}", first.model_text);
        let encoded = first
            .model_text
            .lines()
            .next()
            .unwrap()
            .split_once(" next=")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap();
        let cursor = search::Cursor::decode(encoded).expect("first page cursor is usable");
        assert_eq!(cursor.path, "input.txt");
        assert_eq!(cursor.line, 25);
        let second = run_tool(
            &workspace,
            &state,
            "search",
            &json!({
                "query": "needle", "include": "[\"*.txt\"]", "max_per_file": 50,
                "limit": 25, "cursor": encoded
            })
            .to_string(),
        );
        assert!(!second.is_error, "{}", second.model_text);
        assert!(
            second.model_text.contains("matches=25/25"),
            "{}",
            second.model_text
        );
        assert!(!second.model_text.lines().next().unwrap().contains("next="));
        assert!(second.model_text.contains("note=include_array_coerced"));
    }

    #[test]
    fn readonly_argument_repairs_remain_visible_for_long_paths() {
        let directory = tempfile::tempdir().unwrap();
        let relative = vec!["a".repeat(120); 18].join("/");
        let nested = directory.path().join(&relative);
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("input.txt"), "needle\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        for (name, path) in [
            ("tree", relative.clone()),
            ("read_file", format!("{relative}/input.txt")),
        ] {
            let arguments = json!({"path": path, "cwd": "."}).to_string();
            let result = run_tool(&workspace, &FileState::default(), name, &arguments);
            assert!(!result.is_error, "{}", result.model_text);
            assert!(
                result
                    .model_text
                    .split_whitespace()
                    .any(|field| field == "note=cwd_ignored"),
                "{}",
                result.model_text
            );
        }
    }

    #[test]
    fn readonly_argument_repairs_drop_observed_irrelevant_read_ignore_flag() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "needle\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let expected = run_tool(&workspace, &state, "read_file", r#"{"path":"input.txt"}"#);
        for value in [true, false] {
            let result = run_tool(
                &workspace,
                &state,
                "read_file",
                &json!({"path":"input.txt", "include_ignored": value}).to_string(),
            );
            assert!(!result.is_error, "{}", result.model_text);
            assert_eq!(
                result
                    .model_text
                    .replace(" note=include_ignored_ignored", ""),
                expected.model_text
            );
            assert_eq!(result.file_states, expected.file_states);
        }
    }

    #[test]
    fn readonly_argument_repairs_note_execution_errors() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"missing.txt","ranges":"[\"1\"]"}"#,
        );
        assert!(result.is_error);
        assert!(result.model_text.contains("path_not_found"));
        assert!(
            result.model_text.contains("note=ranges_array_coerced"),
            "{}",
            result.model_text
        );
    }

    #[test]
    fn readonly_argument_repairs_preserve_line_clipping_counts() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "a".repeat(3_000)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let typed = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"input.txt","ranges":["1"]}"#,
        );
        let repaired = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"input.txt","ranges":"[\"1\"]"}"#,
        );
        assert!(typed.spill.is_none());
        assert!(typed.model_text.contains("…+"));
        assert_eq!(
            repaired
                .model_text
                .replace(" note=ranges_array_coerced", ""),
            typed.model_text
        );
    }

    #[test]
    fn readonly_argument_repairs_preserve_the_prunable_search_header() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join(vec!["a".repeat(102); 3].join("/"));
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("input.txt"), "needle\n".repeat(50)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let typed = run_tool(
            &workspace,
            &state,
            "search",
            &json!({"query":"needle","limit":25,"max_per_file":50,"include":["**/*.txt"]})
                .to_string(),
        );
        let header = typed.model_text.lines().next().unwrap();
        assert!(
            header.len() <= output::MAX_STUB_HEADER_BYTES,
            "{}",
            header.len()
        );
        assert!(
            header.len() + " note=include_array_coerced,cwd_ignored".len()
                > output::MAX_STUB_HEADER_BYTES,
            "{}",
            header.len()
        );
        let repaired = run_tool(&workspace, &state, "search", &json!({"query":"needle","limit":25,"max_per_file":50,"include":"[\"**/*.txt\"]","cwd":"."}).to_string());
        assert_eq!(repaired.model_text.lines().next().unwrap(), header);
        assert!(
            repaired
                .model_text
                .lines()
                .any(|line| line == "note=include_array_coerced,cwd_ignored")
        );
    }

    #[test]
    fn read_and_list_are_bounded_and_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("b.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(directory.path().join("a.txt"), "a").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let listed = run_tool(&workspace, &state, "tree", r#"{"path":".","depth":1}"#);
        assert_eq!(
            listed.model_text,
            "tree . depth=1 entries=2/2 files=2 dirs=0\na.txt 1  b.txt 14\n"
        );
        // The pre-v0.1.0 name is not a tool.
        let legacy = run_tool(&workspace, &state, "list_dir", r#"{"path":"."}"#);
        assert!(legacy.is_error, "{}", legacy.model_text);
        let tree = run_tool(&workspace, &state, "tree", r#"{}"#);
        assert_eq!(
            tree.model_text,
            "tree . depth=2 entries=2/2 files=2 dirs=0\na.txt 1  b.txt 14\n"
        );

        let read = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"b.txt","offset":2,"limit":1}"#,
        );
        let hash = content_hash(b"one\ntwo\nthree\n");
        assert_eq!(
            read.model_text,
            format!(
                "read b.txt L2/3 h:{}\n2\ttwo\n…[qq: 1 more lines; continue from offset=3]…\n",
                &hash[..12]
            )
        );
        let update = read.file_states.into_iter().next().unwrap();
        assert_eq!(update.path, "b.txt");
        assert_eq!(update.hash, hash);
        assert_eq!(state.recorded("b.txt"), Some(update.hash));
    }

    #[test]
    fn read_search_identity_retains_read_lines_before_clipping_and_respects_scan_cap() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let path = directory.path().join("large.txt");
        let mut bytes = vec![b'x'; 5 * 1024 * 1024];
        fs::write(&path, &bytes).unwrap();
        let read = || {
            run_tool(
                &workspace,
                &FileState::default(),
                "read_file",
                r#"{"path":"large.txt","limit":1}"#,
            )
        };
        let first = read();
        bytes[100_000] = b'y';
        fs::write(&path, &bytes).unwrap();
        let second = read();
        assert!(!first.is_error && !second.is_error);
        assert_eq!(first.model_text, second.model_text);
        assert_ne!(first.result_digest, second.result_digest);
        bytes[4 * 1024 * 1024 + 100] = b'z';
        fs::write(&path, &bytes).unwrap();
        let outside_scan = read();
        assert_eq!(
            second.result_digest, outside_scan.result_digest,
            "unscanned bytes must not be read for identity"
        );
    }

    #[test]
    fn read_search_identity_retains_search_rows_before_clipping() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let path = directory.path().join("input.txt");
        let mut bytes = format!("needle {}\n", "x".repeat(10_000)).into_bytes();
        fs::write(&path, &bytes).unwrap();
        let search = || {
            run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"needle","path":"input.txt"}"#,
            )
        };
        let first = search();
        bytes[8_000] = b'y';
        fs::write(&path, &bytes).unwrap();
        let second = search();
        assert!(!first.is_error && !second.is_error);
        assert_eq!(first.model_text, second.model_text);
        assert_ne!(first.result_digest, second.result_digest);
    }

    #[test]
    fn read_search_identity_retains_clipped_search_context() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let path = directory.path().join("input.txt");
        let mut bytes =
            format!("{}\nneedle\n{}\n", "x".repeat(10_000), "z".repeat(10_000)).into_bytes();
        fs::write(&path, &bytes).unwrap();
        let search = || {
            run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"needle","path":"input.txt","context":1}"#,
            )
        };
        let first = search();
        bytes[8_000] = b'y';
        fs::write(&path, &bytes).unwrap();
        let second = search();
        assert_eq!(first.model_text, second.model_text);
        assert_ne!(first.result_digest, second.result_digest);
        bytes[18_000] = b'y';
        fs::write(&path, &bytes).unwrap();
        let third = search();
        assert_eq!(second.model_text, third.model_text);
        assert_ne!(second.result_digest, third.result_digest);
    }

    #[test]
    fn read_search_identity_ignores_unmatched_file_renames() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        fs::write(directory.path().join("a.txt"), "NOTHING\n").unwrap();
        let search = || {
            run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"needle"}"#,
            )
        };
        let first = search();
        fs::rename(
            directory.path().join("a.txt"),
            directory.path().join("b.txt"),
        )
        .unwrap();
        let second = search();
        assert_eq!(first.model_text, second.model_text);
        assert_eq!(first.result_digest, second.result_digest);
    }

    #[test]
    fn read_search_identity_excludes_advisory_only_search_changes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let path = directory.path().join("input.txt");
        fs::write(&path, "NEEDLE\n").unwrap();
        let search = || {
            run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"needle","case":"sensitive","path":"input.txt"}"#,
            )
        };
        let first = search();
        assert!(first.model_text.contains("hint=case_insensitive_matches=1"));
        fs::write(&path, "NOTHING\n").unwrap();
        let second = search();
        assert!(!second.model_text.contains("hint="));
        assert_eq!(
            first.result_digest, second.result_digest,
            "primary search result is unchanged"
        );
    }

    #[test]
    fn ab3_default_reads_fill_the_budget_and_explicit_limits_resume() {
        let directory = tempfile::tempdir().unwrap();
        for (path, lines, content) in [
            ("small", 300, "x\n"),
            ("short", 3_000, "x\n"),
            ("large", 3_000, "012345678901234567890123456789\n"),
        ] {
            fs::write(directory.path().join(path), content.repeat(lines)).unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        for (path, expected) in [("small", 300), ("short", 3_000)] {
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "read_file",
                &format!(r#"{{"path":"{path}"}}"#),
            );
            assert!(!result.is_error, "{}", result.model_text);
            assert_eq!(result.model_text.lines().skip(1).count(), expected);
            assert!(!result.model_text.contains(MARKER_PREFIX));
        }
        let large = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"large"}"#,
        );
        let rows = large
            .model_text
            .lines()
            .skip(1)
            .filter(|row| !row.starts_with(MARKER_PREFIX))
            .count();
        assert!(rows > 200 && rows < 3_000, "{rows}");
        assert!(
            large
                .model_text
                .contains(&format!("continue from offset={}", rows + 1))
        );
        let limited = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"small","limit":20}"#,
        );
        assert!(
            limited
                .model_text
                .ends_with("…[qq: 280 more lines; continue from offset=21]…\n"),
            "{}",
            limited.model_text
        );
        let ranged = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"small","ranges":["1-20"]}"#,
        );
        assert!(!ranged.model_text.contains(MARKER_PREFIX));
    }

    #[test]
    fn ab3_long_path_headers_preserve_contiguous_model_visible_rows() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = std::iter::repeat_n("a".repeat(100), 6)
            .collect::<Vec<_>>()
            .join("/");
        std::fs::create_dir_all(directory.path().join(&prefix)).unwrap();
        let path = format!("{prefix}/notes.txt");
        std::fs::write(
            directory.path().join(&path),
            ("x".repeat(80) + "\n").repeat(2_000),
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let output = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            &serde_json::json!({"path":path}).to_string(),
        );
        assert!(!output.is_error, "{}", output.model_text);
        let rows: Vec<usize> = output
            .model_text
            .lines()
            .filter_map(|line| {
                line.split_once('\t')
                    .and_then(|(number, _)| number.trim().parse().ok())
            })
            .collect();
        assert!(rows.len() > 200);
        assert!(
            rows.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "visible rows have a gap"
        );
        assert!(!output.model_text.contains("not stored"));
    }

    #[test]
    fn ab3_full_row_is_not_clipped_again_after_adding_the_gutter() {
        let directory = tempfile::tempdir().unwrap();
        let content = "x".repeat(output::MAX_LINE_BYTES);
        fs::write(directory.path().join("row.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"row.txt"}"#,
        );
        assert!(!result.is_error);
        let row = result
            .model_text
            .lines()
            .find_map(|line| line.split_once('\t'))
            .unwrap();
        assert_eq!(row.1, content);
    }

    #[test]
    fn ab3_secret_masking_preserves_contiguous_default_windows() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = std::iter::repeat_n("a".repeat(100), 6)
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(directory.path().join(&prefix)).unwrap();
        let long_path = format!("{prefix}/{}notes.txt", "SECRET=abcdefgh ".repeat(8));
        for path in ["notes.txt", long_path.as_str()] {
            fs::write(
                directory.path().join(path),
                "SECRET=abcdefgh\n".repeat(2_500),
            )
            .unwrap();
            let workspace = Workspace::open(directory.path()).unwrap();
            let state = FileState::default();
            let mut offset = 1;
            while offset <= 2_500 {
                let arguments = serde_json::json!({"path":path,"offset":offset,
                    "ranges":"[]","cwd":".","include_ignored":true})
                .to_string();
                let result = run_tool(&workspace, &state, "read_file", &arguments);
                assert!(!result.is_error, "{}", result.model_text);
                assert!(result.spill.is_none(), "read window must survive masking");
                assert!(!result.model_text.contains("SECRET=abcdefgh"));
                let rows = result
                    .model_text
                    .lines()
                    .filter_map(|line| {
                        line.split_once('\t')
                            .and_then(|(number, _)| number.trim().parse::<usize>().ok())
                    })
                    .collect::<Vec<_>>();
                assert!(!rows.is_empty());
                assert_eq!(rows, (offset..offset + rows.len()).collect::<Vec<_>>());
                let last = *rows.last().unwrap();
                if last == 2_500 {
                    break;
                }
                let next = result
                    .model_text
                    .split("offset=")
                    .nth(1)
                    .unwrap()
                    .split(|ch: char| !ch.is_ascii_digit())
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                assert_eq!(next, last + 1);
                offset = next;
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn ab3_native_zero_row_window_returns_a_budget_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut dir =
            cap_std::fs::Dir::open_ambient_dir(directory.path(), cap_std::ambient_authority())
                .unwrap();
        let mut components = Vec::new();
        for _ in 0..110 {
            let name = "界".repeat(95);
            dir.create_dir(&name).unwrap();
            dir = dir.open_dir(&name).unwrap();
            components.push(name);
        }
        dir.write("f.txt", format!("{}\n", "x".repeat(2_000)))
            .unwrap();
        drop(dir);
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let args = serde_json::json!({"path":format!("{}/f.txt",components.join("/"))}).to_string();
        let result = run_tool(&workspace, &state, "read_file", &args);
        assert!(result.is_error, "{}", result.model_text);
        assert!(result.model_text.contains("output_limit:"));
        assert!(!result.model_text.contains("offset=0"));
    }

    #[cfg(windows)]
    #[test]
    fn ab3_native_unicode_header_preserves_repaired_read_windows() {
        let directory = tempfile::tempdir().unwrap();
        let mut dir =
            cap_std::fs::Dir::open_ambient_dir(directory.path(), cap_std::ambient_authority())
                .unwrap();
        let mut components = Vec::new();
        for index in 0..110 {
            let name = if index == 0 {
                format!("SECRET={}", "界".repeat(100))
            } else {
                "界".repeat(100)
            };
            dir.create_dir(&name).unwrap();
            dir = dir.open_dir(&name).unwrap();
            components.push(name);
        }
        dir.write("f.txt", "\n".repeat(5_000)).unwrap();
        drop(dir);
        let path = format!("{}/f.txt", components.join("/"));
        assert!(path.len() > read::READ_BOUNDS.max_bytes);
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let mut offset = 1;
        while offset <= 5_000 {
            let args = serde_json::json!({"path":path,"offset":offset,"cwd":"."}).to_string();
            let result = run_tool(&workspace, &state, "read_file", &args);
            assert!(!result.is_error, "{}", result.model_text);
            assert!(
                result.spill.is_none(),
                "native read must preserve its contiguous window"
            );
            let rows = result
                .model_text
                .lines()
                .filter_map(|line| {
                    line.split_once('\t')
                        .and_then(|(n, _)| n.trim().parse::<usize>().ok())
                })
                .collect::<Vec<_>>();
            assert!(!rows.is_empty());
            assert_eq!(rows, (offset..offset + rows.len()).collect::<Vec<_>>());
            offset += rows.len();
            if offset <= 5_000 {
                assert!(
                    result
                        .model_text
                        .contains(&format!("continue from offset={offset}"))
                );
            }
        }
        assert_eq!(offset, 5_001);
    }

    #[test]
    fn ab3_continuation_beyond_start_offset_bound_is_terminal() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("many.txt"), "\n".repeat(105_000)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let output = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"many.txt","offset":99999}"#,
        );
        assert!(!output.is_error, "{}", output.model_text);
        assert!(
            output.model_text.contains("start-offset limit 100000"),
            "{:?}",
            output.model_text.lines().last()
        );
        assert!(!output.model_text.contains("continue from offset="));
    }

    #[test]
    fn ab3_rendered_line_bound_continues_contiguously_without_spill() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("empty"), "\n".repeat(4_000)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let mut offset = 1;
        let mut calls = 0;
        while offset <= 4_000 {
            calls += 1;
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "read_file",
                &format!(r#"{{"path":"empty","offset":{offset}}}"#),
            );
            assert!(!result.is_error, "{}", result.model_text);
            assert!(result.model_text.lines().count() <= 4_000);
            assert!(!result.model_text.contains("not stored"));
            let rows: Vec<_> = result
                .model_text
                .lines()
                .skip(1)
                .filter(|row| !row.starts_with(MARKER_PREFIX))
                .collect();
            assert!(!rows.is_empty());
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(row.trim().parse::<usize>().unwrap(), offset + index);
            }
            offset += rows.len();
            if offset <= 4_000 {
                assert!(
                    result
                        .model_text
                        .contains(&format!("continue from offset={offset}"))
                );
                assert!(rows.len() > 200);
            } else {
                assert!(!result.model_text.contains(MARKER_PREFIX));
            }
        }
        assert_eq!(offset, 4_001);
        assert_eq!(calls, 2);
    }

    #[test]
    fn ab3_scan_cap_continuation_does_not_claim_an_exact_remainder() {
        let directory = tempfile::tempdir().unwrap();
        let line = format!("{}\n", "x".repeat(1_023));
        fs::write(directory.path().join("capped"), line.repeat(4_097)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        for arguments in [r#"{"path":"capped"}"#, r#"{"path":"capped","limit":2}"#] {
            let result = run_tool(&workspace, &FileState::default(), "read_file", arguments);
            assert!(!result.is_error, "{}", result.model_text);
            let marker = result.model_text.lines().last().unwrap();
            assert!(marker.contains("count unknown"), "{marker}");
            assert!(marker.contains("4 MiB scan cap"), "{marker}");
            assert!(marker.contains("continue from offset="), "{marker}");
            assert!(!marker.contains("4094 more lines"));
        }
    }

    #[test]
    fn ab3_scan_cap_offsets_remain_readable_until_a_terminal_marker() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("capped"),
            format!("{}\n", "x".repeat(99)).repeat(52_429),
        )
        .unwrap();
        fs::write(directory.path().join("single"), "x".repeat(5 * 1024 * 1024)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        for path in ["capped", "single"] {
            let mut offset = 1;
            for call in 0..256 {
                let arguments = format!(r#"{{"path":"{path}","offset":{offset}}}"#);
                let result = run_tool(&workspace, &FileState::default(), "read_file", &arguments);
                assert!(!result.is_error, "offset={offset}: {}", result.model_text);
                let marker = result.model_text.lines().last().unwrap();
                if let Some((_, next)) = marker.split_once("continue from offset=") {
                    let next = next.split(']').next().unwrap().parse::<usize>().unwrap();
                    assert!(next > offset);
                    offset = next;
                } else {
                    assert!(
                        marker.contains("file continues past the 4 MiB scan cap"),
                        "{marker}"
                    );
                    break;
                }
                assert!(call < 255, "scan prefix did not terminate");
            }
        }
    }

    #[test]
    fn ab3_six_default_read_results_characterize_turn_budget_pressure() {
        use super::output::{ResultRecall, TurnOutputBudget};
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("batch"),
            "012345678901234567890123456789\n".repeat(1_000),
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let mut offset = 1;
        let mut reads = 0;
        let mut cuts = 0;
        let mut bytes = 0;
        while offset <= 1_000 {
            let mut budget = TurnOutputBudget::new();
            let mut shown = 0;
            for _ in 0..6 {
                let result = run_tool(
                    &workspace,
                    &FileState::default(),
                    "read_file",
                    &format!(r#"{{"path":"batch","offset":{offset}}}"#),
                );
                assert!(!result.is_error);
                shown = result
                    .model_text
                    .lines()
                    .skip(1)
                    .filter(|row| !row.starts_with(MARKER_PREFIX))
                    .count();
                bytes += result.model_text.len();
                let mut projected = result.model_text;
                cuts += usize::from(budget.admit("read_file", &mut projected, ResultRecall::None));
                reads += 1;
            }
            assert!(shown > 0);
            offset += shown;
        }
        eprintln!(
            "AB3 six-result batches: reads={reads} turn_budget_cuts={cuts} mean_read_bytes={}",
            bytes / reads
        );
        assert!(reads <= 30);
        assert!(cuts == 0 || reads < 30, "cuts must buy fewer read calls");
    }

    #[test]
    fn read_file_stops_at_its_byte_budget_and_names_the_resume_offset() {
        let directory = tempfile::tempdir().unwrap();
        // 200 lines of 1 KiB: within the line limit, over the 32 KiB default.
        let line = format!("{}\n", "x".repeat(1_023));
        fs::write(directory.path().join("large.txt"), line.repeat(200)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"large.txt"}"#,
        );

        assert!(!result.is_error);
        assert!(result.model_text.len() <= 32 * 1024);
        let header = result.model_text.lines().next().unwrap();
        assert!(header.starts_with("read large.txt L1-"), "{header}");
        assert!(header.contains("/200 h:"), "{header}");
        assert!(header.ends_with(" truncated=bytes"), "{header}");
        // Whole lines only, in order, then one marker with the next offset.
        let shown: Vec<&str> = result
            .model_text
            .lines()
            .skip(1)
            .filter(|line| !line.starts_with(MARKER_PREFIX))
            .collect();
        assert!(shown.len() > 20 && shown.len() < 40, "{}", shown.len());
        for (index, row) in shown.iter().enumerate() {
            assert_eq!(*row, format!("{:>3}\t{}", index + 1, "x".repeat(1_023)));
        }
        assert_eq!(result.model_text.matches(MARKER_PREFIX).count(), 1);
        assert!(
            result
                .model_text
                .contains(&format!("continue from offset={}", shown.len() + 1)),
            "{}",
            result.model_text.lines().last().unwrap()
        );
        // The whole file still hashes: the read is complete for the guard.
        assert!(!result.file_states.is_empty());
    }

    #[test]
    fn t20_oversized_search_and_ranges_keep_lossless_corrections() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), "x\n".repeat(100)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for (name, arguments, code) in [
            (
                "search",
                r#"{"query":"x","max_per_file":80}"#,
                "invalid_max_per_file:",
            ),
            (
                "read_file",
                r#"{"path":"a","ranges":["1","2","3","4","5","6","7","8","9"]}"#,
                "invalid_ranges:",
            ),
        ] {
            let result = run_tool(&workspace, &state, name, arguments);
            assert!(result.is_error, "{}", result.model_text);
            assert!(result.model_text.starts_with(code), "{}", result.model_text);
            assert_eq!(
                qq_protocol::ToolErrorKind::of(name, &result.model_text),
                qq_protocol::ToolErrorKind::Correction
            );
        }
    }

    #[test]
    fn read_file_ranges_merge_align_and_report_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let content: String = (1..=120).map(|n| format!("line {n}\n")).collect();
        fs::write(directory.path().join("n.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let hash = content_hash(content.as_bytes());

        let read = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"n.txt","ranges":["100-101","3-4","2","119-"]}"#,
        );
        assert!(!read.is_error, "{}", read.model_text);
        assert_eq!(
            read.model_text,
            format!(
                "read n.txt L2-4,100-101,119-120/120 h:{}\n  2\tline 2\n  3\tline 3\n  4\tline 4\n--\n100\tline 100\n101\tline 101\n--\n119\tline 119\n120\tline 120\n",
                &hash[..12]
            )
        );

        // A range past the end is clipped when another range is in bounds…
        let tail = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"n.txt","ranges":["118-500"]}"#,
        );
        assert!(tail.model_text.starts_with("read n.txt L118-120/120 h:"));
        // …and a typed failure when nothing is.
        let past = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"n.txt","offset":121}"#,
        );
        assert!(past.is_error);
        assert_eq!(
            past.model_text,
            "range_out_of_bounds: n.txt has 120 lines (last_line=120)"
        );
        for (arguments, code) in [
            (r#"{"path":"n.txt","ranges":["5-3"]}"#, "invalid_ranges"),
            (r#"{"path":"n.txt","ranges":["a"]}"#, "invalid_ranges"),
            (r#"{"path":"n.txt","ranges":["1-3000"]}"#, "invalid_ranges"),
            (r#"{"path":"n.txt","offset":0}"#, "invalid_offset"),
            (r#"{"path":"n.txt","limit":0}"#, "invalid_limit"),
            (
                r#"{"path":"n.txt","if_changed_since":"abc"}"#,
                "invalid_if_changed_since",
            ),
            (r#"{"path":"."}"#, "not_a_file"),
            (r#"{"path":"missing.txt"}"#, "path_not_found"),
        ] {
            let failed = run_tool(&workspace, &state, "read_file", arguments);
            assert!(failed.is_error, "{arguments}");
            assert!(
                failed.model_text.starts_with(code),
                "{arguments}: {}",
                failed.model_text
            );
        }
        // Failures record nothing.
        assert_eq!(state.recorded("n.txt"), Some(hash));
    }

    /// D9: models that fill every optional field send `offset`/`limit`
    /// beside `ranges`, or a comma for the range separator. `ranges` wins
    /// and the header says what was ignored.
    #[test]
    fn read_file_ranges_win_over_default_shaped_offset_and_limit() {
        let directory = tempfile::tempdir().unwrap();
        let content: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        fs::write(directory.path().join("n.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let short = &content_hash(content.as_bytes())[..12];

        let read = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"n.txt","ranges":["3-4"],"offset":3,"limit":200}"#,
        );
        assert!(!read.is_error, "{}", read.model_text);
        assert_eq!(
            read.model_text,
            format!("read n.txt L3-4/20 h:{short} note=offset_ignored\n3\tline 3\n4\tline 4\n")
        );
        assert_eq!(
            state.recorded("n.txt").as_deref().map(|h| &h[..12]),
            Some(short)
        );

        let comma = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"n.txt","ranges":["5,6"]}"#,
        );
        assert_eq!(
            comma.model_text,
            format!("read n.txt L5-6/20 h:{short}\n5\tline 5\n6\tline 6\n")
        );
        // Without ranges, offset/limit are the request and stay validated.
        let plain = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"n.txt","offset":19}"#,
        );
        assert!(!plain.model_text.contains("note="), "{}", plain.model_text);
    }

    #[test]
    fn read_file_reports_ignored_offsets_in_every_successful_mode() {
        let directory = tempfile::tempdir().unwrap();
        let content = "fn a() {}\n";
        fs::write(directory.path().join("a.rs"), content).unwrap();
        fs::write(directory.path().join("pic.png"), b"\x89PNG\r\n\x1a\n\x00").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let short = &content_hash(content.as_bytes())[..12];
        for arguments in [
            format!(
                r#"{{"path":"a.rs","ranges":["1"],"offset":0,"limit":0,"if_changed_since":"h:{short}"}}"#
            ),
            r#"{"path":"a.rs","ranges":["1"],"offset":0,"mode":"info"}"#.to_owned(),
            r#"{"path":"a.rs","ranges":["1"],"limit":0,"mode":"outline"}"#.to_owned(),
            r#"{"path":"pic.png","ranges":["1"],"offset":0}"#.to_owned(),
        ] {
            let read = run_tool(&workspace, &state, "read_file", &arguments);
            assert!(!read.is_error, "{}", read.model_text);
            assert!(
                read.model_text
                    .lines()
                    .next()
                    .unwrap()
                    .ends_with(" note=offset_ignored"),
                "{}",
                read.model_text
            );
        }
    }

    #[test]
    fn a_spilled_read_stores_the_ignored_offset_note() {
        let directory = tempfile::tempdir().unwrap();
        // 400 outline rows of ~190 bytes overrun the 32 KiB read bound.
        let source: String = (0..400)
            .map(|n| format!("fn f{n:03}_{}() {{}}\n", "x".repeat(180)))
            .collect();
        fs::write(directory.path().join("big.rs"), source).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let read = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"big.rs","mode":"outline","ranges":["1"],"offset":1}"#,
        );
        assert!(!read.is_error, "{}", read.model_text);
        let spill = read.spill.as_ref().expect("the outline overran the bound");
        let header = spill.text.lines().next().unwrap();
        assert!(header.ends_with(" note=offset_ignored"), "{header}");
        assert!(
            read.model_text
                .lines()
                .next()
                .unwrap()
                .ends_with(" note=offset_ignored")
        );
    }

    #[test]
    fn read_file_if_changed_since_skips_unchanged_content_but_still_records() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.rs"), "fn a() {}\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let hash = content_hash(b"fn a() {}\n");
        let short = &hash[..12];

        let unchanged = run_tool(
            &workspace,
            &state,
            "read_file",
            &format!(r#"{{"path":"a.rs","if_changed_since":"h:{short}"}}"#),
        );
        assert!(!unchanged.is_error);
        assert_eq!(
            unchanged.model_text,
            format!("read a.rs unchanged h:{short} lines=1\n")
        );
        assert_eq!(unchanged.file_states[0].hash, hash);
        assert_eq!(state.recorded("a.rs"), Some(hash.clone()));

        fs::write(directory.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let changed = run_tool(
            &workspace,
            &state,
            "read_file",
            &format!(r#"{{"path":"a.rs","if_changed_since":"h:{short}"}}"#),
        );
        assert!(!changed.is_error);
        assert!(
            changed.model_text.starts_with("read a.rs L1-2/2 h:"),
            "{}",
            changed.model_text
        );
        assert!(!changed.model_text.contains(short));
        assert!(
            changed
                .model_text
                .ends_with("\n1\tfn a() {}\n2\tfn b() {}\n")
        );
        assert_eq!(
            state.recorded("a.rs"),
            Some(content_hash(b"fn a() {}\nfn b() {}\n"))
        );
    }

    #[test]
    fn read_file_outline_and_info_modes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("lib.rs"),
            "pub struct Foo;\n\nimpl Foo {\n    pub fn new() -> Self {\n        Foo\n    }\n}\n\nfn helper() {}\n",
        )
        .unwrap();
        fs::write(directory.path().join("win.txt"), "a\r\nb\r\n").unwrap();
        fs::write(directory.path().join("Makefile"), "all:\n\tmake\n").unwrap();
        fs::write(directory.path().join("blob.bin"), b"\x00\xff\xfebinary").unwrap();
        fs::write(directory.path().join("pic.png"), b"\x89PNG\r\n\x1a\n\x00").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let outline = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"lib.rs","mode":"outline"}"#,
        );
        assert!(!outline.is_error, "{}", outline.model_text);
        let mut lines = outline.model_text.lines();
        let header = lines.next().unwrap();
        assert!(
            header.starts_with("read lib.rs outline items=4/4 lines=9 h:"),
            "{header}"
        );
        assert_eq!(
            lines.collect::<Vec<_>>(),
            [
                "L1 struct Foo",
                "L3 impl Foo",
                "L4   fn new",
                "L9 fn helper"
            ]
        );
        // Outline records the hash too: the model saw the file's shape.
        assert!(!outline.file_states.is_empty());

        let unsupported = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"Makefile","mode":"outline"}"#,
        );
        assert!(unsupported.is_error);
        assert!(unsupported.model_text.starts_with("outline_unsupported"));

        let info = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"win.txt","mode":"info"}"#,
        );
        assert!(!info.is_error);
        assert!(
            info.model_text
                .starts_with("read win.txt info size=6 lines=2 h:"),
            "{}",
            info.model_text
        );
        assert!(
            info.model_text.contains(" utf8=true eol=crlf perms="),
            "{}",
            info.model_text
        );
        assert!(
            info.model_text.ends_with(" binary=false\n"),
            "{}",
            info.model_text
        );
        // CRLF content renders without the CR; the hash is of the bytes.
        let win = run_tool(&workspace, &state, "read_file", r#"{"path":"win.txt"}"#);
        assert!(
            win.model_text.ends_with("\n1\ta\n2\tb\n"),
            "{}",
            win.model_text
        );

        let binary = run_tool(&workspace, &state, "read_file", r#"{"path":"blob.bin"}"#);
        assert!(binary.is_error);
        assert!(
            binary.model_text.starts_with("not_text"),
            "{}",
            binary.model_text
        );
        let binary_info = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"blob.bin","mode":"info"}"#,
        );
        assert!(!binary_info.is_error);
        assert!(
            binary_info.model_text.contains(" utf8=false eol=none "),
            "{}",
            binary_info.model_text
        );
        assert!(binary_info.model_text.ends_with(" binary=true\n"));

        let image = run_tool(&workspace, &state, "read_file", r#"{"path":"pic.png"}"#);
        assert!(!image.is_error);
        assert!(
            image
                .model_text
                .contains(" mime=image/png hint=image_unsupported_by_model"),
            "{}",
            image.model_text
        );
    }

    #[test]
    fn control_dense_results_are_bounded_by_their_json_escaped_size() {
        let directory = tempfile::tempdir().unwrap();
        // Raw size stays under the result cap, but every ESC escapes 6:1 so the
        // escaped size would far exceed the persisted-event budget.
        fs::write(
            directory.path().join("ansi.log"),
            format!("{}\n", "\u{1b}".repeat(1_000)).repeat(120),
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"ansi.log"}"#,
        );

        assert!(!result.is_error);
        assert!(result.model_text.contains(MARKER_PREFIX));
        assert!(escaped_len(&result.model_text) <= MAX_MODEL_TEXT_BYTES);
        assert!(
            serde_json::to_string(&result.model_text).unwrap().len() <= MAX_MODEL_TEXT_BYTES + 2
        );
    }

    #[test]
    fn read_file_accepts_a_multibyte_char_split_by_the_scan_cap() {
        let directory = tempfile::tempdir().unwrap();
        let cap = usize::try_from(MAX_READ_SCAN_BYTES).unwrap();
        let mut content = Vec::with_capacity(cap + 2);
        content.resize(cap - 4, b'x');
        content.push(b'\n');
        content.extend_from_slice("ab\u{e9}".as_bytes());
        assert_eq!(content.len(), cap + 1);
        fs::write(directory.path().join("split.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"split.txt","offset":2,"limit":1}"#,
        );

        assert!(!result.is_error, "unexpected error: {}", result.model_text);
        assert!(
            result
                .model_text
                .starts_with("read split.txt L2/2 h:- scanned=4194304\n2\tab\n"),
            "{}",
            result.model_text
        );
        assert!(
            result
                .model_text
                .contains("file continues past the 4 MiB scan cap"),
            "{}",
            result.model_text
        );
        // Nothing is recorded for a file the guard could not hash whole.
        assert!(result.file_states.is_empty());
    }

    #[test]
    fn read_file_does_not_mark_an_exactly_cap_sized_file_truncated() {
        let directory = tempfile::tempdir().unwrap();
        let cap = usize::try_from(MAX_READ_SCAN_BYTES).unwrap();
        let mut content = Vec::with_capacity(cap);
        content.resize(cap - 2, b'x');
        content.push(b'\n');
        content.push(b'y');
        assert_eq!(content.len(), cap);
        fs::write(directory.path().join("exact.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"exact.txt","offset":2,"limit":1}"#,
        );

        assert!(!result.is_error);
        assert!(
            result.model_text.starts_with("read exact.txt L2/2 h:"),
            "{}",
            result.model_text
        );
        assert!(result.model_text.ends_with("\n2\ty\n"));
        assert!(!result.model_text.contains(MARKER_PREFIX));
        assert!(!result.file_states.is_empty());
    }

    #[test]
    fn rejects_parent_and_symlink_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "hidden").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let state = FileState::default();
        let parent = run_tool(&workspace, &state, "read_file", r#"{"path":"../secret"}"#);
        assert!(parent.is_error);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
            let symlink = run_tool(
                &workspace,
                &state,
                "read_file",
                r#"{"path":"outside/secret"}"#,
            );
            assert!(symlink.is_error);
        }
    }

    #[test]
    fn search_matches_names_and_content_without_following_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/needle.rs"), "hay\nneedle here\n").unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("needle.txt"), "needle outside").unwrap();
            std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
            // The symlink stays open across the test so its target exists.
            std::mem::forget(outside);
        }
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );
        assert!(!result.is_error, "{}", result.model_text);
        assert_eq!(
            result.model_text,
            "search \"needle\" mode=content matches=1/1 files=1 scanned=1\nsrc/needle.rs\nL2: needle here\n"
        );
        let names = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle","mode":"names"}"#,
        );
        assert_eq!(
            names.model_text,
            "search \"needle\" mode=names matches=1/1 files=1 scanned=1\nsrc/needle.rs\n"
        );
    }

    #[test]
    fn search_skips_oversized_and_binary_files_and_counts_them() {
        let directory = tempfile::tempdir().unwrap();
        let oversized = fs::File::create(directory.path().join("oversized-needle.txt")).unwrap();
        oversized.set_len(walk::MAX_FILE_SCAN_BYTES + 1).unwrap();
        fs::write(directory.path().join("blob.bin"), b"needle\0needle").unwrap();
        fs::write(directory.path().join("text.txt"), "needle\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );
        assert!(!result.is_error);
        assert!(
            result.model_text.starts_with(
                "search \"needle\" mode=content matches=1/1 files=1 scanned=2 skipped=2\n"
            ),
            "{}",
            result.model_text
        );
        assert!(!result.model_text.contains("blob.bin"));
    }

    #[cfg(unix)]
    #[test]
    fn ab2_names_discovery_reports_safe_instruction_links_and_blocks_bad_links() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::create_dir(directory.path().join("shared")).unwrap();
        fs::write(directory.path().join("shared/rules"), "instructions\n").unwrap();
        symlink("../shared/rules", directory.path().join("src/AGENTS.md")).unwrap();
        fs::write(directory.path().join(".gitignore"), "AGENTS.md\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let arguments = r#"{"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true}"#;
        let result = run_tool(&workspace, &FileState::default(), "search", arguments);
        assert!(!result.is_error, "{}", result.model_text);
        assert!(
            result.model_text.contains("src/AGENTS.md"),
            "{}",
            result.model_text
        );
        let content = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"instructions"}"#,
        );
        assert!(!content.model_text.contains("src/AGENTS.md"));
        for target in ["../../outside", "../missing", "../shared"] {
            fs::remove_file(directory.path().join("src/AGENTS.md")).unwrap();
            symlink(target, directory.path().join("src/AGENTS.md")).unwrap();
            let result = run_tool(&workspace, &FileState::default(), "search", arguments);
            assert!(result.is_error, "target={target}: {}", result.model_text);
            assert!(
                result.model_text.contains("src/AGENTS.md"),
                "{}",
                result.model_text
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn ab2_instruction_link_pages_resume_without_following_directory_links() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("rules"), "instructions\n").unwrap();
        for name in ["a", "b", "c"] {
            fs::create_dir(directory.path().join(name)).unwrap();
            symlink("../rules", directory.path().join(name).join("AGENTS.md")).unwrap();
        }
        symlink("a", directory.path().join("directory-link")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let mut cursor = None;
        for (page, path) in ["a/AGENTS.md", "b/AGENTS.md", "c/AGENTS.md"]
            .iter()
            .enumerate()
        {
            let mut arguments = serde_json::json!({"query":"AGENTS.md","mode":"names","limit":1});
            if let Some(cursor) = cursor.take() {
                arguments["cursor"] = serde_json::Value::String(cursor);
            }
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "search",
                &arguments.to_string(),
            );
            assert!(!result.is_error, "{}", result.model_text);
            let shown = result
                .model_text
                .lines()
                .filter(|line| line.ends_with("/AGENTS.md"))
                .collect::<Vec<_>>();
            assert_eq!(shown, [*path]);
            assert!(!result.model_text.contains("directory-link/"));
            cursor = result
                .model_text
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .find_map(|field| field.strip_prefix("next=").map(str::to_owned));
            assert_eq!(cursor.is_some(), page < 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn ab2_names_scan_budget_advances_past_nonmatching_links() {
        search::with_scan_entries(128, || {
            use std::os::unix::fs::symlink;
            let directory = tempfile::tempdir().unwrap();
            for index in 0..128 {
                symlink("missing", directory.path().join(format!("s{index:05}"))).unwrap();
            }
            fs::create_dir(directory.path().join("z")).unwrap();
            fs::write(directory.path().join("z/AGENTS.md"), "rules\n").unwrap();
            let workspace = Workspace::open(directory.path()).unwrap();
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"AGENTS.md","mode":"names"}"#,
            );
            assert!(!result.is_error, "{}", result.model_text);
            let header = result.model_text.lines().next().unwrap();
            assert!(header.contains("partial=scan"), "{header}");
            let cursor = header
                .split_whitespace()
                .find_map(|field| field.strip_prefix("next="))
                .expect("scan prefix must produce a resumable cursor");
            let arguments =
                serde_json::json!({"query":"AGENTS.md","mode":"names","cursor":cursor}).to_string();
            let next = run_tool(&workspace, &FileState::default(), "search", &arguments);
            assert!(!next.is_error, "{}", next.model_text);
            assert!(
                next.model_text.contains("z/AGENTS.md"),
                "{}",
                next.model_text
            );
            assert!(!next.model_text.contains("partial=scan"));
        });
    }

    #[test]
    fn ab2_names_scan_budget_advances_past_empty_directories() {
        search::with_scan_entries(128, || {
            let directory = tempfile::tempdir().unwrap();
            for index in 0..128 {
                fs::create_dir(directory.path().join(format!("d{index:05}"))).unwrap();
            }
            fs::create_dir(directory.path().join("z")).unwrap();
            fs::write(directory.path().join("z/AGENTS.md"), "rules\n").unwrap();
            let workspace = Workspace::open(directory.path()).unwrap();
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"AGENTS.md","mode":"names"}"#,
            );
            assert!(!result.is_error, "{}", result.model_text);
            let cursor = result
                .model_text
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .find_map(|field| field.strip_prefix("next="))
                .expect("completed directories must provide a cursor");
            let arguments =
                serde_json::json!({"query":"AGENTS.md","mode":"names","cursor":cursor}).to_string();
            let next = run_tool(&workspace, &FileState::default(), "search", &arguments);
            assert!(!next.is_error, "{}", next.model_text);
            assert!(
                next.model_text.contains("z/AGENTS.md"),
                "{}",
                next.model_text
            );
            assert!(!next.model_text.contains("partial="), "{}", next.model_text);
        });
    }

    #[test]
    fn ab2_cursor_path_boundary_round_trips_without_header_clipping() {
        for path in ["a".repeat(4_096), "界".repeat(1_365) + "/"] {
            let cursor = search::Cursor {
                path,
                line: u32::MAX,
            };
            assert_eq!(search::Cursor::decode(&cursor.encode()), Some(cursor));
        }
        let cursor = search::Cursor {
            path: "a".repeat(4_097),
            line: 0,
        };
        assert!(search::Cursor::decode(&cursor.encode()).is_none());

        let directory = tempfile::tempdir().unwrap();
        let root = (0..8)
            .map(|_| "a".repeat(240))
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(directory.path().join(&root)).unwrap();
        for index in 0..10 {
            fs::write(
                directory
                    .path()
                    .join(&root)
                    .join(format!("{index:02}{}", "x".repeat(200))),
                "",
            )
            .unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        let mut cursor = None::<String>;
        let mut found = std::collections::BTreeSet::new();
        for _ in 0..10 {
            let args =
                serde_json::json!({"query":"x", "mode":"names", "cursor":cursor}).to_string();
            let result = run_tool(&workspace, &FileState::default(), "search", &args);
            assert!(!result.is_error, "{}", result.model_text);
            assert!(!result.model_text.contains("…+"), "{}", result.model_text);
            assert!(output::escaped_len(&result.model_text) <= 12 * 1024);
            for recall in [
                ResultRecall::None,
                ResultRecall::StoredResult {
                    tool: "search",
                    call: qq_protocol::ToolCallId::from_bytes([0x31; 16]),
                },
            ] {
                let mut prior = "x".repeat(93 * 1024);
                let mut budget = TurnOutputBudget::for_results(
                    [
                        ("other", prior.as_str()),
                        ("search", result.model_text.as_str()),
                    ]
                    .into_iter(),
                );
                budget.admit("other", &mut prior, ResultRecall::None);
                let mut projected = result.model_text.clone();
                assert!(!budget.admit("search", &mut projected, recall));
                assert_eq!(
                    projected, result.model_text,
                    "search pages cannot lose rows before their continuation"
                );
            }
            for path in result.model_text.lines().skip(1) {
                assert!(found.insert(path.to_owned()), "duplicate path");
            }
            cursor = result
                .model_text
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .find_map(|field| field.strip_prefix("next="))
                .map(str::to_owned);
            if let Some(encoded) = &cursor {
                assert!(search::Cursor::decode(encoded).is_some());
            } else {
                break;
            }
        }
        assert_eq!(found.len(), 10);
        assert!(cursor.is_none());
    }

    #[test]
    fn ab2_long_nonmatching_prefix_releases_its_cursor_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = (0..14)
            .map(|_| "a".repeat(235))
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(directory.path().join(prefix)).unwrap();
        let text = format!(
            "{}needle{}\n",
            format!("{}\n", "x".repeat(1_900)).repeat(4),
            "x".repeat(1_890)
        );
        fs::write(directory.path().join("z.txt"), text).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle","context":4}"#,
        );
        assert!(!result.is_error, "{}", result.model_text);
        assert!(
            result.model_text.contains("L5: needle"),
            "{}",
            result.model_text
        );
        assert!(
            !result.model_text.contains("truncated="),
            "{}",
            result.model_text
        );
    }

    #[cfg(unix)]
    #[test]
    fn ab2_unrepresentable_match_returns_an_explicit_budget_error() {
        let directory = tempfile::tempdir().unwrap();
        let root = (0..16)
            .map(|_| "\"".repeat(240))
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(directory.path().join(&root)).unwrap();
        fs::write(directory.path().join(&root).join("AGENTS.md"), "rules").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"AGENTS.md","mode":"names"}"#,
        );
        assert!(result.is_error, "{}", result.model_text);
        assert!(
            result.model_text.contains("search_output_budget"),
            "{}",
            result.model_text
        );
    }

    #[test]
    fn ab2_long_directory_watermark_decodes_and_reaches_later_instructions() {
        search::with_scan_entries(128, || {
            let directory = tempfile::tempdir().unwrap();
            let root = format!("{}/{}", "a".repeat(250), "a".repeat(250));
            fs::create_dir_all(directory.path().join(&root)).unwrap();
            for index in 0..128 {
                fs::create_dir(directory.path().join(&root).join(format!("d{index:05}"))).unwrap();
            }
            fs::create_dir(directory.path().join("z")).unwrap();
            fs::write(directory.path().join("z/AGENTS.md"), "rules\n").unwrap();
            let workspace = Workspace::open(directory.path()).unwrap();
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "search",
                r#"{"query":"AGENTS.md","mode":"names"}"#,
            );
            assert!(!result.is_error, "{}", result.model_text);
            let cursor = result
                .model_text
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .find_map(|field| field.strip_prefix("next="))
                .expect("completed directories must provide a cursor");
            let arguments =
                serde_json::json!({"query":"AGENTS.md","mode":"names","cursor":cursor}).to_string();
            let next = run_tool(&workspace, &FileState::default(), "search", &arguments);
            assert!(!next.is_error, "{}", next.model_text);
            assert!(
                next.model_text.contains("z/AGENTS.md"),
                "{}",
                next.model_text
            );
            assert!(!next.model_text.contains("partial="), "{}", next.model_text);
        });
    }

    #[test]
    fn ab2_names_discover_file_ignored_instructions_in_visible_directories() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::create_dir(directory.path().join("hidden")).unwrap();
        fs::write(
            directory.path().join(".gitignore"),
            "AGENTS.md\nCLAUDE.md\nhidden/\n",
        )
        .unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "rules\n").unwrap();
        fs::write(directory.path().join("src/CLAUDE.md"), "fallback\n").unwrap();
        fs::write(directory.path().join("hidden/AGENTS.md"), "hidden\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true}"#,
        );
        assert!(!result.is_error, "{}", result.model_text);
        for path in ["src/AGENTS.md", "src/CLAUDE.md"] {
            assert!(result.model_text.contains(path), "{}", result.model_text);
        }
        assert!(!result.model_text.contains("hidden/AGENTS.md"));
    }

    #[test]
    fn ab2_instruction_discovery_depth_windows_cover_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let mut path = String::new();
        let mut roots = vec![".".to_owned()];
        let mut expected = Vec::new();
        for depth in 1..=130 {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&format!("d{depth:03}"));
            fs::create_dir(directory.path().join(&path)).unwrap();
            if matches!(depth, 64 | 65 | 128 | 130) {
                let instruction = format!("{path}/AGENTS.md");
                fs::write(directory.path().join(&instruction), "rules\n").unwrap();
                expected.push(instruction);
            }
            if depth % 64 == 0 {
                roots.push(path.clone());
            }
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        let mut discovered = String::new();
        for (index, root) in roots.iter().enumerate() {
            let arguments = serde_json::json!({"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true,"path":root,"include_ignored":true}).to_string();
            let result = run_tool(&workspace, &FileState::default(), "search", &arguments);
            assert!(!result.is_error, "{}", result.model_text);
            if index < 2 {
                assert!(
                    result.model_text.contains("partial=depth"),
                    "{}",
                    result.model_text
                );
            } else {
                assert!(!result.model_text.contains("partial=depth"));
            }
            discovered.push_str(&result.model_text);
        }
        for instruction in expected {
            assert!(discovered.contains(&instruction), "missing {instruction}");
        }
        let hint = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"RULES","case":"sensitive"}"#,
        );
        assert!(
            hint.model_text.contains("case_insensitive_matches=1+"),
            "{}",
            hint.model_text
        );
    }

    #[test]
    fn ab2_names_and_content_search_reach_an_explicit_ignored_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join(".gitignore"), "/.worktrees/\n/src/vendor/\n").unwrap();
        fs::create_dir_all(root.join("src/vendor")).unwrap();
        fs::write(root.join("src/AGENTS.md"), "source ancestor policy\n").unwrap();
        fs::write(root.join("src/NOTAGENTS.md"), "not instructions\n").unwrap();
        fs::write(root.join("src/vendor/AGENTS.md"), "vendor policy\n").unwrap();
        fs::write(root.join("src/vendor/x.rs"), "// target\n").unwrap();
        fs::create_dir_all(root.join(".worktrees/x/nested")).unwrap();
        fs::write(root.join(".worktrees/AGENTS.md"), "ancestor policy\n").unwrap();
        fs::write(root.join(".worktrees/x/AGENTS.md"), "worktree policy\n").unwrap();
        fs::create_dir_all(root.join("target/stk")).unwrap();
        fs::write(root.join("target/CLAUDE.md"), "target ancestor policy\n").unwrap();
        fs::write(
            root.join("target/stk/AGENTS.md"),
            "second worktree policy\n",
        )
        .unwrap();
        fs::write(
            root.join(".worktrees/x/nested/CLAUDE.md"),
            "nested policy\n",
        )
        .unwrap();
        let workspace = Workspace::open(root).unwrap();
        for (arguments, expected) in [
            (
                r#"{"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true,"path":".worktrees","include_ignored":true}"#,
                ".worktrees/x/nested/CLAUDE.md",
            ),
            (
                r#"{"query":"worktree policy","path":".worktrees","include_ignored":true}"#,
                "L1: worktree policy",
            ),
        ] {
            let result = run_tool(&workspace, &FileState::default(), "search", arguments);
            assert!(!result.is_error, "{}", result.model_text);
            if arguments.contains("names") {
                assert!(
                    result.model_text.contains(".worktrees/AGENTS.md"),
                    "{}",
                    result.model_text
                );
            }
            assert!(
                result.model_text.contains(expected),
                "{}",
                result.model_text
            );
        }

        let second = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true,"path":"target","include_ignored":true}"#,
        );
        assert!(!second.is_error, "{}", second.model_text);
        assert!(second.model_text.contains("target/CLAUDE.md"));
        assert!(second.model_text.contains("target/stk/AGENTS.md"));

        let ancestors = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true,"path":"."}"#,
        );
        assert!(!ancestors.is_error, "{}", ancestors.model_text);
        assert!(ancestors.model_text.contains("src/AGENTS.md"));
        assert!(!ancestors.model_text.contains("NOTAGENTS.md"));
        assert!(!ancestors.model_text.contains("src/vendor/AGENTS.md"));
        let ignored = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"(^|/)(AGENTS|CLAUDE)\\.md$","mode":"names","regex":true,"path":"src/vendor","include_ignored":true}"#,
        );
        assert!(!ignored.is_error, "{}", ignored.model_text);
        assert!(ignored.model_text.contains("src/vendor/AGENTS.md"));
    }

    #[test]
    fn search_honours_gitignore_generated_directories_and_include_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join(".gitignore"), "*.log\n/vendor/\n!keep.log\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join("target/debug/out.rs"), "needle in target\n").unwrap();
        fs::write(root.join("vendor/lib.rs"), "needle in vendor\n").unwrap();
        fs::write(root.join("debug.log"), "needle in log\n").unwrap();
        fs::write(root.join("keep.log"), "needle in kept log\n").unwrap();
        fs::write(root.join(".hidden/h.rs"), "needle hidden\n").unwrap();
        fs::write(root.join("nested/.gitignore"), "*.rs\n").unwrap();
        fs::write(root.join("nested/a.rs"), "needle nested rs\n").unwrap();
        fs::write(root.join("nested/a.txt"), "needle nested txt\n").unwrap();
        fs::write(root.join("main.rs"), "needle main\n").unwrap();
        let workspace = Workspace::open(root).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );
        assert_eq!(
            result.model_text,
            "search \"needle\" mode=content matches=3/3 files=3 scanned=3\n\
             keep.log\nL1: needle in kept log\n\
             main.rs\nL1: needle main\n\
             nested/a.txt\nL1: needle nested txt\n"
        );

        // A walk rooted below the workspace root still honours the root file.
        let nested = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle","path":"nested"}"#,
        );
        assert!(
            nested.model_text.contains("nested/a.txt"),
            "{}",
            nested.model_text
        );
        assert!(!nested.model_text.contains("a.rs"));

        let everything = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle","include_ignored":true}"#,
        );
        for path in [
            "target/debug/out.rs",
            "vendor/lib.rs",
            "debug.log",
            ".hidden/h.rs",
            "nested/a.rs",
        ] {
            assert!(
                everything.model_text.contains(path),
                "{path} missing:\n{}",
                everything.model_text
            );
        }

        // `.qqignore` stacks with `.gitignore` and hides from `search` and
        // `tree` alike, so every listing tool sees the same tree.
        fs::write(root.join(".qqignore"), "main.rs\n").unwrap();
        let qq = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );
        assert_eq!(
            qq.model_text,
            "search \"needle\" mode=content matches=2/2 files=2 scanned=2\n\
             keep.log\nL1: needle in kept log\n\
             nested/a.txt\nL1: needle nested txt\n"
        );
        let tree = run_tool(&workspace, &FileState::default(), "tree", "{}");
        assert!(!tree.model_text.contains("main.rs"), "{}", tree.model_text);
        assert!(tree.model_text.contains("keep.log"), "{}", tree.model_text);
    }

    #[test]
    fn search_pages_with_an_exact_cursor_and_caps_per_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // 3 files × 30 lines = 90 matches; page size 60; per-file cap 10 → 30 shown per page.
        for name in ["a.txt", "b.txt", "c.txt"] {
            let body: String = (1..=30).map(|n| format!("needle {n}\n")).collect();
            fs::write(root.join(name), body).unwrap();
        }
        let workspace = Workspace::open(root).unwrap();
        let state = FileState::default();

        let first = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"needle","limit":25}"#,
        );
        let header = first.model_text.lines().next().unwrap();
        assert!(header.contains("matches=25/"), "{header}");
        assert!(
            first.model_text.contains("+20 more in file"),
            "{}",
            first.model_text
        );
        let cursor = header
            .split_whitespace()
            .find_map(|field| field.strip_prefix("next="))
            .expect("a cursor when limit is hit");
        let decoded = search::Cursor::decode(cursor).unwrap();
        assert_eq!(
            decoded,
            search::Cursor {
                path: "c.txt".to_owned(),
                line: 5
            }
        );

        // Resume: match #26 is c.txt L6 and nothing before it repeats. The
        // per-file cap hides the rest of c.txt behind `+N more in file` and
        // the header's total, not behind a cursor.
        let second = run_tool(
            &workspace,
            &state,
            "search",
            &format!(r#"{{"query":"needle","limit":25,"cursor":"{cursor}"}}"#),
        );
        assert!(
            second.model_text.starts_with(
                "search \"needle\" mode=content matches=10/25 files=1 scanned=1\nc.txt\nL6: needle 6\n"
            ),
            "{}",
            second.model_text
        );
        assert!(
            second
                .model_text
                .ends_with("L15: needle 15\n+15 more in file\n")
        );
        assert!(!second.model_text.contains("next="));

        // Match #61 is reachable across pages with the default per-file cap.
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let arguments = match &cursor {
                Some(cursor) => format!(
                    r#"{{"query":"needle","limit":20,"max_per_file":50,"cursor":"{cursor}"}}"#
                ),
                None => r#"{"query":"needle","limit":20,"max_per_file":50}"#.to_owned(),
            };
            let page = run_tool(&workspace, &state, "search", &arguments);
            let mut file = String::new();
            for line in page.model_text.lines().skip(1) {
                if let Some(rest) = line.strip_prefix('L') {
                    let number: u32 = rest.split(':').next().unwrap().parse().unwrap();
                    seen.push((file.clone(), number));
                } else {
                    file = line.to_owned();
                }
            }
            cursor = page
                .model_text
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .find_map(|field| field.strip_prefix("next=").map(str::to_owned));
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 90);
        assert_eq!(seen[60], ("c.txt".to_owned(), 1));
        let mut deduped = seen.clone();
        deduped.dedup();
        assert_eq!(deduped.len(), 90, "no match repeats across pages");

        let bad = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"needle","cursor":"!!"}"#,
        );
        assert!(bad.is_error);
        assert!(bad.model_text.starts_with("cursor_invalid"));
        assert!(
            bad.model_text.contains("omit `cursor` for the first page"),
            "{}",
            bad.model_text
        );

        // ENG-959: every placeholder a model sends for "start from the
        // beginning" is the first page, byte for byte identical to omitting
        // the field. One day's sessions had ~1,470 of 1,485 search calls
        // fail on these.
        let first_page = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"needle","limit":25}"#,
        );
        assert!(!first_page.is_error);
        for placeholder in [
            "", " ", "initial", "INITIAL", "Initial", "start", "START", "first", "First", "0", ".",
            "/", "null", "NULL", "Null", "none", "None", " none ",
        ] {
            let page = run_tool(
                &workspace,
                &state,
                "search",
                &format!(
                    r#"{{"query":"needle","limit":25,"cursor":{}}}"#,
                    serde_json::json!(placeholder)
                ),
            );
            assert!(
                !page.is_error,
                "cursor {placeholder:?}: {}",
                page.model_text
            );
            assert_eq!(
                page.model_text, first_page.model_text,
                "cursor {placeholder:?}"
            );
        }
    }

    #[test]
    fn search_modes_regex_case_context_and_globs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "use x;\n\npub fn apply_lock(&self) {}\n\nfn caller() {\n    apply_lock();\n}\n",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "Apply_Lock notes\r\nplain\r\n").unwrap();
        let workspace = Workspace::open(root).unwrap();
        let state = FileState::default();

        let definition = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"apply_lock","mode":"definition"}"#,
        );
        assert_eq!(
            definition.model_text,
            "search \"apply_lock\" mode=definition matches=1/1 files=1 scanned=2\nsrc/lib.rs\nL3: pub fn apply_lock(&self) {}\n"
        );
        let references = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"apply_lock","mode":"references","case":"sensitive"}"#,
        );
        assert_eq!(
            references.model_text,
            "search \"apply_lock\" mode=references matches=1/1 files=1 scanned=2\nsrc/lib.rs\nL6:     apply_lock();\n"
        );
        let invalid = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"fn (","mode":"definition"}"#,
        );
        assert!(invalid.is_error && invalid.model_text.starts_with("invalid_symbol"));

        // Smart case: a lowercase query is insensitive; CRLF lines lose the CR.
        let smart = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"apply_lock","include":["*.md"]}"#,
        );
        assert_eq!(
            smart.model_text,
            "search \"apply_lock\" mode=content matches=1/1 files=1 scanned=1\nnotes.md\nL1: Apply_Lock notes\n"
        );
        // Sensitive zero-result search hints at the insensitive count.
        let sensitive = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"APPLY_LOCK","case":"sensitive"}"#,
        );
        assert_eq!(
            sensitive.model_text,
            "search \"APPLY_LOCK\" mode=content matches=0/0 files=0 scanned=2 hint=case_insensitive_matches=3\n"
        );

        let regex = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"^fn \\w+\\(","regex":true,"context":1,"exclude":["*.md"]}"#,
        );
        assert_eq!(
            regex.model_text,
            "search \"^fn \\\\w+\\\\(\" mode=content matches=1/1 files=1 scanned=1\nsrc/lib.rs\nL4- \nL5: fn caller() {\nL6-     apply_lock();\n"
        );
        let bad_regex = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"(","regex":true}"#,
        );
        assert!(bad_regex.is_error && bad_regex.model_text.starts_with("invalid_regex"));
        let bad_glob = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"x","include":["["]}"#,
        );
        assert!(bad_glob.is_error && bad_glob.model_text.starts_with("bad_glob"));
        // D9: an empty glob is a filled-in "no filter", and context above the
        // bound clamps with a note instead of refusing the search.
        let defaults = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"^fn \\w+\\(","regex":true,"context":10,"include":[""],"exclude":["","*.md"]}"#,
        );
        assert!(!defaults.is_error, "{}", defaults.model_text);
        let (header, body) = defaults.model_text.split_once('\n').unwrap();
        assert_eq!(
            header,
            "search \"^fn \\\\w+\\\\(\" mode=content matches=1/1 files=1 scanned=1 note=context_clamped=5,empty_glob_ignored"
        );
        assert!(body.starts_with("src/lib.rs\n"), "{body}");
        let empty_only = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"^fn \\w+\\(","regex":true,"include":[""]}"#,
        );
        assert!(
            empty_only
                .model_text
                .lines()
                .next()
                .unwrap()
                .ends_with(" note=empty_glob_ignored"),
            "{}",
            empty_only.model_text
        );
        let escape = run_tool(&workspace, &state, "search", r#"{"query":"x","path":".."}"#);
        assert!(escape.is_error && escape.model_text.starts_with("path_escapes_workspace"));
    }

    #[test]
    fn search_tolerates_invalid_utf8_and_a_symlink_loop() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("mixed.txt"),
            b"needle before\n\xff\xfe bad bytes\nneedle after\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(".", root.join("loop")).unwrap();
        let workspace = Workspace::open(root).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );
        assert_eq!(
            result.model_text,
            "search \"needle\" mode=content matches=2/2 files=1 scanned=1\nmixed.txt\nL1: needle before\nL3: needle after\n"
        );
    }

    #[test]
    fn search_stops_at_the_byte_budget_with_a_cursor_instead_of_cutting() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // 8 files × 50 matches of ~100 bytes: over the 12 KiB body budget.
        for index in 0..8 {
            let body: String = (1..=50)
                .map(|n| format!("needle {n} {}\n", "x".repeat(90)))
                .collect();
            fs::write(root.join(format!("big-{index}.txt")), body).unwrap();
        }
        let workspace = Workspace::open(root).unwrap();
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle","limit":500,"max_per_file":50}"#,
        );
        let header = result.model_text.lines().next().unwrap();
        assert!(header.contains("truncated=bytes"), "{header}");
        assert!(header.contains("next="), "{header}");
        assert!(
            !result.model_text.contains(MARKER_PREFIX),
            "no mid-body cut"
        );
        assert!(escaped_len(&result.model_text) <= search::SEARCH_BOUNDS.max_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_or_oversized_ignore_file_is_skipped_not_read() {
        // A FIFO `.gitignore` used to block the walk at open forever, and an
        // ignore file of any size was read whole. Both are now ignored as if
        // absent; a normal `.qqignore` beside them still applies.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let status = std::process::Command::new("mkfifo")
            .arg(root.join(".gitignore"))
            .status()
            .unwrap();
        assert!(status.success());
        let oversized = format!(
            "a.md\n{}",
            "#".repeat(usize::try_from(walk::MAX_IGNORE_FILE_BYTES).unwrap())
        );
        fs::write(root.join(".ignore"), oversized).unwrap();
        fs::write(root.join(".qqignore"), "b.md\n").unwrap();
        fs::write(root.join("a.md"), "a").unwrap();
        fs::write(root.join("b.md"), "b").unwrap();
        let workspace = Workspace::open(root).unwrap();
        let state = FileState::default();

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let tree = run_tool(&workspace, &state, "tree", r#"{"depth":1}"#);
            let _ = sender.send(tree);
        });
        let tree = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the walk does not block on a FIFO ignore file");
        assert!(!tree.is_error, "{}", tree.model_text);
        assert!(
            tree.model_text.contains("a.md"),
            "oversized .ignore is not applied: {}",
            tree.model_text
        );
        assert!(
            !tree.model_text.contains("b.md"),
            ".qqignore still applies: {}",
            tree.model_text
        );
    }

    #[test]
    fn tree_fills_breadth_first_with_counts_chains_and_ignored_markers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src/deep/er/est")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("src/deep/er/est/leaf.rs"), "x".repeat(2048)).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("target/debug/bin"), "").unwrap();
        fs::write(root.join("docs/a.md"), "a").unwrap();
        fs::write(root.join("docs/b.md"), "bb").unwrap();
        fs::write(root.join("README.md"), "readme").unwrap();
        fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();
        fs::write(root.join("scratch.tmp"), "ignored").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("README.md", root.join("link.md")).unwrap();
        let workspace = Workspace::open(root).unwrap();
        let state = FileState::default();

        // depth=3 lists src/deep/er but not est; the chain collapses as far
        // as it was listed and the counts cover the whole subtree.
        let tree = run_tool(&workspace, &state, "tree", r#"{"depth":3}"#);
        assert!(!tree.is_error, "{}", tree.model_text);
        let expected_link = if cfg!(unix) { "link.md@\n" } else { "" };
        assert_eq!(
            tree.model_text,
            format!(
                "tree . depth=3 entries={n}/{n} files={f} dirs=5\n\
                 README.md 6\n\
                 docs/ (2f 0d)\n\
                 \x20 a.md 1  b.md 2\n\
                 {link}\
                 src/ (2f 3d)\n\
                 \x20 deep/er/ (1f 1d)\n\
                 \x20 main.rs 13\n\
                 target/ …ignored\n",
                n = if cfg!(unix) { 10 } else { 9 },
                f = if cfg!(unix) { 5 } else { 4 },
                link = expected_link
            )
        );
        let deep = run_tool(&workspace, &state, "tree", r#"{"depth":4,"path":"src"}"#);
        assert!(
            deep.model_text
                .contains("deep/er/est/ (1f 0d)\n  leaf.rs 2.0k\n"),
            "{}",
            deep.model_text
        );

        let shallow = run_tool(&workspace, &state, "tree", r#"{"limit":3,"depth":1}"#);
        assert!(
            shallow.model_text.starts_with("tree . depth=1 entries=3/"),
            "{}",
            shallow.model_text
        );
        assert!(shallow.model_text.contains("more entries; raise limit"));

        let globbed = run_tool(&workspace, &state, "tree", r#"{"glob":"*.md","depth":2}"#);
        assert!(
            globbed.model_text.contains("a.md"),
            "{}",
            globbed.model_text
        );
        assert!(!globbed.model_text.contains("main.rs"));

        let not_dir = run_tool(&workspace, &state, "tree", r#"{"path":"README.md"}"#);
        assert!(not_dir.is_error && not_dir.model_text.starts_with("not_a_directory"));
        let too_deep = run_tool(&workspace, &state, "tree", r#"{"depth":7}"#);
        assert!(too_deep.is_error && too_deep.model_text.starts_with("invalid_depth"));
    }

    #[test]
    fn cancellation_and_large_directories_stop_at_explicit_bounds() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "content").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = execute_blocking(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"note.txt"}"#,
            &ToolCancellation::new(RunCancellation::already_cancelled()),
        );
        assert!(result.is_error);
        assert!(result.model_text.contains("cancelled"));

        for index in 0..=tree::MAX_ENTRIES {
            fs::write(directory.path().join(format!("entry-{index}")), "").unwrap();
        }
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "tree",
            &format!(r#"{{"path":".","depth":1,"limit":{}}}"#, tree::MAX_ENTRIES),
        );
        assert!(!result.is_error);
        assert!(
            result.model_text.starts_with(&format!(
                "tree . depth=1 entries={}/{} ",
                tree::MAX_ENTRIES,
                tree::MAX_ENTRIES + 2
            )),
            "{}",
            result.model_text.lines().next().unwrap()
        );
        assert!(result.model_text.contains("2 more entries; raise limit"));

        // D9: `"glob":""` is a filled-in "no filter", not `bad_glob`.
        let unfiltered = run_tool(
            &workspace,
            &FileState::default(),
            "tree",
            &format!(
                r#"{{"path":".","depth":1,"limit":{},"glob":""}}"#,
                tree::MAX_ENTRIES
            ),
        );
        let (header, body) = unfiltered.model_text.split_once('\n').unwrap();
        let (expected_header, expected_body) = result.model_text.split_once('\n').unwrap();
        assert_eq!(header, format!("{expected_header} note=empty_glob_ignored"));
        assert_eq!(body, expected_body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_swap_cannot_redirect_an_admitted_write_into_git_trust_files() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("ordinary")).unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(directory.path().join(".git/config"), "trusted").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let (entered, release) = hold_tool_apply(workspace.path());
        let write = tokio::task::spawn_blocking(move || {
            run_tool(
                &workspace,
                &FileState::default(),
                "write_file",
                r#"{"path":"ordinary/config","content":"task data"}"#,
            )
        });
        entered.await.unwrap();
        let temp = fs::read_dir(directory.path().join("ordinary"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name();
        fs::rename(
            directory.path().join("ordinary"),
            directory.path().join("original-directory"),
        )
        .unwrap();
        std::os::unix::fs::symlink(".git", directory.path().join("ordinary")).unwrap();
        fs::write(directory.path().join(".git").join(&temp), "redirected").unwrap();
        release.send(()).unwrap();
        let _result = write.await.unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join(".git/config")).unwrap(),
            "trusted"
        );
    }

    #[tokio::test]
    async fn file_tools_preserve_human_changes_during_staging() {
        for (name, existing) in [
            ("write_file", false),
            ("write_file", true),
            ("edit_file", true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            if existing {
                fs::write(directory.path().join("a"), "original").unwrap();
            }
            let workspace = Workspace::open(&directory.path().canonicalize().unwrap()).unwrap();
            let arguments = if name == "edit_file" {
                json!({"edits":[{"path":"a","old":"original","new":"agent change"}]})
            } else {
                json!({"path":"a","content":"agent change", "if_hash":format!("h:{}", &content_hash(b"original")[..12])})
            };
            let (entered, release) = hold_tool_apply(workspace.path());
            let operation = tokio::task::spawn_blocking(move || {
                run_tool(
                    &workspace,
                    &FileState::default(),
                    name,
                    &arguments.to_string(),
                )
            });
            entered.await.unwrap();
            fs::write(directory.path().join("a"), "human change").unwrap();
            release.send(()).unwrap();
            let result = operation.await.unwrap();
            assert!(result.is_error, "{name}: {}", result.model_text);
            assert!(result.file_states.is_empty());
            assert_eq!(
                fs::read_to_string(directory.path().join("a")).unwrap(),
                "human change"
            );
            assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".qq-apply-")
            }));
        }
    }

    #[test]
    fn dispatch_protects_policy_and_git_trust_files_without_blocking_ordinary_writes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for path in [
            "src/main.rs",
            ".github/workflows/check.yml",
            ".qq/skills/example/SKILL.md",
        ] {
            let result = run_tool(
                &workspace,
                &state,
                "write_file",
                &json!({"path":path,"content":"allowed"}).to_string(),
            );
            assert!(!result.is_error, "{path}: {}", result.model_text);
            assert_eq!(
                fs::read_to_string(directory.path().join(path)).unwrap(),
                "allowed"
            );
        }
        for path in [
            ".qq/config.ron",
            ".qq/config.d/policy.ron",
            ".qq/task-publication.lock",
            ".qq/packs/untrusted/pack.ron",
            "qq.ron",
            ".git",
            "nested/.git/config",
            ".git/hooks/pre-commit",
        ] {
            let result = run_tool(
                &workspace,
                &state,
                "write_file",
                &json!({"path":path,"content":"unsafe"}).to_string(),
            );
            assert!(
                result.is_error,
                "{path} was writable: {}",
                result.model_text
            );
            assert!(
                result.model_text.contains("protected_path"),
                "{}",
                result.model_text
            );
            assert!(!directory.path().join(path).exists());
        }
    }

    #[test]
    fn dispatch_rejects_a_protected_edit_batch_before_changing_any_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".qq")).unwrap();
        fs::write(directory.path().join(".qq/config.ron"), "original").unwrap();
        fs::write(directory.path().join("ordinary.txt"), "original").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        for path in [".qq/config.ron", "ordinary.txt"] {
            run_tool(
                &workspace,
                &state,
                "read_file",
                &json!({"path":path}).to_string(),
            );
        }
        let result = run_tool(
            &workspace,
            &state,
            "edit_file",
            &json!({"edits":[
                {"path":"ordinary.txt","old":"original","new":"changed"},
                {"path":".qq/config.ron","old":"original","new":"changed"}
            ]})
            .to_string(),
        );
        assert!(result.is_error, "{}", result.model_text);
        assert!(
            result.model_text.contains("protected_path"),
            "{}",
            result.model_text
        );
        for path in [".qq/config.ron", "ordinary.txt"] {
            assert_eq!(
                fs::read_to_string(directory.path().join(path)).unwrap(),
                "original"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn dispatch_protects_canonical_policy_and_git_aliases() {
        let directory = tempfile::tempdir().unwrap();
        for name in [".git", ".qq"] {
            fs::create_dir(directory.path().join(name)).unwrap();
        }
        std::os::unix::fs::symlink(".git", directory.path().join("git-alias")).unwrap();
        std::os::unix::fs::symlink(".qq", directory.path().join("policy-alias")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        for path in ["git-alias/hooks/pre-commit", "policy-alias/config.ron"] {
            let result = run_tool(
                &workspace,
                &FileState::default(),
                "write_file",
                &json!({"path":path,"content":"unsafe"}).to_string(),
            );
            assert!(result.is_error, "{path}: {}", result.model_text);
            assert!(
                result.model_text.contains("protected_path"),
                "{}",
                result.model_text
            );
            assert!(!directory.path().join(path).exists());
        }
    }

    #[test]
    fn edit_replaces_exact_strings_and_refreshes_the_recorded_hash() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("main.rs"),
            "fn one() {}\nfn two() {}\n",
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"main.rs"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"main.rs","old":"fn one() {}","new":"fn one() { start() }"}]}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.model_text);
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs")).unwrap(),
            "fn one() { start() }\nfn two() {}\n"
        );
        let update = edited.file_states.into_iter().next().unwrap();
        assert_eq!(update.path, "main.rs");
        assert_eq!(
            update.hash,
            content_hash(b"fn one() { start() }\nfn two() {}\n")
        );

        // The recorded hash was refreshed by the apply, so a follow-up edit
        // needs no intervening read.
        let followup = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"main.rs","old":"fn two() {}","new":"fn two() { end() }"}]}"#,
        );
        assert!(
            !followup.is_error,
            "unexpected error: {}",
            followup.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs")).unwrap(),
            "fn one() { start() }\nfn two() { end() }\n"
        );
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("list.txt"), "item\nitem\nitem\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"list.txt"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"list.txt","old":"item","new":"entry","replace_all":true}]}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.model_text);
        assert!(
            edited.model_text.ends_with(&format!(
                "\nlist.txt h:{} L1 -1+1 x3\n",
                &content_hash(b"entry\nentry\nentry\n")[..12]
            )),
            "{}",
            edited.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("list.txt")).unwrap(),
            "entry\nentry\nentry\n"
        );
    }

    #[test]
    #[cfg(feature = "tool-fetch")]
    fn built_in_tool_declarations_keep_their_order_and_schema_identity() {
        let specs = specs();
        assert_eq!(
            specs.iter().map(|spec| spec.name()).collect::<Vec<_>>(),
            [
                "read_file",
                "tree",
                "search",
                "edit_file",
                "write_file",
                "shell",
                "exec",
                "ask_user",
                "fetch",
            ]
        );
        assert!(!specs.iter().any(|spec| spec.name() == SPAWN_AGENT_TOOL));
        assert!(!specs.iter().any(|spec| spec.name() == "list_dir"));
        assert_eq!(
            crate::runtime::tool_schema_measurement(&specs)
                .hash
                .to_string(),
            "521e3115c1cf748d75b32e337ce10c086b8e424d569a905238bd63e5917b32ef"
        );
    }

    #[test]
    fn fetch_catalog_presence_matches_the_build_feature() {
        let present = specs().iter().any(|spec| spec.name() == "fetch");
        assert_eq!(present, cfg!(feature = "tool-fetch"));
        assert_eq!(specs::BuiltInTool::from_name("fetch").is_some(), present);
    }

    #[test]
    fn spawn_agent_model_override_is_explicit_and_omitted_by_default() {
        let routes = [
            "anthropic/claude-test".to_owned(),
            "openai-codex/gpt-test".to_owned(),
        ];
        let spec = spawn_agent_spec(&routes, &qq_protocol::DelegationRoster::default());
        assert!(spec.description().contains("Omit model by default"));
        assert!(
            spec.description()
                .contains("never guess, translate, or invent a route")
        );
        let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
        assert_eq!(schema["required"], json!(["task"]));
        assert_eq!(schema["properties"]["model"]["enum"], json!(routes));
        let model = schema["properties"]["model"]["description"]
            .as_str()
            .unwrap();
        assert!(model.contains("Omit by default"));
        assert!(model.contains("configured worker model"));
        assert!(model.contains("this session's selected model"));
        assert!(model.contains("never guess or translate providers"));
    }

    fn roster() -> qq_protocol::DelegationRoster {
        qq_protocol::DelegationRoster {
            roster: vec![
                qq_protocol::DelegationRosterEntry {
                    route: "openai/fast".to_owned(),
                    role: qq_protocol::DelegationRole::Fast,
                    note: Some("lookups".to_owned()),
                    effort: None,
                    context_window: Some(400_000),
                    max_output_tokens: None,
                    relative_cost_permille: Some(150),
                },
                qq_protocol::DelegationRosterEntry {
                    route: "anthropic/strong".to_owned(),
                    role: qq_protocol::DelegationRole::Strong,
                    note: None,
                    effort: None,
                    context_window: None,
                    max_output_tokens: None,
                    relative_cost_permille: Some(2_500),
                },
            ],
            default_role: qq_protocol::DelegationRole::Fast,
            max_depth: 1,
            write_children: false,
        }
    }

    #[test]
    fn spawn_agent_with_a_roster_selects_by_role_and_limits_overrides_to_roster_routes() {
        // The flat authenticated list is ignored once a roster exists: the
        // model may only name roster routes exactly.
        let every_route = ["openai/fast".to_owned(), "openai/other".to_owned()];
        let spec = spawn_agent_spec(&every_route, &roster());
        let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
        assert_eq!(schema["required"], json!(["task"]));
        assert_eq!(
            schema["properties"]["role"]["enum"],
            json!(["fast", "strong"])
        );
        assert!(
            schema["properties"]["role"]["description"]
                .as_str()
                .unwrap()
                .contains("default (fast)")
        );
        assert_eq!(
            schema["properties"]["model"]["enum"],
            json!(["openai/fast", "anthropic/strong"])
        );
        assert!(spec.description().contains("Choose the sub-agent by role"));
        let bytes = spec.name().len() + spec.description().len() + schema.to_string().len();
        assert!(bytes <= MAX_SPAWN_AGENT_SCHEMA_BYTES, "{bytes}");

        let parsed: SpawnAgentArgs =
            serde_json::from_str(r#"{"task":"t","role":"strong"}"#).unwrap();
        assert_eq!(parsed.role, Some(qq_protocol::DelegationRole::Strong));
        assert!(serde_json::from_str::<SpawnAgentArgs>(r#"{"task":"t","role":"warp"}"#).is_err());
    }

    #[test]
    fn spawn_agent_authority_guidance_matches_write_capability() {
        for mut delegation in [qq_protocol::DelegationRoster::default(), roster()] {
            for write_children in [false, true] {
                delegation.write_children = write_children;
                let spec = spawn_agent_spec(&[], &delegation);
                let schema: serde_json::Value =
                    serde_json::from_str(spec.input_schema().get()).unwrap();
                assert_eq!(
                    schema["properties"].get("authority").is_some(),
                    write_children
                );
                if write_children {
                    assert!(!spec.description().contains("to a read-only sub-agent"));
                    assert!(spec.description().contains("authority"));
                    assert!(spec.description().contains("Auto within the parent"));
                    assert!(!spec.description().contains("reviewer_model"));
                    assert!(!spec.description().contains("supervised approval"));
                    let authority = schema["properties"]["authority"]["description"]
                        .as_str()
                        .unwrap();
                    assert!(authority.contains("Auto within the parent"));
                    assert!(!authority.contains("reviewer_model"));
                    assert!(!authority.contains("supervised approval"));
                    assert!(spec.description().contains("one write sub-agent"));
                    assert_eq!(
                        schema["properties"]["authority"]["enum"],
                        json!(["read", "write"])
                    );
                } else {
                    assert!(spec.description().contains("to a read-only sub-agent"));
                    assert!(!spec.description().contains("authority"));
                }
            }
        }
    }

    #[test]
    fn spawn_agent_hides_model_override_without_authenticated_routes() {
        let spec = spawn_agent_spec(&[], &qq_protocol::DelegationRoster::default());
        let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
        assert!(schema["properties"].get("model").is_none());
        assert!(schema["properties"].get("role").is_none());
        assert_eq!(schema["required"], json!(["task"]));
    }

    #[test]
    fn edit_fails_precisely_on_absent_and_ambiguous_old_strings() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("list.txt"), "item\nitem\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"list.txt"}"#);

        let absent = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"list.txt","old":"missing","new":"other"}]}"#,
        );
        assert!(absent.is_error);
        assert!(
            absent.model_text.contains("not found"),
            "{}",
            absent.model_text
        );

        let ambiguous = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"list.txt","old":"item","new":"entry"}]}"#,
        );
        assert!(ambiguous.is_error);
        assert!(
            ambiguous
                .model_text
                .starts_with("edit 0: ambiguous: 2 matches via exact at lines L1,L2")
                && ambiguous.model_text.contains("replace_all"),
            "{}",
            ambiguous.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("list.txt")).unwrap(),
            "item\nitem\n"
        );
    }

    #[test]
    fn edits_and_overwrites_require_a_prior_read_in_this_session() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "content\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        // ADR-0058: a unique exact `old` is proof enough; an anchor is not.
        let edit = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"note.txt","insert_after":"content","new":"changed"}]}"#,
        );
        assert!(edit.is_error);
        assert!(edit.model_text.contains("read_file"), "{}", edit.model_text);

        let overwrite = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"note.txt","content":"replaced\n"}"#,
        );
        assert!(overwrite.is_error);
        assert!(
            overwrite.model_text.contains("read_file"),
            "{}",
            overwrite.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "content\n"
        );
    }

    #[test]
    fn stale_files_fail_the_apply_until_they_are_reread() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "original\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"note.txt"}"#);

        // An external writer (editor, another process) changes the file
        // between the read and the apply.
        fs::write(directory.path().join("note.txt"), "external change\n").unwrap();
        let stale = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"note.txt","old":"original","new":"edited"}]}"#,
        );
        assert!(stale.is_error);
        assert!(
            stale.model_text.contains("changed since"),
            "{}",
            stale.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "external change\n"
        );

        run_tool(&workspace, &state, "read_file", r#"{"path":"note.txt"}"#);
        let retried = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"note.txt","old":"external change","new":"edited"}]}"#,
        );
        assert!(
            !retried.is_error,
            "unexpected error: {}",
            retried.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn concurrent_sessions_conflict_at_file_granularity() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("shared.txt"), "base\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let winner = FileState::default();
        let loser = FileState::default();
        run_tool(&workspace, &winner, "read_file", r#"{"path":"shared.txt"}"#);
        run_tool(&workspace, &loser, "read_file", r#"{"path":"shared.txt"}"#);

        let won = run_tool(
            &workspace,
            &winner,
            "edit_file",
            r#"{"edits":[{"path":"shared.txt","old":"base","new":"winner"}]}"#,
        );
        assert!(!won.is_error, "unexpected error: {}", won.model_text);

        let lost = run_tool(
            &workspace,
            &loser,
            "edit_file",
            r#"{"edits":[{"path":"shared.txt","old":"base","new":"loser"}]}"#,
        );
        assert!(lost.is_error);
        assert!(
            lost.model_text.contains("changed since"),
            "{}",
            lost.model_text
        );

        run_tool(&workspace, &loser, "read_file", r#"{"path":"shared.txt"}"#);
        let reconciled = run_tool(
            &workspace,
            &loser,
            "edit_file",
            r#"{"edits":[{"path":"shared.txt","old":"winner","new":"reconciled"}]}"#,
        );
        assert!(
            !reconciled.is_error,
            "unexpected error: {}",
            reconciled.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("shared.txt")).unwrap(),
            "reconciled\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_applies_preserve_permissions_and_leave_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("run.sh");
        fs::write(&target, "echo one\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o754)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"run.sh"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"run.sh","old":"echo one","new":"echo two"}]}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.model_text);
        assert_eq!(fs::read_to_string(&target).unwrap(), "echo two\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o754
        );

        let overwritten = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"run.sh","content":"echo three\n"}"#,
        );
        assert!(
            !overwritten.is_error,
            "unexpected error: {}",
            overwritten.model_text
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o754
        );
        let leftovers = fs::read_dir(directory.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".qq-apply-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn write_file_creates_new_files_without_a_prior_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("docs")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let created = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"docs/NOTES.md","content":"first\n"}"#,
        );
        assert!(
            !created.is_error,
            "unexpected error: {}",
            created.model_text
        );
        assert!(
            created
                .model_text
                .starts_with("write docs/NOTES.md created bytes=6 lines=1 h:"),
            "{}",
            created.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/NOTES.md")).unwrap(),
            "first\n"
        );
        let update = created.file_states.into_iter().next().unwrap();
        assert_eq!(update.path, "docs/NOTES.md");
        assert_eq!(update.hash, content_hash(b"first\n"));

        // The create recorded the written content, so the same session may
        // overwrite it without an intervening read.
        let overwritten = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"docs/NOTES.md","content":"second\n"}"#,
        );
        assert!(
            !overwritten.is_error,
            "unexpected error: {}",
            overwritten.model_text
        );
        assert!(
            overwritten
                .model_text
                .starts_with("write docs/NOTES.md replaced bytes=7 lines=1 h:"),
            "{}",
            overwritten.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/NOTES.md")).unwrap(),
            "second\n"
        );

        // Missing parents are created, up to eight deep; the path stays
        // contained and `..` is refused outright.
        let nested = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"missing/a/b/NOTES.md","content":"first\n"}"#,
        );
        assert!(!nested.is_error, "{}", nested.model_text);
        assert_eq!(
            fs::read_to_string(directory.path().join("missing/a/b/NOTES.md")).unwrap(),
            "first\n"
        );
        let too_deep = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"1/2/3/4/5/6/7/8/9/NOTES.md","content":"x"}"#,
        );
        assert!(
            too_deep.model_text.starts_with("too_deep"),
            "{}",
            too_deep.model_text
        );
        let escape = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"missing/../../escape.md","content":"x"}"#,
        );
        assert!(escape.is_error);
        assert!(!directory.path().join("../escape.md").exists());

        // create_only refuses an existing file; if_hash proves currency
        // without a recorded read.
        let fresh = FileState::default();
        let refused = run_tool(
            &workspace,
            &fresh,
            "write_file",
            r#"{"path":"docs/NOTES.md","content":"x","create_only":true}"#,
        );
        assert!(
            refused.model_text.starts_with("exists"),
            "{}",
            refused.model_text
        );
        let unread = run_tool(
            &workspace,
            &fresh,
            "write_file",
            r#"{"path":"docs/NOTES.md","content":"third\n"}"#,
        );
        assert!(
            unread.model_text.starts_with("not_read"),
            "{}",
            unread.model_text
        );
        let short = &content_hash(b"second\n")[..12];
        let proven = run_tool(
            &workspace,
            &fresh,
            "write_file",
            &format!(r#"{{"path":"docs/NOTES.md","content":"third\n","if_hash":"h:{short}"}}"#),
        );
        assert!(!proven.is_error, "{}", proven.model_text);
        let wrong = run_tool(
            &workspace,
            &fresh,
            "write_file",
            &format!(r#"{{"path":"docs/NOTES.md","content":"x","if_hash":"h:{short}"}}"#),
        );
        assert!(
            wrong.model_text.starts_with("stale_file"),
            "{}",
            wrong.model_text
        );
    }

    /// Models with no hash fill `if_hash`/`if_changed_since` with zeros
    /// (21 stale_file errors in 15 v0.1.8 runs). The placeholder is no
    /// claim at all: a read file edits as if it were absent, and an unread
    /// one still needs a read.
    #[test]
    fn zero_hash_placeholder_reads_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let zero = "h:000000000000";

        let read = run_tool(
            &workspace,
            &state,
            "read_file",
            &format!(r#"{{"path":"a.txt","if_changed_since":"{zero}"}}"#),
        );
        assert!(!read.is_error, "{}", read.model_text);
        assert!(read.model_text.contains("alpha"), "{}", read.model_text);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            &format!(
                r#"{{"edits":[{{"path":"a.txt","old":"a","new":"A","replace_all":true,"if_hash":"{zero}"}}]}}"#
            ),
        );
        assert!(!edited.is_error, "{}", edited.model_text);
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "AlphA\nbetA\n"
        );

        let written = run_tool(
            &workspace,
            &state,
            "write_file",
            &format!(r#"{{"path":"a.txt","content":"gamma\n","if_hash":"{zero}"}}"#),
        );
        assert!(!written.is_error, "{}", written.model_text);

        fs::write(directory.path().join("b.txt"), "one\n").unwrap();
        let unread = run_tool(
            &workspace,
            &FileState::default(),
            "write_file",
            &format!(r#"{{"path":"b.txt","content":"two\n","if_hash":"{zero}"}}"#),
        );
        assert!(
            unread.model_text.starts_with("not_read"),
            "{}",
            unread.model_text
        );
    }

    #[test]
    fn write_file_hints_at_edit_file_when_most_lines_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        let before: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        fs::write(directory.path().join("big.txt"), &before).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"big.txt"}"#);

        let mostly_same = before.replace("line 7\n", "LINE 7\n");
        let hinted = run_tool(
            &workspace,
            &state,
            "write_file",
            &serde_json::json!({ "path": "big.txt", "content": mostly_same }).to_string(),
        );
        assert!(!hinted.is_error, "{}", hinted.model_text);
        assert!(
            hinted.model_text.contains(" hint=use_edit_file"),
            "{}",
            hinted.model_text
        );
        match hinted.ui_payload {
            Some(qq_protocol::ToolCallDisplay::Diff { path, diff }) => {
                assert_eq!(path, "big.txt");
                assert!(diff.contains("-line 7\n+LINE 7\n"), "{diff}");
            }
            other => panic!("expected a diff payload, got {other:?}"),
        }

        let rewrite = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"big.txt","content":"totally\ndifferent\n"}"#,
        );
        assert!(
            !rewrite.model_text.contains("hint="),
            "{}",
            rewrite.model_text
        );
    }

    #[test]
    fn edit_batches_apply_in_order_across_files_with_anchors_and_report_each() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("a.rs"),
            "use std::fmt;\n\nfn one() {}\n\nfn three() {}\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("b.rs"),
            "    let x = 1;\n    let y = 2;\n",
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"a.rs"}"#);
        run_tool(&workspace, &state, "read_file", r#"{"path":"b.rs"}"#);

        // Four edits, two files, out of path order: a replace, an insert
        // after an anchor, an insert before one, and a dedented (indent-
        // flexible) replace whose replacement is re-indented.
        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[
                {"path":"b.rs","old":"\tlet y = 2;\n","new":"\tlet y = 20;\n\tlet z = 3;\n"},
                {"path":"a.rs","old":"fn one() {}","new":"fn one() { 1 }"},
                {"path":"a.rs","insert_after":"fn one() { 1 }","new":"\nfn two() {}"},
                {"path":"a.rs","insert_before":"use std::fmt;","new":"//! crate docs"}
            ]}"#,
        );
        assert!(!edited.is_error, "{}", edited.model_text);
        let a_after =
            "//! crate docs\nuse std::fmt;\n\nfn one() { 1 }\n\nfn two() {}\n\nfn three() {}\n";
        let b_after = "    let x = 1;\n    let y = 20;\n    let z = 3;\n";
        assert_eq!(
            fs::read_to_string(directory.path().join("a.rs")).unwrap(),
            a_after
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("b.rs")).unwrap(),
            b_after
        );
        // Header, then one line per file in path order with a change per edit
        // (line numbers as of that edit) and via= for the non-exact one.
        assert_eq!(
            edited.model_text,
            format!(
                "edit ok files=2 edits=4\na.rs h:{} L3 -1+1 | L4 +0+2 | L1 -0+1\nb.rs h:{} L2 -1+2 via=indent_flexible\n",
                &content_hash(a_after.as_bytes())[..12],
                &content_hash(b_after.as_bytes())[..12]
            )
            .replace("+0+2", "-0+2")
        );
        assert_eq!(edited.file_states.len(), 2);
        assert_eq!(
            state.recorded("a.rs"),
            Some(content_hash(a_after.as_bytes()))
        );
        assert_eq!(
            state.recorded("b.rs"),
            Some(content_hash(b_after.as_bytes()))
        );
        match edited.ui_payload {
            Some(qq_protocol::ToolCallDisplay::Diff { path, diff }) => {
                assert_eq!(path, "a.rs");
                assert!(diff.starts_with("--- a/a.rs\n+++ b/a.rs\n@@ "), "{diff}");
                assert!(diff.contains("\n--- a/b.rs\n+++ b/b.rs\n"), "{diff}");
                assert!(
                    diff.contains("-    let y = 2;\n+    let y = 20;\n+    let z = 3;\n"),
                    "{diff}"
                );
            }
            other => panic!("expected a diff payload, got {other:?}"),
        }
    }

    #[test]
    fn edit_batches_fail_whole_with_the_edit_index_and_write_nothing() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        fs::write(directory.path().join("b.txt"), "one\ntwo\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"a.txt"}"#);
        run_tool(&workspace, &state, "read_file", r#"{"path":"b.txt"}"#);

        for (arguments, code) in [
            // The second edit's old is gone after the first rewrote it.
            (
                r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma"},{"path":"a.txt","old":"beta","new":"delta"}]}"#,
                "edit 1: not_found",
            ),
            (
                r#"{"edits":[{"path":"a.txt","old":"x","new":"y"},{"path":"b.txt","old":"one","new":"uno"}]}"#,
                "edit 0: not_found",
            ),
            (
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"a","insert_after":"beta"}]}"#,
                "edit 0: invalid_edit",
            ),
            (
                r#"{"edits":[{"path":"a.txt","new":"a"}]}"#,
                "edit 0: invalid_edit",
            ),
            (
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"alpha"}]}"#,
                "edit 0: invalid_edit",
            ),
            (
                r#"{"edits":[{"path":"a.txt","insert_after":"alpha","new":"z","replace_all":true}]}"#,
                "edit 0: invalid_edit",
            ),
            (
                r#"{"edits":[{"path":"c.txt","old":"a","new":"b"}]}"#,
                "edit 0: path_not_found",
            ),
            (
                r#"{"edits":[{"path":".","old":"a","new":"b"}]}"#,
                "edit 0: not_a_file",
            ),
            (
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"b","if_hash":"h:zz"}]}"#,
                "edit 0: invalid_if_hash",
            ),
            (
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"b","if_hash":"h:0123456789ab"}]}"#,
                "edit 0: stale_file",
            ),
            (r#"{"edits":[]}"#, "invalid_edit"),
            // The second edit matches inside what the first wrote.
            (
                r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma delta"},{"path":"a.txt","old":"delta","new":"epsilon"}]}"#,
                "edit 1: conflicting_edits: a=0 b=1",
            ),
        ] {
            let failed = run_tool(&workspace, &state, "edit_file", arguments);
            assert!(failed.is_error, "{arguments}");
            assert!(
                failed.model_text.starts_with(code),
                "{arguments}: {}",
                failed.model_text
            );
            assert!(failed.ui_payload.is_none());
            assert!(failed.file_states.is_empty());
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("b.txt")).unwrap(),
            "one\ntwo\n"
        );

        // not_found carries the closest line so the retry needs no read.
        let close = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"betta\n","new":"x"}]}"#,
        );
        assert!(
            close.model_text.contains("closest L2 distance=0."),
            "{}",
            close.model_text
        );
        assert!(
            close.model_text.contains("\nL2: beta"),
            "{}",
            close.model_text
        );
    }

    #[test]
    fn edit_dry_run_previews_without_writing_and_if_hash_replaces_a_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        // No read in this session: if_hash from a header stands in for it.
        let state = FileState::default();
        let short = &content_hash(b"alpha\nbeta\n")[..12];

        let preview = run_tool(
            &workspace,
            &state,
            "edit_file",
            &format!(
                r#"{{"edits":[{{"path":"a.txt","old":"beta","new":"gamma","if_hash":"h:{short}"}}],"dry_run":true}}"#
            ),
        );
        assert!(!preview.is_error, "{}", preview.model_text);
        assert!(
            preview
                .model_text
                .starts_with("edit dry_run files=1 edits=1\n"),
            "{}",
            preview.model_text
        );
        // D9: an empty unused form is ignored and the header says so.
        let tolerant = run_tool(
            &workspace,
            &state,
            "edit_file",
            &format!(
                r#"{{"edits":[{{"path":"a.txt","old":"","insert_after":"beta","new":"gamma","if_hash":"h:{short}"}}],"dry_run":true}}"#
            ),
        );
        assert!(!tolerant.is_error, "{}", tolerant.model_text);
        assert!(
            tolerant
                .model_text
                .starts_with("edit dry_run files=1 edits=1 note=empty_form_ignored\n"),
            "{}",
            tolerant.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
        assert!(preview.file_states.is_empty());
        assert!(matches!(
            preview.ui_payload,
            Some(qq_protocol::ToolCallDisplay::Diff { .. })
        ));
        assert_eq!(state.recorded("a.txt"), None);

        let applied = run_tool(
            &workspace,
            &state,
            "edit_file",
            &format!(
                r#"{{"edits":[{{"path":"a.txt","old":"beta","new":"gamma","if_hash":"h:{short}"}}]}}"#
            ),
        );
        assert!(!applied.is_error, "{}", applied.model_text);
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\ngamma\n"
        );
        assert_eq!(
            state.recorded("a.txt"),
            Some(content_hash(b"alpha\ngamma\n"))
        );

        // fuzzy=false refuses a whitespace drift the cascade would forgive.
        let strict = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"gamma   ","new":"delta"}],"fuzzy":false}"#,
        );
        assert!(
            strict.model_text.starts_with("edit 0: not_found"),
            "{}",
            strict.model_text
        );
        let forgiven = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"gamma   ","new":"delta"}]}"#,
        );
        assert!(!forgiven.is_error, "{}", forgiven.model_text);
        assert!(
            forgiven.model_text.contains(" via=line_trimmed"),
            "{}",
            forgiven.model_text
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_rename_failure_midway_reports_partial_apply_and_records_what_landed() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
        fs::create_dir(directory.path().join("locked")).unwrap();
        fs::write(directory.path().join("locked/b.txt"), "beta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"a.txt"}"#);
        run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"locked/b.txt"}"#,
        );
        // The second file's directory refuses new entries, so its temp file
        // cannot be created after the first file has already been renamed.
        fs::set_permissions(
            directory.path().join("locked"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let result = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"alpha","new":"ALPHA"},{"path":"locked/b.txt","old":"beta","new":"BETA"}]}"#,
        );
        fs::set_permissions(
            directory.path().join("locked"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(result.is_error);
        assert!(
            result
                .model_text
                .starts_with("partial_apply: applied=[a.txt] failed=locked/b.txt"),
            "{}",
            result.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("locked/b.txt")).unwrap(),
            "beta\n"
        );
        // The applied file's new hash is recorded so a retry of the rest
        // does not trip the staleness guard on it.
        assert_eq!(state.recorded("a.txt"), Some(content_hash(b"ALPHA\n")));
    }

    #[test]
    fn edits_carry_their_diff_as_a_ui_payload_never_as_model_text() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"a.txt"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma"}]}"#,
        );
        assert!(!edited.is_error, "{}", edited.model_text);
        assert_eq!(
            edited.model_text,
            format!(
                "edit ok files=1 edits=1\na.txt h:{} L2 -1+1\n",
                &content_hash(b"alpha\ngamma\n")[..12]
            )
        );
        match edited.ui_payload {
            Some(qq_protocol::ToolCallDisplay::Diff { path, diff }) => {
                assert_eq!(path, "a.txt");
                assert_eq!(
                    diff,
                    "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+gamma\n"
                );
            }
            other => panic!("expected a diff payload, got {other:?}"),
        }

        let written = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"b.txt","content":"one\n"}"#,
        );
        assert!(!written.is_error);
        assert!(matches!(
            written.ui_payload,
            Some(qq_protocol::ToolCallDisplay::Diff { .. })
        ));

        // A failed edit carries no payload: nothing was applied.
        let failed = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"missing","new":"x"}]}"#,
        );
        assert!(failed.is_error);
        assert!(failed.ui_payload.is_none());
        assert!(
            run_tool(&workspace, &state, "read_file", r#"{"path":"a.txt"}"#)
                .ui_payload
                .is_none()
        );
    }

    #[test]
    fn secrets_in_results_are_masked_at_the_boundary() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(".env"),
            "AWS_KEY=AKIAIOSFODNN7EXAMPLE\nDB_PASSWORD=hunter2hunter2\nPORT=$PORT\n",
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let read = run_tool(&workspace, &state, "read_file", r#"{"path":".env"}"#);
        assert!(!read.is_error);
        assert!(read.model_text.starts_with("read .env L1-3/3 h:"));
        assert!(read.model_text.ends_with(
            "\n1\tAWS_KEY=[masked:aws_key]\n2\tDB_PASSWORD=[masked:credential]\n3\tPORT=$PORT\n"
        ));
        // The hash is of the file, not of the rendering.
        assert_eq!(
            read.file_states[0].hash,
            content_hash(b"AWS_KEY=AKIAIOSFODNN7EXAMPLE\nDB_PASSWORD=hunter2hunter2\nPORT=$PORT\n")
        );

        // Hidden files are searched only on request; masking still applies.
        let found = run_tool(
            &workspace,
            &state,
            "search",
            r#"{"query":"AKIA","include_ignored":true}"#,
        );
        assert!(
            found
                .model_text
                .contains(".env\nL1: AWS_KEY=[masked:aws_key]"),
            "{}",
            found.model_text
        );
        assert!(!found.model_text.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[cfg(unix)]
    async fn run_shell_tool(
        workspace: Workspace,
        arguments: &'static str,
        cancelled: RunCancellation,
        output: Option<mpsc::Sender<String>>,
    ) -> ToolOutput {
        execute(
            workspace,
            Arc::new(FileState::default()),
            "shell".to_owned(),
            arguments.to_owned(),
            cancelled,
            output,
            ToolTasks::default(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
        )
        .await
    }

    #[tokio::test]
    async fn dropped_write_waiter_drains_the_actual_atomic_apply() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tasks = ToolTasks::default();
        let cancelled = RunCancellation::new();
        let (entered, release) = hold_tool_apply(workspace.path());
        let mut execution = Box::pin(execute(
            workspace,
            Arc::new(FileState::default()),
            "write_file".to_owned(),
            r#"{"path":"result.txt","content":"committed locally"}"#.to_owned(),
            cancelled.clone(),
            None,
            tasks.clone(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
        ));
        assert!(futures_util::poll!(execution.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap();
        drop(execution);

        let mut abandoned_drain = Box::pin(tasks.drain());
        assert!(futures_util::poll!(abandoned_drain.as_mut()).is_pending());
        let mut drain = Box::pin(tasks.drain());
        assert!(futures_util::poll!(drain.as_mut()).is_pending());
        drop(abandoned_drain);
        assert!(!directory.path().join("result.txt").exists());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("result.txt")).unwrap(),
            "committed locally"
        );
        assert!(!cancelled.is_cancelled());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_shell_waiter_is_reaped_before_drain_with_full_live_output() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tasks = ToolTasks::default();
        let cancelled = RunCancellation::new();
        let (output, mut chunks) = mpsc::channel::<String>(1);
        let mut execution = Box::pin(execute(
            workspace,
            Arc::new(FileState::default()),
            "shell".to_owned(),
            r#"{"command":"echo pid:$$; while [ ! -e start-output ]; do sleep 0.01; done; while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#.to_owned(),
            cancelled.clone(),
            Some(output),
            tasks.clone(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
        ));
        assert!(futures_util::poll!(execution.as_mut()).is_pending());
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), chunks.recv())
            .await
            .unwrap()
            .unwrap();
        let pid = parse_marked_pid(&first);
        // Do not spend the live-output budget before the PID chunk is consumed.
        fs::write(directory.path().join("start-output"), "go").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while chunks.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(execution);
        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.drain())
            .await
            .unwrap()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        assert!(rustix::process::test_kill_process(pid).is_err());
        assert!(!cancelled.is_cancelled());
    }

    #[tokio::test]
    async fn dropped_durable_output_recording_is_not_a_clean_drain() {
        let tasks = ToolTasks::default();
        tasks
            .set_output_recorder(|_, _| std::future::pending())
            .unwrap();
        let lease = tasks.enter();
        let write = lease
            .output_record(qq_protocol::ToolCallId::generate().unwrap(), b"checkpoint")
            .unwrap();
        drop(write);
        drop(lease);
        assert_eq!(tasks.drain().await, Err(ToolDrainError::UnrecordedOutput));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_process_receipt_write_reaps_child_and_blocks_settlement() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tasks = ToolTasks::default();
        let (sender, spawned) = tokio::sync::oneshot::channel();
        let sender = std::sync::Mutex::new(Some(sender));
        tasks
            .set_process_recorder(move |_, receipt| {
                sender
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(receipt.facts().pid)
                    .unwrap();
                async { Err(crate::sessions::SessionRuntimeError::Unavailable) }
            })
            .unwrap();
        let result = dispatch::execute_with_images(
            None,
            workspace,
            Arc::new(FileState::default()),
            "shell".to_owned(),
            PANIC_SHELL_ARGUMENTS.to_owned(),
            RunCancellation::new(),
            None,
            tasks.clone(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
            Arc::default(),
            Arc::default(),
            Some(qq_protocol::ToolCallId::generate().unwrap()),
        )
        .await;
        assert_eq!(result.disposition, ResultDisposition::Unobserved);
        assert!(
            result
                .model_text
                .contains("could not persist the command process")
        );
        assert_eq!(tasks.drain().await, Err(ToolDrainError::UnrecordedProcess));
        let pid =
            rustix::process::Pid::from_raw(i32::try_from(spawned.await.unwrap()).unwrap()).unwrap();
        assert!(rustix::process::test_kill_process(pid).is_err());
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn stalled_process_receipt_cannot_delay_cancellation_or_timeout() {
        for expire in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let workspace =
                Workspace::open(&std::fs::canonicalize(directory.path()).unwrap()).unwrap();
            let tasks = ToolTasks::default();
            let cancelled = RunCancellation::new();
            let (sender, spawned) = tokio::sync::oneshot::channel();
            let (release, gate) = tokio::sync::oneshot::channel();
            let hook = std::sync::Mutex::new(Some((sender, gate)));
            tasks
                .set_process_recorder(move |_, receipt| {
                    let (sender, gate) = hook.lock().unwrap().take().unwrap();
                    sender.send(receipt.facts().pid).unwrap();
                    async move {
                        gate.await.unwrap();
                        Ok(())
                    }
                })
                .unwrap();
            let mut arguments: serde_json::Value =
                serde_json::from_str(PANIC_SHELL_ARGUMENTS).unwrap();
            if expire {
                arguments["timeout_seconds"] = json!(120);
            }
            let execution = tokio::spawn(dispatch::execute_with_images(
                None,
                workspace,
                Arc::default(),
                "shell".to_owned(),
                arguments.to_string(),
                cancelled.clone(),
                None,
                tasks.clone(),
                Arc::default(),
                Arc::default(),
                Arc::default(),
                Arc::default(),
                Some(qq_protocol::ToolCallId::generate().unwrap()),
            ));
            let pid = tokio::time::timeout(std::time::Duration::from_secs(5), spawned)
                .await
                .unwrap()
                .unwrap();
            let stopped = shell::observe_shell_termination(pid);
            if expire {
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(121)).await;
                tokio::time::resume();
            } else {
                cancelled.cancel();
            }
            let terminated = tokio::time::timeout(std::time::Duration::from_secs(5), stopped).await;
            release.send(()).unwrap();
            let result = execution.await.unwrap();
            tasks.drain().await.unwrap();
            assert!(
                matches!(terminated, Ok(Ok(()))),
                "receipt persistence must not extend execution (timeout={expire})"
            );
            assert!(result.is_error);
        }
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn panicked_shell_task_never_reports_confirmed_quiescence() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let spawned = shell::observe_shell_spawn(workspace.path(), true);
        let tasks = ToolTasks::default();
        let result = execute(
            workspace,
            Arc::new(FileState::default()),
            "shell".to_owned(),
            PANIC_SHELL_ARGUMENTS.to_owned(),
            RunCancellation::new(),
            None,
            tasks.clone(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
        )
        .await;
        assert!(result.is_error);
        assert_eq!(tasks.check(), Err(ToolDrainError::UnconfirmedProcessExit));
        assert_eq!(
            tasks.drain().await,
            Err(ToolDrainError::UnconfirmedProcessExit)
        );
        assert_eq!(tasks.check(), Err(ToolDrainError::UnconfirmedProcessExit));
        assert_panicked_process_exits(spawned.await.unwrap()).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropped_windows_shell_waiter_kills_the_owned_process_before_drain() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let spawned = shell::observe_shell_spawn(workspace.path(), false);
        let tasks = ToolTasks::default();
        let (output, chunks) = mpsc::channel::<String>(1);
        let mut execution = Box::pin(execute(
            workspace,
            Arc::new(FileState::default()),
            "shell".to_owned(),
            r#"{"command":"for /L %i in (1,1,2147483647) do @echo waiting"}"#.to_owned(),
            RunCancellation::new(),
            Some(output),
            tasks.clone(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
        ));
        assert!(futures_util::poll!(execution.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(5), spawned)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while chunks.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(execution);
        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.drain())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            chunks.capacity(),
            0,
            "the output remained unread during drain"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_timeout_kills_the_owned_process() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tasks = ToolTasks::default();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute(
                workspace,
                Arc::new(FileState::default()),
                "shell".to_owned(),
                r#"{"command":"for /L %i in (1,1,2147483647) do @echo waiting","timeout_seconds":1}"#.to_owned(),
                RunCancellation::new(),
                None,
                tasks.clone(),
                Arc::new(crate::runtime::ShellPolicy::default()),
                Arc::default(),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert!(result.model_text.contains("timed out"));
        tasks.drain().await.unwrap();
    }

    /// Polls until the process is gone, failing the test after a generous
    /// deadline instead of asserting on a single sleep.
    #[cfg(unix)]
    async fn assert_process_exits(pid: u32) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let alive = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok());
            if !alive {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "process {pid} is still alive after the kill deadline"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    fn parse_marked_pid(content: &str) -> u32 {
        let start = content.find("pid:").expect("output must mark the pid") + "pid:".len();
        content[start..]
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse().ok())
            .expect("the marked pid must be numeric")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runs_commands_streams_output_and_reports_the_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let (sender, mut receiver) = mpsc::channel::<String>(16);

        let result = run_shell_tool(
            workspace.clone(),
            r#"{"command":"echo out; echo err 1>&2"}"#,
            RunCancellation::new(),
            Some(sender),
        )
        .await;

        assert_eq!(
            result.native_outcome,
            Some(crate::command_owner::NativeCommandOutcome::Exited { code: Some(0) })
        );
        assert!(!result.is_error, "unexpected error: {}", result.model_text);
        assert!(result.model_text.contains("out\n"), "{}", result.model_text);
        assert!(result.model_text.contains("err\n"), "{}", result.model_text);
        let header = result.model_text.lines().next().unwrap();
        assert!(header.starts_with("shell exit=0 elapsed="), "{header}");
        assert!(header.ends_with(" bytes=8"), "{header}");
        let mut streamed = String::new();
        while let Ok(chunk) = receiver.try_recv() {
            streamed.push_str(&chunk);
        }
        assert!(streamed.contains("out"), "streamed: {streamed}");
        assert!(streamed.contains("err"), "streamed: {streamed}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_nonzero_exits_are_tool_errors_that_carry_the_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_shell_tool(
            workspace.clone(),
            r#"{"command":"echo before failure; exit 7"}"#,
            RunCancellation::new(),
            None,
        )
        .await;

        assert_eq!(
            result.native_outcome,
            Some(crate::command_owner::NativeCommandOutcome::Exited { code: Some(7) })
        );
        assert!(result.is_error);
        assert!(
            result.model_text.contains("before failure"),
            "{}",
            result.model_text
        );
        assert!(
            result.model_text.starts_with("shell exit=7 "),
            "{}",
            result.model_text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_pins_the_working_directory_inside_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("sub")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let inside = run_shell_tool(
            workspace.clone(),
            r#"{"command":"pwd","cwd":"sub"}"#,
            RunCancellation::new(),
            None,
        )
        .await;
        assert!(!inside.is_error, "unexpected error: {}", inside.model_text);
        let expected = fs::canonicalize(directory.path().join("sub")).unwrap();
        assert_eq!(
            inside.model_text.lines().nth(1),
            Some(expected.to_str().unwrap()),
            "{}",
            inside.model_text
        );

        for arguments in [
            r#"{"command":"pwd","cwd":".."}"#,
            r#"{"command":"pwd","cwd":"/"}"#,
            r#"{"command":"pwd","cwd":"missing"}"#,
        ] {
            let escaped =
                run_shell_tool(workspace.clone(), arguments, RunCancellation::new(), None).await;
            assert!(escaped.is_error, "cwd escape accepted: {arguments}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_accepts_a_build_deadline_longer_than_ten_minutes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = run_shell_tool(
            workspace,
            r#"{"command":"printf build-finished","timeout_seconds":1800}"#,
            RunCancellation::new(),
            None,
        )
        .await;

        assert!(!result.is_error, "{}", result.model_text);
        assert!(result.model_text.contains("build-finished"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_running_build_survives_the_old_ten_minute_ceiling() {
        running_build_survives_clock_advance(
            r#"{"command":"printf ready; read token < release; printf build-finished","timeout_seconds":1800}"#,
            601,
        ).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_running_build_without_a_command_timeout_survives_two_minutes() {
        running_build_survives_clock_advance(
            r#"{"command":"printf ready; read token < release; printf build-finished"}"#,
            121,
        )
        .await;
    }

    #[tokio::test]
    async fn native_admission_freezes_task_and_explicit_command_deadlines_without_a_default_cap() {
        use crate::command_owner::{
            CommandOwnerError, CommandSnapshot, NativeCommand, NativeCommandFuture,
            NativeCommandService,
        };
        struct Capture(Arc<std::sync::Mutex<Vec<NativeCommand>>>);
        impl NativeCommandService for Capture {
            fn identity(&self) -> NativeCommandFuture<qq_protocol::StoreId> {
                Box::pin(async { Ok(qq_protocol::StoreId::from_bytes([7; 16])) })
            }
            fn acknowledge(&self, _: Vec<qq_protocol::ToolCallId>) -> NativeCommandFuture<()> {
                unreachable!()
            }
            fn cancel(&self, _: qq_protocol::ToolCallId) -> NativeCommandFuture<()> {
                unreachable!()
            }
            fn wait(
                &self,
                _: qq_protocol::ToolCallId,
                _: u64,
            ) -> NativeCommandFuture<CommandSnapshot> {
                unreachable!()
            }
            fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
                self.0.lock().unwrap().push(request);
                Box::pin(async { Err(CommandOwnerError::Full) })
            }
        }
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let task_deadline = now + 600_000;
        for name in ["shell", "exec"] {
            for (timeout, task) in [
                (None, Some(task_deadline)),
                (None, None),
                (Some(3600), Some(task_deadline)),
                (Some(1), Some(task_deadline)),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let workspace = Workspace::open(directory.path()).unwrap();
                let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
                let service = Arc::new(Capture(requests.clone()));
                let tasks = ToolTasks::with_native_commands(Some(service));
                tasks.set_native_recorder(|_, _| async { Ok(()) }).unwrap();
                tasks.shorten_native_deadline(task);
                let mut arguments = if name == "shell" {
                    json!({"command":"unused"})
                } else {
                    json!({"program":"unused","args":[]})
                };
                if let Some(timeout) = timeout {
                    arguments["timeout_seconds"] = json!(timeout);
                }
                let result = super::dispatch::execute_with_images(
                    None,
                    workspace,
                    Arc::new(FileState::default()),
                    name.to_owned(),
                    arguments.to_string(),
                    RunCancellation::new(),
                    None,
                    tasks.clone(),
                    Arc::new(crate::ShellPolicy::default()),
                    Arc::default(),
                    Arc::default(),
                    Arc::default(),
                    Some(qq_protocol::ToolCallId::generate().unwrap()),
                )
                .await;
                assert!(result.is_error);
                tasks.drain().await.unwrap();
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                let frozen = requests[0].not_after_ms;
                if timeout == Some(1) {
                    let frozen = frozen.expect("an explicit command timeout remains bounded");
                    assert!(frozen >= now + 1_000 && frozen < task_deadline);
                } else {
                    assert_eq!(
                        frozen, task,
                        "{name} timeout={timeout:?} must not invent a two-minute cap"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    async fn running_build_survives_clock_advance(arguments: &'static str, seconds: u64) {
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let release = directory.path().join("release");
        crate::tools::create_test_fifo(&release).await;
        // Keep both ends open until completion so an input written just
        // before the shell opens its reader cannot disappear with the writer.
        let gate = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(release)
            .unwrap();
        let (output, mut chunks) = mpsc::channel(8);
        let cancellation = RunCancellation::new();
        let execution = tokio::spawn(run_shell_tool(
            workspace,
            arguments,
            cancellation.clone(),
            Some(output),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut observed = String::new();
            while !observed.contains("ready") {
                observed.push_str(&chunks.recv().await.expect("command reached its gate"));
            }
        })
        .await
        .expect("command started");

        // Advance the deadline clock while the real native process waits on
        // explicit input, rather than making a long build fixture sleep.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(seconds)).await;
        tokio::time::resume();
        let mut writer = gate.try_clone().unwrap();
        tokio::task::spawn_blocking(move || {
            writer.write_all(b"continue\n").unwrap();
        })
        .await
        .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), execution).await;
        cancellation.cancel();
        let result = result.expect("released command completed").unwrap();
        assert!(!result.is_error, "{}", result.model_text);
        assert!(result.model_text.contains("build-finished"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_rejects_empty_commands_and_out_of_range_timeouts() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        for arguments in [
            r#"{"command":"   "}"#,
            r#"{"command":"true","timeout_seconds":0}"#,
            r#"{"command":"true","timeout_seconds":18446744073709551615}"#,
        ] {
            let result =
                run_shell_tool(workspace.clone(), arguments, RunCancellation::new(), None).await;
            assert!(result.is_error, "invalid arguments accepted: {arguments}");
            assert_eq!(
                result.disposition,
                ResultDisposition::Refused,
                "{arguments}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_children_start_from_a_cleared_environment_plus_allowlisted_names() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        // The test process's own environment stands in for the server's:
        // CARGO_MANIFEST_DIR is always set under `cargo test` and must not
        // reach a child unless requested and allowed.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let run = |arguments: &'static str, policy: crate::runtime::ShellPolicy| {
            let workspace = workspace.clone();
            async move {
                execute(
                    workspace,
                    Arc::new(FileState::default()),
                    "shell".to_owned(),
                    arguments.to_owned(),
                    RunCancellation::new(),
                    None,
                    ToolTasks::default(),
                    Arc::new(policy),
                    Arc::default(),
                )
                .await
            }
        };
        let default = crate::runtime::ShellPolicy::default();
        let allowing = crate::runtime::ShellPolicy {
            env_allowlist: std::sync::Arc::from(["CARGO_MANIFEST_DIR".to_owned()]),
            builtin_preference: crate::runtime::BuiltinPreference::Hint,
            ..Default::default()
        };

        let leaked = run(
            r#"{"command":"echo v=${CARGO_MANIFEST_DIR:-unset} path=${PATH:+set}"}"#,
            default.clone(),
        )
        .await;
        assert!(!leaked.is_error, "{}", leaked.model_text);
        assert!(
            leaked.model_text.contains("v=unset path=set"),
            "{}",
            leaked.model_text
        );

        // Not requested: not present, even though policy allows it.
        let unrequested = run(
            r#"{"command":"echo v=${CARGO_MANIFEST_DIR:-unset}"}"#,
            allowing.clone(),
        )
        .await;
        assert!(
            unrequested.model_text.contains("v=unset"),
            "{}",
            unrequested.model_text
        );
        // Requested and allowed: present.
        let requested = run(
            r#"{"command":"echo v=${CARGO_MANIFEST_DIR:-unset}","env":["CARGO_MANIFEST_DIR"]}"#,
            allowing.clone(),
        )
        .await;
        assert!(
            requested.model_text.contains("v=/"),
            "{}",
            requested.model_text
        );
        // Requested but not allowed: a typed refusal, nothing runs.
        let refused = run(
            r#"{"command":"echo v=$CARGO_MANIFEST_DIR","env":["CARGO_MANIFEST_DIR"]}"#,
            default.clone(),
        )
        .await;
        assert!(refused.is_error);
        assert!(
            refused
                .model_text
                .starts_with("env_not_allowed: CARGO_MANIFEST_DIR"),
            "{}",
            refused.model_text
        );
        let invalid = run(r#"{"command":"true","env":["1BAD"]}"#, allowing.clone()).await;
        assert!(
            invalid.model_text.starts_with("invalid_env"),
            "{}",
            invalid.model_text
        );
        // Base names never need listing.
        let base = run(
            r#"{"command":"echo h=${HOME:+set}","env":["HOME"]}"#,
            default,
        )
        .await;
        assert!(base.model_text.contains("h=set"), "{}", base.model_text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_runs_an_exact_argv_without_shell_interpretation() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a b.txt"), "spaced\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let run = |arguments: &str| {
            let workspace = workspace.clone();
            let arguments = arguments.to_owned();
            async move {
                execute(
                    workspace,
                    Arc::new(FileState::default()),
                    "exec".to_owned(),
                    arguments,
                    RunCancellation::new(),
                    None,
                    ToolTasks::default(),
                    Arc::new(crate::runtime::ShellPolicy::default()),
                    Arc::default(),
                )
                .await
            }
        };
        // Words are argv, not shell: a space in a filename needs no quoting
        // and `$HOME`, `*`, and `|` are literal characters.
        let literal =
            run(r#"{"program":"printf","args":["%s|%s|%s\n","$HOME","a b.txt","*"]}"#).await;
        assert!(!literal.is_error, "{}", literal.model_text);
        assert!(
            literal.model_text.starts_with("exec exit=0 elapsed="),
            "{}",
            literal.model_text
        );
        assert!(
            literal.model_text.contains("\n$HOME|a b.txt|*\n"),
            "{}",
            literal.model_text
        );
        let spaced = run(r#"{"program":"cat","args":["a b.txt"]}"#).await;
        assert!(
            spaced.model_text.contains("\nspaced\n"),
            "{}",
            spaced.model_text
        );
        // stdin reaches the program and the write end closes.
        let fed = run(r#"{"program":"tr","args":["a-z","A-Z"],"stdin":"hello\n"}"#).await;
        assert!(fed.model_text.contains("\nHELLO\n"), "{}", fed.model_text);
        // Failures are exit codes with the header, like shell.
        let failed = run(r#"{"program":"false"}"#).await;
        assert!(failed.is_error);
        assert!(
            failed.model_text.starts_with("exec exit=1 "),
            "{}",
            failed.model_text
        );
        let missing = run(r#"{"program":"definitely-not-a-program-qq"}"#).await;
        assert!(missing.is_error);
        assert!(
            missing
                .model_text
                .starts_with("could not start the command"),
            "{}",
            missing.model_text
        );
        for (arguments, code) in [
            (r#"{"program":""}"#, "invalid_program"),
            (
                r#"{"program":"true","args":["x"],"stdin":"y","env":["1x"]}"#,
                "invalid_env",
            ),
            (r#"{"program":"true","cwd":"../"}"#, "path"),
        ] {
            let bad = run(arguments).await;
            assert!(bad.is_error, "{arguments}");
            assert!(
                bad.model_text.contains(code),
                "{arguments}: {}",
                bad.model_text
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_hints_at_the_built_in_for_the_first_program_unless_off() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let run = |policy: crate::runtime::BuiltinPreference| {
            let workspace = workspace.clone();
            async move {
                execute(
                    workspace,
                    Arc::new(FileState::default()),
                    "shell".to_owned(),
                    r#"{"command":"cat a.txt"}"#.to_owned(),
                    RunCancellation::new(),
                    None,
                    ToolTasks::default(),
                    Arc::new(crate::runtime::ShellPolicy {
                        env_allowlist: std::sync::Arc::from([]),
                        builtin_preference: policy,
                        ..Default::default()
                    }),
                    Arc::default(),
                )
                .await
            }
        };
        let hinted = run(crate::runtime::BuiltinPreference::Hint).await;
        assert!(!hinted.is_error);
        assert!(
            hinted.model_text.contains("alpha\n"),
            "{}",
            hinted.model_text
        );
        assert!(
            hinted.model_text.ends_with("hint: use read_file instead of cat; it is bounded, ignore-aware, and needs no approval\n"),
            "{}",
            hinted.model_text
        );
        let quiet = run(crate::runtime::BuiltinPreference::Off).await;
        assert!(!quiet.model_text.contains("hint:"), "{}", quiet.model_text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_kills_the_whole_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        // The command starts a background child and then blocks: the timeout
        // must kill the child too, not just the immediate `sh`.
        let result = run_shell_tool(
            workspace.clone(),
            r#"{"command":"sleep 300 & echo pid:$!; wait","timeout_seconds":1}"#,
            RunCancellation::new(),
            None,
        )
        .await;

        assert!(result.is_error);
        assert!(
            result.model_text.contains("timed out"),
            "{}",
            result.model_text
        );
        assert_process_exits(parse_marked_pid(&result.model_text)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saturated_live_output_never_masks_the_shell_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let (sender, _receiver) = mpsc::channel::<String>(1);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_shell_tool(
                workspace,
                r#"{"command":"while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done","timeout_seconds":1}"#,
                RunCancellation::new(),
                Some(sender),
            ),
        )
        .await
        .expect("a full live-output queue must not stall the shell deadline");

        assert!(result.is_error);
        assert!(
            result.model_text.contains("timed out"),
            "{}",
            result.model_text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saturated_live_output_never_masks_shell_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let cancelled = RunCancellation::new();
        let (sender, receiver) = mpsc::channel::<String>(1);
        let execution = tokio::spawn(run_shell_tool(
            workspace,
            r#"{"command":"while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#,
            cancelled.clone(),
            Some(sender),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while receiver.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the live-output queue must become saturated");
        cancelled.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution)
            .await
            .expect("cancellation must remain live with a full output queue")
            .unwrap();
        assert!(result.is_error);
        assert!(result.model_text.starts_with("shell exit=cancelled "));
        assert!(
            result
                .model_text
                .contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        );
        assert!(
            result
                .model_text
                .ends_with("tool execution was cancelled; its process group was killed")
        );
        assert!(result.model_text.len() <= shell::SHELL_BOUNDS.max_bytes);
        assert_eq!(result.disposition, ResultDisposition::Interrupted);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_cancellation_kills_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let cancelled = RunCancellation::new();
        let (sender, mut receiver) = mpsc::channel::<String>(16);

        let execution = tokio::spawn(run_shell_tool(
            workspace.clone(),
            r#"{"command":"sleep 300 & echo pid:$!; wait"}"#,
            cancelled.clone(),
            Some(sender),
        ));
        // The first live chunk proves the command is running and carries the
        // background child's pid.
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
            .await
            .expect("the running command must stream its first chunk")
            .expect("the delta channel must be open while the command runs");
        cancelled.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution)
            .await
            .expect("cancellation must stop the command promptly")
            .unwrap();
        assert!(result.is_error);
        assert!(
            result.model_text.contains("cancelled"),
            "{}",
            result.model_text
        );
        assert_process_exits(parse_marked_pid(&chunk)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_output_is_truncated_head_and_tail_at_the_budget() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        // Pure-shell loop producing well over the 16 KiB model bound (and the
        // 128 KiB capture) with distinct head and tail lines.
        let result = run_shell_tool(
            workspace.clone(),
            r#"{"command":"i=0; while [ $i -lt 40000 ]; do echo line-$i; i=$((i+1)); done"}"#,
            RunCancellation::new(),
            None,
        )
        .await;

        assert!(!result.is_error, "unexpected error: {}", result.model_text);
        let mut lines = result.model_text.lines();
        let header = lines.next().unwrap();
        assert!(header.starts_with("shell exit=0 elapsed="), "{header}");
        // 40 000 lines of `line-N\n`: the header counts every byte written.
        assert!(header.ends_with(" bytes=428890"), "{header}");
        assert_eq!(lines.next(), Some("line-0"), "head was not kept");
        assert!(
            result.model_text.ends_with("line-39999\n"),
            "tail was not kept"
        );
        assert_eq!(
            result.model_text.matches(MARKER_PREFIX).count(),
            1,
            "{}",
            result.model_text
        );
        assert!(result.model_text.contains("lines omitted"));
        assert!(result.model_text.len() <= SHELL_BOUNDS.max_bytes);
        // Every kept line is whole.
        for line in result.model_text.lines().skip(1) {
            assert!(
                line.starts_with("line-") || line.starts_with(MARKER_PREFIX),
                "partial line kept: {line:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_identity_survives_the_actual_capture_and_dispatch_path() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let first = run_shell_tool(
            workspace.clone(),
            r#"{"command":"printf '%070000dX%070000d' 0 0"}"#,
            RunCancellation::new(),
            None,
        )
        .await;
        let second = run_shell_tool(
            workspace.clone(),
            r#"{"command":"printf '%070000dY%070000d' 0 0"}"#,
            RunCancellation::new(),
            None,
        )
        .await;
        assert!(!first.is_error && !second.is_error);
        assert_eq!(
            first.model_text.split_once('\n').unwrap().1,
            second.model_text.split_once('\n').unwrap().1
        );
        assert_ne!(
            first.result_digest, second.result_digest,
            "the changed middle byte was dropped by capture"
        );
        let stdout = run_shell_tool(
            workspace.clone(),
            r#"{"command":"printf same"}"#,
            RunCancellation::new(),
            None,
        )
        .await;
        let stderr = run_shell_tool(
            workspace.clone(),
            r#"{"command":"printf same >&2"}"#,
            RunCancellation::new(),
            None,
        )
        .await;
        let again = run_shell_tool(
            workspace,
            r#"{"command":"printf same"}"#,
            RunCancellation::new(),
            None,
        )
        .await;
        assert_ne!(stdout.result_digest, stderr.result_digest);
        assert_eq!(stdout.result_digest, again.result_digest);
    }

    #[test]
    fn bounded_capture_keeps_head_and_tail_and_counts_omitted_bytes() {
        let mut capture = BoundedCapture::new(8);
        capture.push(b"abcd");
        assert_eq!(capture.into_output(), "abcd");

        let mut capture = BoundedCapture::new(8);
        capture.push(b"abcd");
        capture.push(b"efgh");
        assert_eq!(capture.into_output(), "abcdefgh");

        let mut capture = BoundedCapture::new(8);
        capture.push(b"abcdefgh");
        capture.push(b"ij");
        capture.push(b"klmnop");
        // Head keeps the first 4 bytes, the rolling tail keeps the last 4.
        assert_eq!(
            capture.into_output(),
            "abcd\n…[qq: 8 bytes not captured]…\nmnop"
        );
    }

    #[test]
    fn edit_and_write_reject_containment_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim.txt"), "untouched\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        for arguments in [
            r#"{"edits":[{"path":"../victim.txt","old":"untouched","new":"changed"}]}"#,
            r#"{"edits":[{"path":"/etc/hosts","old":"localhost","new":"changed"}]}"#,
        ] {
            let result = run_tool(&workspace, &state, "edit_file", arguments);
            assert!(result.is_error, "escape accepted: {arguments}");
        }
        for arguments in [
            r#"{"path":"../victim.txt","content":"changed\n"}"#,
            r#"{"path":"/tmp/qq-escape.txt","content":"changed\n"}"#,
            r#"{"path":"..","content":"changed\n"}"#,
        ] {
            let result = run_tool(&workspace, &state, "write_file", arguments);
            assert!(result.is_error, "escape accepted: {arguments}");
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
            let through_symlink = run_tool(
                &workspace,
                &state,
                "write_file",
                r#"{"path":"outside/victim.txt","content":"changed\n"}"#,
            );
            assert!(through_symlink.is_error);
            std::os::unix::fs::symlink(
                outside.path().join("victim.txt"),
                directory.path().join("link.txt"),
            )
            .unwrap();
            let onto_symlink = run_tool(
                &workspace,
                &state,
                "edit_file",
                r#"{"edits":[{"path":"link.txt","old":"untouched","new":"changed"}]}"#,
            );
            assert!(onto_symlink.is_error);
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
            "untouched\n"
        );
    }
}

#[cfg(all(test, not(windows)))]
#[path = "tools/windows_owner/arguments.rs"]
mod windows_arguments;
