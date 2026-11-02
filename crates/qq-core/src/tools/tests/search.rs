use super::*;

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
fn names_discovery_reports_safe_instruction_links_and_blocks_bad_links() {
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
fn instruction_link_pages_resume_without_following_directory_links() {
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
fn names_scan_budget_advances_past_nonmatching_links() {
    crate::tools::search::with_scan_entries(128, || {
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
fn names_scan_budget_advances_past_empty_directories() {
    crate::tools::search::with_scan_entries(128, || {
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
fn cursor_path_boundary_round_trips_without_header_clipping() {
    for path in ["a".repeat(4_096), "界".repeat(1_365) + "/"] {
        let cursor = crate::tools::search::Cursor {
            path,
            line: u32::MAX,
        };
        assert_eq!(
            crate::tools::search::Cursor::decode(&cursor.encode()),
            Some(cursor)
        );
    }
    let cursor = crate::tools::search::Cursor {
        path: "a".repeat(4_097),
        line: 0,
    };
    assert!(crate::tools::search::Cursor::decode(&cursor.encode()).is_none());

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
        let args = serde_json::json!({"query":"x", "mode":"names", "cursor":cursor}).to_string();
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
            assert!(crate::tools::search::Cursor::decode(encoded).is_some());
        } else {
            break;
        }
    }
    assert_eq!(found.len(), 10);
    assert!(cursor.is_none());
}

#[test]
fn long_nonmatching_prefix_releases_its_cursor_reservation() {
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
fn unrepresentable_match_returns_an_explicit_budget_error() {
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
fn long_directory_watermark_decodes_and_reaches_later_instructions() {
    crate::tools::search::with_scan_entries(128, || {
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
fn names_discover_file_ignored_instructions_in_visible_directories() {
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
fn instruction_discovery_depth_windows_cover_boundaries() {
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
fn names_and_content_search_reach_an_explicit_ignored_worktree() {
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
    let decoded = crate::tools::search::Cursor::decode(cursor).unwrap();
    assert_eq!(
        decoded,
        crate::tools::search::Cursor {
            path: "c.txt".to_owned(),
            line: 5
        }
    );

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

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let arguments = match &cursor {
            Some(cursor) => {
                format!(r#"{{"query":"needle","limit":20,"max_per_file":50,"cursor":"{cursor}"}}"#)
            }
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
    assert!(escaped_len(&result.model_text) <= crate::tools::search::SEARCH_BOUNDS.max_bytes);
}

#[cfg(unix)]
#[test]
fn a_fifo_or_oversized_ignore_file_is_skipped_not_read() {
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
