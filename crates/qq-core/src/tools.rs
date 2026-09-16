pub(crate) mod ask;
mod dispatch;
mod edit;
pub(crate) mod fetch;
mod lang;
mod matching;
pub(crate) mod network;
pub mod output;
mod read;
mod search;
mod shell;
mod specs;
mod tree;
pub(crate) mod walk;
mod write;

#[cfg(test)]
pub(crate) use dispatch::test_executions_started;

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
    SpillRecord, ToolDrainError, ToolOutput, ToolTasks, bounded_result, execute,
};
#[cfg(test)]
pub(crate) use edit::hold_tool_apply;
#[cfg(test)]
pub(crate) use output::MAX_MODEL_TEXT_BYTES;
pub(crate) use output::{TurnOutputBudget, finalize_spill_marker, header_line};
pub(crate) use specs::{
    MAX_SPAWN_AGENT_SCHEMA_BYTES, SPAWN_AGENT_TOOL, SpawnAgentArgs, spawn_agent_spec, static_tools,
};
#[cfg(test)]
pub(crate) use specs::{specs, test_tool_effect};

#[cfg(test)]
use crate::RunCancellation;
#[cfg(test)]
use crate::workspace::{FileState, Workspace, content_hash};
#[cfg(test)]
use dispatch::{ToolCancellation, execute_blocking};
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

#[cfg(all(test, any(unix, windows)))]
pub(crate) use shell::observe_shell_spawn;

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
    // This observes only the fixture's numeric PID. It cannot turn an
    // unconfirmed production cleanup result into confirmed quiescence.
    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         $observedProcess = Get-Process -Id {pid} -ErrorAction SilentlyContinue; \
         if ($null -eq $observedProcess) {{ exit 0 }}; \
         if ($observedProcess.WaitForExit(10000)) {{ exit 0 }}; \
         [Console]::Error.WriteLine('the owned shell did not exit after panic'); exit 1"
    );
    let observed = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the Windows process-exit observer must finish")
    .expect("PowerShell must observe the panicked fixture's process");
    assert!(
        observed.status.success(),
        "the owned process must still exit after panic: {}",
        String::from_utf8_lossy(&observed.stderr)
    );
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
            format!("read b.txt L2/3 h:{}\n2\ttwo\n", &hash[..12])
        );
        let update = read.file_states.into_iter().next().unwrap();
        assert_eq!(update.path, "b.txt");
        assert_eq!(update.hash, hash);
        assert_eq!(state.recorded("b.txt"), Some(update.hash));
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
            (
                r#"{"path":"n.txt","ranges":["1"],"offset":2}"#,
                "invalid_ranges",
            ),
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
            "0cbb5795a3387f58ef002a09270201e49eef05af56e06f47087fa6f453439189"
        );
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
                    context_window: Some(400_000),
                    max_output_tokens: None,
                    relative_cost_permille: Some(150),
                },
                qq_protocol::DelegationRosterEntry {
                    route: "anthropic/strong".to_owned(),
                    role: qq_protocol::DelegationRole::Strong,
                    note: None,
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

        let edit = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"note.txt","old":"content","new":"changed"}]}"#,
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
                r#"{"edits":[{"path":"a.txt","old":"alpha","new":"b","if_hash":"h:000000000000"}]}"#,
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
            r#"{"command":"echo pid:$$; while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#.to_owned(),
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
    async fn shell_rejects_empty_commands_and_out_of_range_timeouts() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        for arguments in [
            r#"{"command":"   "}"#,
            r#"{"command":"true","timeout_seconds":0}"#,
            r#"{"command":"true","timeout_seconds":601}"#,
        ] {
            let result =
                run_shell_tool(workspace.clone(), arguments, RunCancellation::new(), None).await;
            assert!(result.is_error, "invalid arguments accepted: {arguments}");
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
        assert_eq!(result.model_text, "tool execution was cancelled");
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
