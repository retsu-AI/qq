use super::*;

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
        cap_std::fs::Dir::open_ambient_dir(directory.path(), cap_std::ambient_authority()).unwrap();
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
        cap_std::fs::Dir::open_ambient_dir(directory.path(), cap_std::ambient_authority()).unwrap();
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
    assert!(path.len() > crate::tools::read::READ_BOUNDS.max_bytes);
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
    for (path, mut offset) in [("capped", 40_000), ("single", 1)] {
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
fn read_file_stops_at_its_byte_budget_and_names_the_resume_offset() {
    let directory = tempfile::tempdir().unwrap();
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

    let tail = run_tool(
        &workspace,
        &state,
        "read_file",
        r#"{"path":"n.txt","ranges":["118-500"]}"#,
    );
    assert!(tail.model_text.starts_with("read n.txt L118-120/120 h:"));
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
    assert_eq!(state.recorded("n.txt"), Some(hash));
}

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
    assert!(serde_json::to_string(&result.model_text).unwrap().len() <= MAX_MODEL_TEXT_BYTES + 2);
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
