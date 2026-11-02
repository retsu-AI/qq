use super::*;

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
    assert!(result.model_text.len() <= crate::tools::read::READ_BOUNDS.max_bytes);
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
    assert!(result.model_text.len() <= crate::tools::search::SEARCH_BOUNDS.max_bytes);
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
    assert!(crate::tools::search::Cursor::decode(encoded).is_some());
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
    let cursor =
        crate::tools::search::Cursor::decode(encoded).expect("first page cursor is usable");
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
        &json!({"query":"needle","limit":25,"max_per_file":50,"include":["**/*.txt"]}).to_string(),
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
