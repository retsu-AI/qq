use super::*;

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
            "invalid_edit: old and new are identical in every edit",
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
