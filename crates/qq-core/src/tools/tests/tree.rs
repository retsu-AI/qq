use super::*;

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

    for index in 0..=crate::tools::tree::MAX_ENTRIES {
        fs::write(directory.path().join(format!("entry-{index}")), "").unwrap();
    }
    let result = run_tool(
        &workspace,
        &FileState::default(),
        "tree",
        &format!(
            r#"{{"path":".","depth":1,"limit":{}}}"#,
            crate::tools::tree::MAX_ENTRIES
        ),
    );
    assert!(!result.is_error);
    assert!(
        result.model_text.starts_with(&format!(
            "tree . depth=1 entries={}/{} ",
            crate::tools::tree::MAX_ENTRIES,
            crate::tools::tree::MAX_ENTRIES + 2
        )),
        "{}",
        result.model_text.lines().next().unwrap()
    );
    assert!(result.model_text.contains("2 more entries; raise limit"));

    let unfiltered = run_tool(
        &workspace,
        &FileState::default(),
        "tree",
        &format!(
            r#"{{"path":".","depth":1,"limit":{},"glob":""}}"#,
            crate::tools::tree::MAX_ENTRIES
        ),
    );
    let (header, body) = unfiltered.model_text.split_once('\n').unwrap();
    let (expected_header, expected_body) = result.model_text.split_once('\n').unwrap();
    assert_eq!(header, format!("{expected_header} note=empty_glob_ignored"));
    assert_eq!(body, expected_body);
}
