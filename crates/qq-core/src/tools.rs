mod dispatch;
mod edit;
mod lang;
pub mod output;
mod read;
mod search;
mod shell;
mod specs;
mod tree;
mod walk;
mod write;

#[cfg(test)]
pub(crate) use dispatch::test_executions_started;
pub(crate) use dispatch::{ToolDrainError, ToolOutput, ToolTasks, bounded_result, execute};
#[cfg(test)]
pub(crate) use edit::hold_tool_apply;
#[cfg(test)]
pub(crate) use output::MAX_MODEL_TEXT_BYTES;
pub(crate) use output::{TurnOutputBudget, header_line};
pub(crate) use specs::{
    MAX_SPAWN_AGENT_SCHEMA_BYTES, SPAWN_AGENT_TOOL, SpawnAgentArgs, alias_effect, spawn_agent_spec,
    static_tools,
};
#[cfg(test)]
pub(crate) use specs::{specs, test_tool_effect};

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
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
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
            &ToolCancellation::new(Arc::new(AtomicBool::new(false))),
        )
    }

    #[test]
    fn read_and_list_are_bounded_and_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("b.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(directory.path().join("a.txt"), "a").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let listed = run_tool(&workspace, &state, "list_dir", r#"{"path":"."}"#);
        assert_eq!(
            listed.model_text,
            "tree . depth=1 entries=2/2 files=2 dirs=0\na.txt 1  b.txt 14\n"
        );
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
        assert_eq!(read.model_text, "two\n");
        let update = read.file_state.unwrap();
        assert_eq!(update.path, "b.txt");
        assert_eq!(update.hash, content_hash(b"one\ntwo\nthree\n"));
        assert_eq!(state.recorded("b.txt"), Some(update.hash));
    }

    #[test]
    fn read_file_marks_oversized_results_as_truncated() {
        let directory = tempfile::tempdir().unwrap();
        // 200 lines of 1 KiB: within the read limit, over the byte ceiling.
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
        assert!(result.model_text.len() <= MAX_MODEL_TEXT_BYTES);
        assert!(result.model_text.starts_with(&line), "head was not kept");
        assert!(result.model_text.ends_with(&line), "tail was not kept");
        assert_eq!(result.model_text.matches(MARKER_PREFIX).count(), 1);
        assert!(result.model_text.contains("lines omitted"));
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
            result.model_text.starts_with("ab\n"),
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
        assert_eq!(result.model_text, "y");
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
            &ToolCancellation::new(Arc::new(AtomicBool::new(true))),
        );
        assert!(result.is_error);
        assert!(result.model_text.contains("cancelled"));

        for index in 0..=tree::MAX_ENTRIES {
            fs::write(directory.path().join(format!("entry-{index}")), "").unwrap();
        }
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "list_dir",
            r#"{"path":"."}"#,
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
            r#"{"path":"main.rs","old_string":"fn one() {}","new_string":"fn one() { start() }"}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.model_text);
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs")).unwrap(),
            "fn one() { start() }\nfn two() {}\n"
        );
        let update = edited.file_state.unwrap();
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
            r#"{"path":"main.rs","old_string":"fn two() {}","new_string":"fn two() { end() }"}"#,
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
            r#"{"path":"list.txt","old_string":"item","new_string":"entry","replace_all":true}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.model_text);
        assert!(edited.model_text.contains("3 occurrence"));
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
            ]
        );
        assert!(!specs.iter().any(|spec| spec.name() == SPAWN_AGENT_TOOL));
        assert!(!specs.iter().any(|spec| spec.name() == "list_dir"));
        assert_eq!(
            crate::runtime::tool_schema_measurement(&specs)
                .hash
                .to_string(),
            "40dece9e4ff88144d5b71e10ac3ce9b7f7685c975ca55b18158a6b9496b2fdd6"
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
        let schema = spec.input_schema();
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
        let schema = spec.input_schema();
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
        let schema = spec.input_schema();
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
            r#"{"path":"list.txt","old_string":"missing","new_string":"other"}"#,
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
            r#"{"path":"list.txt","old_string":"item","new_string":"entry"}"#,
        );
        assert!(ambiguous.is_error);
        assert!(
            ambiguous.model_text.contains("2 times")
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
            r#"{"path":"note.txt","old_string":"content","new_string":"changed"}"#,
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
            r#"{"path":"note.txt","old_string":"original","new_string":"edited"}"#,
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
            r#"{"path":"note.txt","old_string":"external change","new_string":"edited"}"#,
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
            r#"{"path":"shared.txt","old_string":"base","new_string":"winner"}"#,
        );
        assert!(!won.is_error, "unexpected error: {}", won.model_text);

        let lost = run_tool(
            &workspace,
            &loser,
            "edit_file",
            r#"{"path":"shared.txt","old_string":"base","new_string":"loser"}"#,
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
            r#"{"path":"shared.txt","old_string":"winner","new_string":"reconciled"}"#,
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
            r#"{"path":"run.sh","old_string":"echo one","new_string":"echo two"}"#,
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
        assert!(created.model_text.starts_with("Created"));
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/NOTES.md")).unwrap(),
            "first\n"
        );
        let update = created.file_state.unwrap();
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
        assert!(overwritten.model_text.starts_with("Wrote"));
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/NOTES.md")).unwrap(),
            "second\n"
        );

        let missing_parent = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"missing/NOTES.md","content":"first\n"}"#,
        );
        assert!(missing_parent.is_error);
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
            r#"{"path":"a.txt","old_string":"beta","new_string":"gamma"}"#,
        );
        assert!(!edited.is_error, "{}", edited.model_text);
        assert_eq!(edited.model_text, "Edited a.txt: replaced 1 occurrence(s).");
        match edited.ui_payload {
            Some(qq_protocol::ToolCallDisplay::Diff { path, diff }) => {
                assert_eq!(path, "a.txt");
                assert_eq!(diff, "- beta\n+ gamma\n");
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
            r#"{"path":"a.txt","old_string":"missing","new_string":"x"}"#,
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
        assert_eq!(
            read.model_text,
            "AWS_KEY=[masked:aws_key]\nDB_PASSWORD=[masked:credential]\nPORT=$PORT\n"
        );
        // The hash is of the file, not of the rendering.
        assert_eq!(
            read.file_state.unwrap().hash,
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
        cancelled: Arc<AtomicBool>,
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
        )
        .await
    }

    #[tokio::test]
    async fn dropped_write_waiter_drains_the_actual_atomic_apply() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tasks = ToolTasks::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (entered, release) = hold_tool_apply(workspace.path());
        let mut execution = Box::pin(execute(
            workspace,
            Arc::new(FileState::default()),
            "write_file".to_owned(),
            r#"{"path":"result.txt","content":"committed locally"}"#.to_owned(),
            Arc::clone(&cancelled),
            None,
            tasks.clone(),
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
        assert!(!cancelled.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_shell_waiter_is_reaped_before_drain_with_full_live_output() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tasks = ToolTasks::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (output, mut chunks) = mpsc::channel::<String>(1);
        let mut execution = Box::pin(execute(
            workspace,
            Arc::new(FileState::default()),
            "shell".to_owned(),
            r#"{"command":"echo pid:$$; while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#.to_owned(),
            Arc::clone(&cancelled),
            Some(output),
            tasks.clone(),
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
        assert!(!cancelled.load(Ordering::Acquire));
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
            Arc::new(AtomicBool::new(false)),
            None,
            tasks.clone(),
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
            Arc::new(AtomicBool::new(false)),
            Some(output),
            tasks.clone(),
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
                Arc::new(AtomicBool::new(false)),
                None,
                tasks.clone(),
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
            Arc::new(AtomicBool::new(false)),
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
            Arc::new(AtomicBool::new(false)),
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
            Arc::new(AtomicBool::new(false)),
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
            let escaped = run_shell_tool(
                workspace.clone(),
                arguments,
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .await;
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
            let result = run_shell_tool(
                workspace.clone(),
                arguments,
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .await;
            assert!(result.is_error, "invalid arguments accepted: {arguments}");
        }
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
            Arc::new(AtomicBool::new(false)),
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
                Arc::new(AtomicBool::new(false)),
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
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel::<String>(1);
        let execution = tokio::spawn(run_shell_tool(
            workspace,
            r#"{"command":"while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#,
            Arc::clone(&cancelled),
            Some(sender),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while receiver.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the live-output queue must become saturated");
        cancelled.store(true, Ordering::Release);

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
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, mut receiver) = mpsc::channel::<String>(16);

        let execution = tokio::spawn(run_shell_tool(
            workspace.clone(),
            r#"{"command":"sleep 300 & echo pid:$!; wait"}"#,
            Arc::clone(&cancelled),
            Some(sender),
        ));
        // The first live chunk proves the command is running and carries the
        // background child's pid.
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
            .await
            .expect("the running command must stream its first chunk")
            .expect("the delta channel must be open while the command runs");
        cancelled.store(true, Ordering::Release);

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
            Arc::new(AtomicBool::new(false)),
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
            r#"{"path":"../victim.txt","old_string":"untouched","new_string":"changed"}"#,
            r#"{"path":"/etc/hosts","old_string":"localhost","new_string":"changed"}"#,
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
                r#"{"path":"link.txt","old_string":"untouched","new_string":"changed"}"#,
            );
            assert!(onto_symlink.is_error);
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
            "untouched\n"
        );
    }
}
