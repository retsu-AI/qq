use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{
    AccountingTotal, CommandRequest, ModelSelection, RunId, SessionAccounting, SessionCommand,
    SessionEvent, SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus, SessionSummary,
    WorkspaceSnapshot,
};

use super::*;
use crate::{
    ClientRequest, ClientUpdate, ModelOption, TuiOptions,
    commands::Command,
    fixtures::{self, SESSION},
    render::{code_keyword, success, surface, surface_color},
    theme::Palette,
    view::markdown::{code_panel_row, tests::style_of},
};

fn completed_message(byte: u8, output: String) -> MessageSnapshot {
    MessageSnapshot {
        turn_ordinal: 0,
        ..fixtures::message(MessageId::from_bytes([byte; 16]), SESSION, &output)
    }
}

fn app_with_messages(count: u8) -> App {
    let summary = fixtures::session_summary(SESSION);
    let mut app = App::new(TuiOptions::default());
    app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
        sessions: vec![summary.clone()],
        focused: Some(SessionSnapshot {
            messages: (0..count)
                .map(|row| completed_message(row + 1, format!("row {row}")))
                .collect(),
            ..fixtures::session_snapshot(summary)
        }),
        ..fixtures::workspace_snapshot()
    }));
    app
}

/// Row text with runs of spaces collapsed so tool rows (a fixed subject
/// column followed by a right-aligned metric) compare without counting
/// padding.
fn squash(row: &str) -> String {
    let mut out = String::with_capacity(row.len());
    let mut spaces = 0;
    for character in row.chars() {
        if character == ' ' {
            spaces += 1;
            if spaces <= 1 {
                out.push(character);
            }
        } else {
            spaces = 0;
            out.push(character);
        }
    }
    out.trim_end().to_owned()
}

fn squashed_rows(frame: &[Line]) -> Vec<String> {
    frame_rows(frame).iter().map(|row| squash(row)).collect()
}

fn frame_text(frame: &[Line]) -> String {
    frame
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn frame_rows(frame: &[Line]) -> Vec<String> {
    frame
        .iter()
        .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
        .collect()
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line> {
    let mut renderer = FrameRenderer::default();
    let body = renderer.transcript(app, width);
    body.viewport(app, body.rows, 0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn completed_messages_render_plain_then_upgrade_to_highlighted() {
    let mut renderer = FrameRenderer::default();
    let mut message = completed_message(1, "```rust\nlet x = 1;\n```".to_owned());
    message.state = MessageState::Streaming;

    let streaming = renderer.render_message(&message, 40);

    // Re-rendered every frame while streaming: plain panel, no cache.
    assert_eq!(style_of(&streaming, "let"), Some(surface(normal())));
    assert!(renderer.markdown().is_empty());
    assert_eq!(renderer.highlighter.in_flight(), 0);

    // Completion caches a plain layout immediately and schedules
    // highlighting off the render path.
    message.state = MessageState::Complete;
    let complete = renderer.render_message(&message, 40);
    assert_eq!(style_of(&complete, "let"), Some(surface(normal())));
    assert!(renderer.markdown().contains_key(&message.id));
    assert_eq!(renderer.highlighter.in_flight(), 1);

    let highlighted = renderer.highlighter.next().await;
    assert!(renderer.apply_highlight(highlighted));
    let upgraded = renderer.render_message(&message, 40);
    assert_eq!(style_of(&upgraded, "let"), Some(surface(code_keyword())));

    // A stale result (different width) is dropped, not installed.
    let stale = Highlighted {
        key: HighlightKey {
            message_id: message.id,
            width: 41,
            output_bytes: message.output.len(),
            refusal_bytes: 0,
            loaded_through: 0,
        },
        lines: Vec::new(),
    };
    assert!(!renderer.apply_highlight(stale));
    assert_eq!(
        style_of(&renderer.render_message(&message, 40), "let"),
        Some(surface(code_keyword()))
    );
}

#[test]
fn prose_only_messages_do_not_request_highlighting() {
    let mut renderer = FrameRenderer::default();
    let message = completed_message(1, "plain **prose** without code".to_owned());
    renderer.render_message(&message, 40);
    assert_eq!(renderer.highlighter.in_flight(), 0);
}

#[test]
fn live_message_rendering_is_bounded_without_hiding_completed_output() {
    let mut renderer = FrameRenderer::default();
    let mut message = completed_message(
        1,
        format!(
            "BEGIN-LIVE-MESSAGE\n{}\nEND-LIVE-MESSAGE",
            "streaming row\n".repeat(MAX_LIVE_MARKDOWN_ROWS * 4)
        ),
    );
    message.state = MessageState::Streaming;

    let live = renderer.render_message(&message, 40);
    let live_text = frame_text(&live);

    assert!(live.len() <= MAX_LIVE_MARKDOWN_ROWS + 1);
    assert!(live_text.contains("earlier output remains"));
    assert!(!live_text.contains("BEGIN-LIVE-MESSAGE"));
    assert!(live_text.contains("END-LIVE-MESSAGE"));

    message.state = MessageState::Complete;
    let complete = renderer.render_message(&message, 40);
    let complete_text = frame_text(&complete);
    assert!(complete_text.contains("BEGIN-LIVE-MESSAGE"));
    assert!(complete_text.contains("END-LIVE-MESSAGE"));
}

fn tool_call_snapshot(
    byte: u8,
    name: &str,
    arguments: &str,
    state: ToolCallState,
    result: Option<&str>,
    is_error: bool,
) -> ToolCallSnapshot {
    ToolCallSnapshot {
        arguments: arguments.to_owned(),
        state,
        result: result.map(str::to_owned),
        is_error,
        ..fixtures::tool_call(ToolCallId::from_bytes([byte; 16]), SESSION, name)
    }
}

#[test]
fn transcript_renders_replayed_tool_activity_collapsed() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Completed,
        Some("contents"),
        false,
    )]);

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 30);
    let rows = frame_rows(&frame);

    assert!(
        rows.iter()
            .any(|row| squash(row).contains("● Read note.txt 1 line"))
    );
    assert!(!frame_text(&frame).contains("contents"));
}

#[test]
fn call_only_run_renders_before_its_first_assistant_message() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].role = MessageRole::User;
    session.tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Running,
        None,
        false,
    )]);

    let rows = frame_rows(&transcript_lines(&app, 100));

    assert!(
        rows.iter()
            .any(|row| squash(row).contains("Read note.txt") && row.contains("running"))
    );
}

#[test]
fn steering_rows_say_what_they_are_at_every_state() {
    let mut app = app_with_messages(4);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.status = SessionStatus::Running;
    let active_run_id = RunId::from_bytes([2; 16]);
    session.summary.active_run_id = Some(active_run_id);
    let messages = session.messages.as_mut().unwrap();
    for (index, (state, text)) in [
        (MessageState::Complete, "the model turn"),
        (MessageState::Queued, "pending steer"),
        (MessageState::Complete, "applied steer"),
        (MessageState::Cancelled, "late steer"),
    ]
    .into_iter()
    .enumerate()
    {
        messages[index].run_id = active_run_id;
        messages[index].state = state;
        messages[index].output = text.to_owned();
        if index > 0 {
            messages[index].role = MessageRole::User;
            messages[index].steering = true;
        }
    }

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 40);
    let rows = frame_rows(&frame);
    let header_before = |needle: &str| {
        let at = rows.iter().position(|row| row.contains(needle)).unwrap();
        rows[..at]
            .iter()
            .rev()
            .find(|row| row.contains("YOU"))
            .cloned()
            .unwrap()
    };
    assert!(header_before("pending steer").contains("steering  waiting for the next turn"));
    assert!(
        header_before("applied steer")
            .trim_end()
            .ends_with("steered")
    );
    assert!(header_before("late steer").contains("steering  run finished first"));
    // A steering row never shows the plain "queued" of a queued prompt,
    // which would read as a new run waiting its turn.
    assert!(!rows.iter().any(|row| row.contains("YOU  queued")));
}

#[test]
fn transcript_spacing_separates_blocks_and_doubles_before_prompts() {
    let mut app = app_with_messages(3);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].role = MessageRole::User;
    messages[2].role = MessageRole::User;
    session.tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Completed,
        Some("contents"),
        false,
    )]);

    let rows = squashed_rows(&transcript_lines(&app, 80));

    assert_eq!(
        rows,
        [
            " ▌ YOU",
            " ▌ row 0",
            "",
            " QQ",
            " row 1",
            "",
            " ● Read note.txt 1 line",
            "",
            "",
            " ▌ YOU",
            " ▌ row 2",
        ]
    );
}

#[test]
fn head_orphan_call_turns_render_before_the_runs_first_message() {
    let mut app = app_with_messages(2);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].role = MessageRole::User;
    messages[1].turn_ordinal = 2;
    let call = |byte, turn, name: &str, arguments: &str, result: &str| {
        let mut call = tool_call_snapshot(
            byte,
            name,
            arguments,
            ToolCallState::Completed,
            Some(result),
            false,
        );
        call.turn_ordinal = turn;
        call
    };
    // Arrival order is scrambled; rendering re-sorts by (turn, call).
    session.tool_calls = Some(vec![
        call(5, 2, "search", r#"{"query":"x"}"#, "No matches found.\n"),
        call(4, 1, "read_file", r#"{"path":"b.rs"}"#, "b\n"),
        call(3, 1, "read_file", r#"{"path":"a.rs"}"#, "a\n"),
    ]);

    let rows = squashed_rows(&transcript_lines(&app, 80));

    // The call-only turn 1 renders before the run's first message (turn
    // 2), so the transcript reads in execution order.
    assert_eq!(
        rows,
        [
            " ▌ YOU",
            " ▌ row 0",
            "",
            " ● Read a.rs 1 line",
            " ● Read b.rs 1 line",
            "",
            " QQ",
            " row 1",
            "",
            " ● Search \"x\" no matches",
        ]
    );
}

#[test]
fn consecutive_call_only_turns_merge_into_one_folded_group() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].turn_ordinal = 1;
    let calls = [(2, 1), (3, 1), (4, 2), (5, 3)]
        .into_iter()
        .map(|(byte, turn)| {
            let mut call = tool_call_snapshot(
                byte,
                "read_file",
                r#"{"path":"a.rs"}"#,
                ToolCallState::Completed,
                Some("a\n"),
                false,
            );
            call.turn_ordinal = turn;
            call
        })
        .collect::<Vec<_>>();
    session.tool_calls = Some(calls);

    // By default every call is a row, in one contiguous block.
    let rows = squashed_rows(&transcript_lines(&app, 80));
    assert_eq!(rows.len(), 7, "{rows:?}");
    assert!(rows[3..].iter().all(|row| row.contains("Read a.rs")));

    // Folded: the call-only turns 2 and 3 merge into turn 1's contiguous
    // call group, and the four quiet calls fold as one, not per turn.
    app.tool_detail = ToolDetail::Folded;
    let rows = squashed_rows(&transcript_lines(&app, 80));
    assert_eq!(rows, [" QQ", " row 0", "", " ▸ Read ×4 a.rs",]);
}

#[test]
fn completed_edit_results_color_diff_shaped_content_at_expanded_detail() {
    let diff_call = tool_call_snapshot(
        1,
        "edit_file",
        r#"{"path":"src/lib.rs"}"#,
        ToolCallState::Completed,
        Some("@@ -1 +1 @@\n-old\n+new\n context"),
        false,
    );
    let lines = render_tool_calls_simple(
        &[&diff_call],
        &HashMap::new(),
        SimpleDetail::Expanded,
        0,
        80,
        &|_, _| Vec::new(),
    );
    let style_of = |lines: &[Line], needle: &str| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of(&lines, "@@ -1 +1 @@"), Some(muted()));
    assert_eq!(style_of(&lines, "-old"), Some(diff_line_style("-")));
    assert_eq!(style_of(&lines, "+new"), Some(diff_line_style("+")));
    assert_eq!(style_of(&lines, " context"), Some(normal()));

    // Today's summary results are not diff-shaped and keep the raw style.
    let summary_call = tool_call_snapshot(
        2,
        "edit_file",
        r#"{"path":"src/lib.rs"}"#,
        ToolCallState::Completed,
        Some("Edited src/lib.rs: replaced 1 occurrence(s)."),
        false,
    );
    let lines = render_tool_calls_simple(
        &[&summary_call],
        &HashMap::new(),
        SimpleDetail::Expanded,
        0,
        80,
        &|_, _| Vec::new(),
    );
    assert_eq!(style_of(&lines, "Edited src/lib.rs"), Some(muted()));
}

#[test]
fn display_payload_diffs_replace_the_result_summary_at_expanded_detail() {
    let mut call = tool_call_snapshot(
        3,
        "edit_file",
        r#"{"path":"src/lib.rs"}"#,
        ToolCallState::Completed,
        Some("Edited src/lib.rs: replaced 1 occurrence(s)."),
        false,
    );
    call.display = Some(ToolCallDisplay::Diff {
        path: "src/lib.rs".to_owned(),
        diff: "- old line\n+ new line\n".to_owned(),
    });

    let lines = render_tool_calls_simple(
        &[&call],
        &HashMap::new(),
        SimpleDetail::Expanded,
        0,
        80,
        &|_, _| Vec::new(),
    );
    let style_of = |needle: &str| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of("- old line"), Some(diff_line_style("-")));
    assert_eq!(style_of("+ new line"), Some(diff_line_style("+")));
    // The payload renders instead of the raw summary sentence.
    assert!(style_of("replaced 1 occurrence").is_none());

    // Collapsed detail keeps the one-liner; the payload adds no rows.
    let lines = render_tool_calls_simple(
        &[&call],
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        80,
        &|_, _| Vec::new(),
    );
    assert_eq!(lines.len(), 1);
}

#[test]
fn running_calls_show_a_live_output_tail_of_complete_lines() {
    let call = tool_call_snapshot(
        3,
        "shell",
        r#"{"command":"cargo build"}"#,
        ToolCallState::Running,
        None,
        false,
    );
    let mut live = HashMap::new();
    live.insert(
        call.id,
        "one\ntwo\nthree\nfour\nfive\nsix\nseven b\u{7}ell\npartial".to_owned(),
    );

    for detail in [SimpleDetail::Rows, SimpleDetail::Expanded] {
        let rows = frame_rows(&render_tool_calls_simple(
            &[&call],
            &live,
            detail,
            0,
            80,
            &|_, _| Vec::new(),
        ));
        assert!(rows[0].contains("Run"), "the spinner one-liner stays");
        let tail_start = rows.len() - MAX_LIVE_TAIL_ROWS;
        assert_eq!(
            &rows[tail_start..],
            [
                "     two",
                "     three",
                "     four",
                "     five",
                "     six",
                // Control characters are stripped; the mid-line chunk
                // tail stays hidden until its newline arrives.
                "     seven bell",
            ]
        );
        assert!(!rows.iter().any(|row| row.contains("partial")));
    }

    // Overlong lines wrap literally at the character level and the tail
    // stays bounded in rows.
    let mut live = HashMap::new();
    live.insert(call.id, format!("{}\n", "x".repeat(40)));
    let rows = frame_rows(&render_tool_calls_simple(
        &[&call],
        &live,
        SimpleDetail::Rows,
        0,
        20,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows[1], format!("     {}", "x".repeat(15)));
    assert_eq!(rows[2], format!("     {}", "x".repeat(15)));
    assert!(rows.len() <= 1 + MAX_LIVE_TAIL_ROWS);

    // Calls that are no longer running render no tail even if a stale
    // buffer lingers.
    let mut finished = call.clone();
    finished.state = ToolCallState::Completed;
    finished.result = Some("ok\n".to_owned());
    let rows = frame_rows(&render_tool_calls_simple(
        &[&finished],
        &live,
        SimpleDetail::Rows,
        0,
        80,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows.len(), 1);
}

#[test]
fn diff_detection_requires_hunks_or_paired_change_lines() {
    assert!(looks_like_diff("@@ -1 +1 @@\n context"));
    assert!(looks_like_diff("-old\n+new"));
    assert!(!looks_like_diff(
        "Edited src/lib.rs: replaced 1 occurrence(s)."
    ));
    assert!(!looks_like_diff("+new line only"));
    assert!(!looks_like_diff(""));
}

#[test]
fn approval_prompts_render_edit_previews_as_colored_diffs() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let tool_call = tool_call_snapshot(
        9,
        "edit_file",
        r#"{"path":"src/lib.rs","content":"new"}"#,
        ToolCallState::AwaitingApproval,
        None,
        false,
    );
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(tool_call.run_id),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::ToolApprovalRequested {
                tool_call,
                shell: None,
                edit: Some(qq_protocol::EditPreview {
                    path: "src/lib.rs".to_owned(),
                    diff: "@@ -1 +1 @@\n-old\n+new".to_owned(),
                }),
            },
        )
    }));

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 24);
    let rows = frame_rows(&frame);

    // The approval is inline under the tool row: the transcript stays
    // visible, the file and diff head follow, then the four choices.
    assert!(
        rows.iter().any(|row| row.contains("row 0")),
        "transcript stays: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| squash(row).contains("◇ Edit src/lib.rs")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("approval needed")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("src/lib.rs")),
        "{rows:?}"
    );
    assert!(!frame_text(&frame).contains("arguments:"));
    let style_of = |needle: &str| {
        frame
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of("@@ -1 +1 @@"), Some(muted()));
    assert_eq!(style_of("-old"), Some(diff_line_style("-")));
    assert_eq!(style_of("+new"), Some(diff_line_style("+")));
    // The block offers all four decisions, including workspace lifetime.
    assert!(rows.iter().any(|row| {
        let row = squash(row);
        row.contains("y once")
            && row.contains("a session")
            && row.contains("w workspace")
            && row.contains("n deny")
    }));
    // The composer is disabled while the approval owns input.
    assert!(rows.iter().any(|row| row.starts_with(" ✎ ")), "{rows:?}");
}

#[test]
fn collapsed_summaries_curate_known_tools() {
    let cases = [
        (
            tool_call_snapshot(
                1,
                "read_file",
                r#"{"path":"src/config/loader.rs"}"#,
                ToolCallState::Completed,
                Some("a\nb\nc\n"),
                false,
            ),
            " ● Read src/config/loader.rs 3 lines",
        ),
        (
            tool_call_snapshot(
                2,
                "read_file",
                r#"{"path":"big.log"}"#,
                ToolCallState::Completed,
                Some("a\n…[qq: 41,207 bytes / 1,142 lines omitted; not stored]…\nz\n"),
                false,
            ),
            " ● Read big.log 2 lines · truncated",
        ),
        (
            tool_call_snapshot(
                6,
                "shell",
                r#"{"command":"cargo test"}"#,
                ToolCallState::Completed,
                Some("shell exit=0 elapsed=3.2 bytes=512\nrunning 4 tests\n"),
                false,
            ),
            " ● Run cargo test exit 0",
        ),
        (
            tool_call_snapshot(
                3,
                "search",
                r#"{"query":"pattern"}"#,
                ToolCallState::Completed,
                Some(
                    "search \"pattern\" mode=content matches=3/3 files=2 scanned=40\nsrc/a.rs\nL1: x pattern\nL9: pattern y\nsrc/b.rs\nL4: pattern\n",
                ),
                false,
            ),
            " ● Search \"pattern\" 3 hits · 2 files",
        ),
        (
            tool_call_snapshot(
                4,
                "search",
                r#"{"query":"absent"}"#,
                ToolCallState::Completed,
                Some("search \"absent\" mode=content matches=0/0 files=0 scanned=40\n"),
                false,
            ),
            " ● Search \"absent\" no matches",
        ),
        (
            tool_call_snapshot(
                6,
                "search",
                r#"{"query":"needle","limit":25}"#,
                ToolCallState::Completed,
                Some(
                    "search \"needle\" mode=content matches=25/90+ files=3 scanned=3 next=Yy50eHQANQ\n",
                ),
                false,
            ),
            " ● Search \"needle\" 25/90+ hits · 3 files",
        ),
        (
            tool_call_snapshot(
                5,
                "tree",
                r#"{"path":"crates/qq-core/src"}"#,
                ToolCallState::Completed,
                Some(
                    "tree crates/qq-core/src depth=2 entries=3/3 files=3 dirs=0\nlib.rs 1.2k  sessions.rs 40k  tools.rs 9.1k\n",
                ),
                false,
            ),
            " ● List crates/qq-core/src 3 entries",
        ),
        (
            tool_call_snapshot(
                7,
                "list_dir",
                r#"{"path":"."}"#,
                ToolCallState::Completed,
                Some(
                    "tree . depth=1 entries=3/12 files=3 dirs=0\na.rs 1  b.rs 2  c.rs 3\n…[qq: 9 more entries; raise limit]…\n",
                ),
                false,
            ),
            " ● List . 3/12 entries · truncated",
        ),
    ];
    for (call, expected) in cases {
        let rows = squashed_rows(&render_tool_calls_simple(
            &[&call],
            &HashMap::new(),
            SimpleDetail::Rows,
            0,
            120,
            &|_, _| Vec::new(),
        ));
        assert_eq!(rows, [expected]);
    }
}

#[test]
fn unknown_tools_fall_back_to_the_first_string_argument_and_byte_size() {
    let result = "x".repeat(2048);
    let call = tool_call_snapshot(
        1,
        "mcp__executor__run_query",
        r#"{"sql":"select 1","limit":10}"#,
        ToolCallState::Completed,
        Some(&result),
        false,
    );

    let rows = squashed_rows(&render_tool_calls_simple(
        &[&call],
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        160,
        &|_, _| Vec::new(),
    ));

    assert_eq!(rows, [" ● executor · run_query select 1 2.0 KB"]);

    // A known tool without a diff payload keeps a size metric.
    let edit = tool_call_snapshot(
        2,
        "edit_file",
        r#"{"path":"src/main.rs","content":"fn main() {}"}"#,
        ToolCallState::Completed,
        Some("Edited src/main.rs: replaced 1 occurrence(s)."),
        false,
    );
    let rows = squashed_rows(&render_tool_calls_simple(
        &[&edit],
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        160,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows, [" ● Edit src/main.rs 45 B"]);
}

#[test]
fn malformed_arguments_fall_back_to_a_raw_preview() {
    let call = tool_call_snapshot(
        1,
        "read_file",
        "{not json",
        ToolCallState::Completed,
        Some("a\n"),
        false,
    );

    let rows = frame_rows(&render_tool_calls_simple(
        &[&call],
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        120,
        &|_, _| Vec::new(),
    ));

    assert_eq!(squash(&rows[0]), " ● Read {not json 1 line");
}

#[test]
fn error_results_expand_under_the_summary_by_default() {
    let call = tool_call_snapshot(
        1,
        "read_file",
        r#"{"path":"gone.txt"}"#,
        ToolCallState::Completed,
        Some("path is not a file"),
        true,
    );

    let rows = frame_rows(&render_tool_calls_simple(
        &[&call],
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        120,
        &|_, _| Vec::new(),
    ));

    assert_eq!(squash(&rows[0]), " ✕ Read gone.txt");
    assert_eq!(rows[1], "     path is not a file");
}

#[test]
fn pending_states_show_their_glyph_and_label() {
    let awaiting = tool_call_snapshot(
        1,
        "shell",
        r#"{"command":"cargo test"}"#,
        ToolCallState::AwaitingApproval,
        None,
        false,
    );
    let rows = squashed_rows(&render_tool_calls_simple(
        &[&awaiting],
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows, [" ◇ Run cargo test awaiting approval"]);

    let running = tool_call_snapshot(
        2,
        "search",
        r#"{"query":"x"}"#,
        ToolCallState::Running,
        None,
        false,
    );
    let rows = frame_rows(&render_tool_calls_simple(
        &[&running],
        &HashMap::new(),
        SimpleDetail::Rows,
        1,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(
        squashed_rows(
            &rows
                .iter()
                .map(|row| Line::styled(row.clone(), normal()))
                .collect::<Vec<_>>()
        ),
        [" ◓ Search \"x\" running"]
    );
}

#[test]
fn quiet_runs_fold_into_a_single_counted_line() {
    let mut calls = Vec::new();
    for byte in 1..=4 {
        calls.push(tool_call_snapshot(
            byte,
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolCallState::Completed,
            Some("a\n"),
            false,
        ));
    }
    for byte in 5..=6 {
        calls.push(tool_call_snapshot(
            byte,
            "search",
            r#"{"query":"x"}"#,
            ToolCallState::Completed,
            Some("No matches found.\n"),
            false,
        ));
    }
    let references = calls.iter().collect::<Vec<_>>();

    // The default shows one row per call; folding is opt-in.
    let rows = frame_rows(&render_tool_calls_simple(
        &references,
        &HashMap::new(),
        SimpleDetail::Rows,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows.len(), 6);

    let rows = frame_rows(&render_tool_calls_simple(
        &references,
        &HashMap::new(),
        SimpleDetail::Folded,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(
        squashed_rows(
            &rows
                .iter()
                .map(|row| Line::styled(row.clone(), normal()))
                .collect::<Vec<_>>()
        ),
        [" ▸ Read ×4 Search ×2 a.rs"]
    );

    // An active or failed call keeps every line visible even when folded.
    calls[5].state = ToolCallState::Running;
    let references = calls.iter().collect::<Vec<_>>();
    let rows = frame_rows(&render_tool_calls_simple(
        &references,
        &HashMap::new(),
        SimpleDetail::Folded,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows.len(), 6);

    // Expanded detail never folds.
    calls[5].state = ToolCallState::Completed;
    let references = calls.iter().collect::<Vec<_>>();
    let rows = frame_rows(&render_tool_calls_simple(
        &references,
        &HashMap::new(),
        SimpleDetail::Expanded,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert!(rows.len() > 6);
}

#[test]
fn expanding_a_read_shows_the_head_of_the_file_and_never_its_json() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let body = (1..=20)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Completed,
        Some(&body),
        false,
    )]);
    let mut renderer = FrameRenderer::default();

    let rows = frame_rows(&renderer.frame_and_commit(&mut app, 100, 30));
    assert!(
        rows.iter()
            .any(|row| squash(row).contains("Read note.txt 20 lines"))
    );
    assert!(
        !rows.iter().any(|row| row.contains("line 1")),
        "no body by default"
    );

    // Ctrl-Up selects the call, Enter expands it alone.
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::CONTROL,
    )));
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let rows = frame_rows(&renderer.frame_and_commit(&mut app, 100, 30));
    let text = rows.join("\n");
    assert!(text.contains("line 1\n"), "head first: {text}");
    assert!(
        text.contains(&format!("line {MAX_TOOL_RESULT_ROWS}")),
        "{text}"
    );
    assert!(!text.contains("line 20"), "bounded: {text}");
    assert!(text.contains("… 8 lines more"), "{text}");
    assert!(
        !text.contains("\"path\""),
        "known tools show no JSON: {text}"
    );

    // Ctrl-O folds the block rather than expanding anything.
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.tool_detail, ToolDetail::Folded);
}

#[test]
fn tool_rows_respect_narrow_widths() {
    let calls = [
        tool_call_snapshot(
            1,
            "read_file",
            r#"{"path":"a/very/long/path/that/never/ends.rs"}"#,
            ToolCallState::Completed,
            Some("line one that is fairly long\nline two\n"),
            false,
        ),
        tool_call_snapshot(
            2,
            "shell",
            r#"{"command":"cargo test --workspace --all-features"}"#,
            ToolCallState::Failed,
            Some("error: a very long failure message that overflows"),
            true,
        ),
    ];
    let references = calls.iter().collect::<Vec<_>>();
    for width in 0..24 {
        for detail in [SimpleDetail::Rows, SimpleDetail::Expanded] {
            let lines = render_tool_calls_simple(
                &references,
                &HashMap::new(),
                detail,
                0,
                width,
                &|_, _| Vec::new(),
            );
            assert!(lines.iter().all(|line| line.width() <= width));
        }
    }
}

#[test]
fn user_prompts_carry_an_accent_bar() {
    let mut renderer = FrameRenderer::default();
    let mut message = completed_message(1, "deploy the API".to_owned());
    message.role = MessageRole::User;

    let rows = frame_rows(&renderer.render_message(&message, 80));

    assert!(rows[0].starts_with(" ▌ YOU"));
    assert!(rows[1].starts_with(" ▌ "));
    assert_eq!(
        renderer.render_message(&message, 80)[0].spans[0].style,
        accent()
    );
}

#[test]
fn final_output_sanitizes_every_dynamic_span() {
    let line = Line::styled("title\u{1b}]52;c;Y2xpcGJvYXJk\u{7}\u{202e}", normal());
    let mut rendered = Vec::new();

    write_line(&mut rendered, &line).unwrap();

    let rendered = String::from_utf8(rendered).unwrap();
    assert!(!rendered.contains("\u{1b}]52"));
    assert!(!rendered.contains('\u{7}'));
    assert!(!rendered.contains('\u{202e}'));
}

#[test]
fn panel_rows_carry_the_surface_background_through_output() {
    let row = code_panel_row(Line::styled("x", normal()), 8);
    assert!(
        row.spans
            .iter()
            .all(|span| span.style.background == Some(surface_color()))
    );
    let mut rendered = Vec::new();

    write_line(&mut rendered, &row).unwrap();

    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains('x'));
    if std::env::var_os("NO_COLOR").is_none() {
        assert!(rendered.contains("\u{1b}[48;2;38;40;48m"));
    } else {
        assert!(!rendered.contains("\u{1b}[48;2;38;40;48m"));
    }
}

#[test]
fn completed_markdown_cache_is_bounded_and_keeps_one_width() {
    let mut renderer = FrameRenderer::default();
    let message = completed_message(1, "hello".to_owned());
    renderer.render_message(&message, 40);
    renderer.render_message(&message, 80);
    assert_eq!(renderer.markdown().len(), 1);
    assert_eq!(renderer.markdown()[&message.id].width, 80);

    for byte in 2..=u8::try_from(MAX_VISIBLE_MESSAGES + 8).unwrap() {
        renderer.render_message(&completed_message(byte, byte.to_string()), 80);
    }
    assert!(renderer.markdown().len() <= MAX_VISIBLE_MESSAGES);
}

#[test]
fn authoritative_snapshot_generation_invalidates_same_length_cached_output() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions
        .get_mut(&session_id)
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0]
        .output = "old".to_owned();
    let mut renderer = FrameRenderer::default();
    let initial = renderer.transcript(&app, 80);
    assert!(frame_text(&initial.viewport(&app, initial.rows, 0)).contains("old"));
    drop(initial);

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].output = "new".to_owned();
    session.loaded_through += 1;
    let refreshed = renderer.transcript(&app, 80);
    let text = frame_text(&refreshed.viewport(&app, refreshed.rows, 0));

    assert!(text.contains("new"));
    assert!(!text.contains("old"));
}

#[test]
fn completed_markdown_preserves_the_beginning_and_end_of_long_messages() {
    let mut renderer = FrameRenderer::default();
    let output = (1..=10)
        .map(|phase| {
            format!(
                "## Phase {phase}\n{}{}\n",
                if phase == 1 {
                    "BEGIN-FIRST-PHASE\n"
                } else {
                    ""
                },
                (0..80)
                    .map(|step| format!("- phase {phase} step {step}: verify the complete output"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
        .collect::<String>()
        + "\nEND-FINAL-PHASE";
    let message = completed_message(1, output);
    let rendered = renderer.render_message(&message, 80);

    let text = rendered
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(text.contains("BEGIN-FIRST-PHASE"));
    assert!(text.contains("phase 5 step 40"));
    assert!(text.contains("END-FINAL-PHASE"));
}

#[test]
fn oversized_completed_messages_use_a_sparse_full_history_index() {
    let mut app = app_with_messages(1);
    let message = &mut app
        .sessions
        .get_mut(&app.focused().unwrap())
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0];
    message.output = std::iter::once("BEGIN-SPARSE".to_owned())
        .chain((0..12_000).map(|row| format!("ROW-{row:05} 😀")))
        .chain(std::iter::once("END-SPARSE".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(message.output.len() > MAX_FULL_MARKDOWN_BYTES);

    let mut renderer = FrameRenderer::default();
    let body = renderer.transcript(&app, 80);
    let (index, message_id, prefix, prefix_style, width) = body
        .segments
        .iter()
        .find_map(|segment| match segment {
            BodySegment::Plain {
                index,
                message_id,
                prefix,
                prefix_style,
                width,
            } => Some((*index, *message_id, *prefix, *prefix_style, *width)),
            BodySegment::Owned(_) | BodySegment::Cached(_) => None,
        })
        .expect("oversized message uses sparse rendering");
    assert!(index.checkpoints.len() <= MAX_PLAIN_TEXT_CHECKPOINTS + 1);

    let top = frame_text(&body.viewport(&app, 20, body.rows.saturating_sub(20)));
    let tail = frame_text(&body.viewport(&app, 20, 0));
    assert!(top.contains("BEGIN-SPARSE"));
    assert!(tail.contains("END-SPARSE"));

    let source = MessageText::new(find_message(&app, message_id).unwrap());
    let middle = frame_text(&index.render(source, 6_000..6_006, prefix, prefix_style, width));
    assert!(middle.contains("ROW-05999"));
    assert!(middle.contains('😀'));
}

#[test]
fn combined_output_and_refusal_preserve_both_channels() {
    let mut app = app_with_messages(1);
    let message = &mut app
        .sessions
        .get_mut(&app.focused().unwrap())
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0];
    message.output = "OUTPUT-BEGIN".to_owned() + &"o".repeat(40 * 1024);
    message.refusal = "REFUSAL-BEGIN".to_owned() + &"r".repeat(40 * 1024) + "REFUSAL-END";

    let mut renderer = FrameRenderer::default();
    let body = renderer.transcript(&app, 80);
    let top = frame_text(&body.viewport(&app, 20, body.rows.saturating_sub(20)));
    let tail = frame_text(&body.viewport(&app, 20, 0));

    assert!(top.contains("OUTPUT-BEGIN"));
    assert!(tail.contains("REFUSAL-END"));
}

#[test]
fn completing_a_long_live_message_preserves_a_scrolled_tail_anchor() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let message = &mut session.messages.as_mut().unwrap()[0];
    message.state = MessageState::Streaming;
    message.output = (0..2_000)
        .map(|row| format!("LIVE-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 24);
    app.handle_terminal_event(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ),
    ));
    let live_offset = app.transcript_scroll_offset();
    assert!(live_offset > 0);

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].state = MessageState::Complete;
    session.loaded_through += 1;
    renderer.frame_and_commit(&mut app, 80, 24);

    assert_eq!(app.transcript_scroll_offset(), live_offset);
}

#[test]
fn completion_behind_an_overlay_preserves_the_scrolled_live_tail() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let message = &mut session.messages.as_mut().unwrap()[0];
    message.state = MessageState::Streaming;
    message.output = (0..2_000)
        .map(|row| format!("LIVE-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 24);
    app.handle_terminal_event(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ),
    ));
    let live_offset = app.transcript_scroll_offset();
    app.open_model_picker_for_test();
    renderer.frame_and_commit(&mut app, 80, 24);

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].state = MessageState::Complete;
    session.loaded_through += 1;
    renderer.frame_and_commit(&mut app, 80, 24);
    app.overlay = None;
    renderer.frame_and_commit(&mut app, 80, 24);

    assert_eq!(app.transcript_scroll_offset(), live_offset);
}

#[test]
fn completing_a_live_message_does_not_move_an_older_history_viewport() {
    let mut app = app_with_messages(2);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].output = (0..200)
        .map(|row| format!("HISTORY-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    messages[1].state = MessageState::Streaming;
    messages[1].output = (0..2_000)
        .map(|row| format!("LIVE-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 24);
    let page_up = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::PageUp,
        crossterm::event::KeyModifiers::NONE,
    ));
    while app.handle_terminal_event(page_up.clone()).split().0 {}
    let before = renderer.frame_and_commit(&mut app, 80, 24);
    assert!(frame_text(&before).contains("HISTORY-ROW-0000"));
    let history_offset = app.transcript_scroll_offset();

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[1].state = MessageState::Complete;
    session.loaded_through += 1;
    let after = renderer.frame_and_commit(&mut app, 80, 24);

    assert!(frame_text(&after).contains("HISTORY-ROW-0000"));
    assert!(app.transcript_scroll_offset() > history_offset);
}

#[test]
fn sparse_rows_have_a_byte_ceiling_for_zero_width_text() {
    let message = completed_message(
        1,
        format!(
            "a{}",
            "\u{0301}".repeat(MAX_FULL_MARKDOWN_BYTES / '\u{0301}'.len_utf8() + 1)
        ),
    );
    let source = MessageText::new(&message);
    let mut byte = 0;
    let mut rows = 0;
    while let Some((range, next)) = next_plain_text_row(source, byte, 80) {
        assert!(range.len() <= MAX_PLAIN_TEXT_ROW_BYTES);
        assert!(next > byte);
        rows += 1;
        byte = next;
    }
    assert!(rows > 1);

    let index = PlainTextIndex::new(source, 80);
    let rendered = index.render(source, 0..1, "   ", muted(), 83);
    let emitted_bytes = rendered[0]
        .spans
        .iter()
        .map(|span| span.text.len())
        .sum::<usize>();
    assert!(emitted_bytes <= MAX_PLAIN_TEXT_ROW_BYTES + 3);
}

#[test]
fn refreshed_chrome_shows_identity_status_and_session_metrics() {
    let mut app = app_with_messages(1);
    app.connection = crate::ConnectionState::Live;
    app.models.push(ModelOption {
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        name: Some("GPT Test".to_owned()),
        context_window: Some(128_000),
        selection: ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        },
    });
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.context_tokens = Some(64_000);
    session.context_window = Some(128_000);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 12);
    let rows = frame_rows(&frame);

    // One top row: brand, breadcrumb, then model and context on the right.
    // No version, no "local", no layout name.
    assert!(rows[0].starts_with(" qq  Session"), "{:?}", rows[0]);
    assert!(!rows[0].contains(VERSION));
    assert!(!rows[0].contains("local"));
    assert!(rows[0].contains("openai/gpt-test"));
    assert!(rows[0].contains("50% ctx"), "{:?}", rows[0]);
    assert_eq!(frame[0].spans[0].style, brand().bold());
    // Rule then composer: the bottom two rows. The rule carries the hints so
    // no row is spent on them, and an idle session shows no state chip.
    assert!(rows[10].starts_with('─'), "{:?}", rows[10]);
    assert!(rows[10].contains("F1 help"), "{:?}", rows[10]);
    assert!(rows[10].contains("^K commands"), "{:?}", rows[10]);
    assert!(!rows[10].contains("idle"), "{:?}", rows[10]);
    assert!(rows[11].starts_with(" › Ask QQ..."), "{:?}", rows[11]);
    // Rows 1..=9 are transcript: nine body rows out of twelve.
    assert!(rows[1..10].iter().any(|row| row.contains("row 0")));
}

#[test]
fn top_row_renders_unknown_context_and_cost_without_inventing_zero_usage() {
    let mut app = app_with_messages(0);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.estimated_cost_usd_nanos = Some(100_000_000);
    session.summary.accounting = Some(SessionAccounting {
        direct: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(100_000_000),
        },
        inclusive: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        },
    });
    session.summary.context_tokens = None;
    session.context_window = Some(272_000);

    let rows = frame_rows(&[top_row(&app, 80)]);

    assert!(
        !rows[0].contains("ctx"),
        "unknown occupancy shows nothing: {:?}",
        rows[0]
    );
    assert!(
        !rows[0].contains('$'),
        "unknown cost shows nothing: {:?}",
        rows[0]
    );
}

#[test]
fn top_row_uses_legacy_direct_cost_when_structured_accounting_is_absent() {
    let mut app = app_with_messages(0);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.accounting = None;
    session.summary.estimated_cost_usd_nanos = Some(100_000_000);

    app.connection = crate::ConnectionState::Live;
    let rows = frame_rows(&[top_row(&app, 80)]);

    assert!(rows[0].ends_with("$0.10 "), "{:?}", rows[0]);
}

#[test]
fn top_row_displays_inclusive_accounting_cost() {
    let mut app = app_with_messages(0);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.estimated_cost_usd_nanos = Some(100_000_000);
    session.summary.accounting = Some(SessionAccounting {
        direct: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(100_000_000),
        },
        inclusive: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(250_000_000),
        },
    });

    app.connection = crate::ConnectionState::Live;
    let rows = frame_rows(&[top_row(&app, 80)]);

    assert!(rows[0].ends_with("$0.25 "), "{:?}", rows[0]);
}

#[test]
fn top_row_names_the_connection_only_when_it_has_a_problem() {
    let mut app = app_with_messages(0);
    app.connection = crate::ConnectionState::Live;
    let live = frame_rows(&[top_row(&app, 80)])[0].clone();
    assert!(!live.contains("connecting") && !live.contains("offline"));
    for (connection, expected) in [
        (crate::ConnectionState::Connecting, "connecting "),
        (crate::ConnectionState::Replaying, "reconnecting "),
        (crate::ConnectionState::Offline, "offline "),
    ] {
        app.connection = connection;
        let row = frame_rows(&[top_row(&app, 80)])[0].clone();
        assert!(row.ends_with(expected), "{row:?}");
    }
}

#[test]
fn threadline_has_no_vertical_message_rails() {
    let mut app = app_with_messages(2);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 14);

    assert!(frame_rows(&frame).iter().all(|row| !row.contains("  |  ")));
}

#[test]
fn composer_renders_hard_newlines_across_multiple_rows_and_reports_the_caret() {
    let mut app = App::new(TuiOptions::default());
    app.composer.text = "hello\nworld".to_owned();
    let (lines, caret) = composer(&app, 40, 8);
    let rows = frame_rows(&lines);
    // No fake caret in the text; the real cursor sits after "world".
    assert_eq!(rows, vec![" › hello".to_owned(), "   world".to_owned()]);
    assert_eq!(caret, Some((3 + 5, 1)));
}

#[test]
fn the_terminal_cursor_follows_the_composer_caret_and_hides_under_overlays() {
    let mut app = app_with_messages(1);
    app.composer.text = "ab".to_owned();
    let mut renderer = FrameRenderer::default();
    let bytes = renderer.draw(&mut app, (80, 12)).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    // Composer is row 11 (0-based) in a 12-row frame; caret after "ab" is
    // column 3 + 2 = 5, so the terminal cursor moves to row 12, column 6 in
    // 1-based ANSI coordinates and is shown.
    assert!(text.contains("\x1b[12;6H\x1b[?25h"), "{text:?}");
    // Moving the cursor left moves the terminal cursor with it.
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::NONE,
    )));
    let bytes = renderer.draw(&mut app, (80, 12)).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[12;5H\x1b[?25h"), "{text:?}");
    // An overlay owns input without a caret: the cursor hides.
    app.execute(Command::OpenCommands);
    let bytes = renderer.draw(&mut app, (80, 12)).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.ends_with("\x1b[?25l\x1b[?2026l"), "{text:?}");
}

#[test]
fn the_composer_glyph_says_what_enter_will_do() {
    let (mut app, _, _, _) = running_view_app();
    let (lines, _) = composer(&app, 40, 2);
    assert!(
        frame_rows(&lines)[0].starts_with(" ⇥ "),
        "queue while running without steering"
    );
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        fixtures::steering_capabilities(),
    )));
    let (lines, _) = composer(&app, 40, 2);
    assert!(
        frame_rows(&lines)[0].starts_with(" ↦ "),
        "steer when advertised"
    );
    let idle = app_with_messages(0);
    let (lines, _) = composer(&idle, 40, 2);
    assert!(frame_rows(&lines)[0].starts_with(" › "), "send when idle");
}

#[test]
fn an_80_by_24_frame_gives_the_transcript_at_least_twenty_rows() {
    let mut app = app_with_messages(30);
    app.sidebar = crate::app::Sidebar::Hidden;
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 24);
    let rows = frame_rows(&frame);
    // Body rows are everything between the top row and the composer rule.
    let rule = rows
        .iter()
        .position(|row| row.starts_with('─'))
        .expect("composer rule");
    let transcript_rows = rule - 1;
    assert!(
        transcript_rows >= 20,
        "{transcript_rows} transcript rows: {rows:#?}"
    );
}

#[test]
fn composer_keeps_the_rows_around_the_caret_when_max_rows_clip() {
    let mut app = App::new(TuiOptions::default());
    app.composer.text = "one\ntwo\nthree\nfour".to_owned();
    let (lines, caret) = composer(&app, 40, 2);
    let rows = frame_rows(&lines);
    assert_eq!(rows, vec![" … three".to_owned(), "   four".to_owned()]);
    assert_eq!(caret, Some((3 + 4, 1)));
}

#[test]
fn slash_autocomplete_is_filtered_above_the_composer() {
    let mut app = app_with_messages(1);
    app.composer.text = "/".to_owned();
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 30);
    let text = frame_text(&frame);
    // The menu is a boxed list: a labelled rule, then at most eight rows
    // with the cursor visible, so it never swallows the transcript.
    assert!(text.contains(" commands "));
    for command in ["/help", "/commands", "/sessions", "/resume", "/new"] {
        assert!(text.contains(command), "{command}");
    }
    assert!(
        !text.contains("/exit"),
        "rows past the cap stay hidden until the cursor reaches them"
    );
    for _ in 0..30 {
        app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
    }
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 30);
    let text = frame_text(&frame);
    assert!(text.contains("/exit"));

    app.composer.text = "/qu".to_owned();
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 14);
    let text = frame_text(&frame);

    assert!(text.contains("/quit"));
    assert!(!text.contains("/models"));
    assert!(!text.contains("/sessions"));
}

#[test]
fn session_picker_pins_search_and_keeps_the_selection_visible() {
    let mut app = app_with_messages(0);
    let mut selected = None;
    for byte in 2..20 {
        let session_id = SessionId::from_bytes([byte; 16]);
        if byte == 10 {
            selected = Some(session_id);
        }
        let summary = SessionSummary {
            title: format!("Session {byte}"),
            updated_at_ms: u64::from(byte),
            ..fixtures::session_summary(session_id)
        };
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            occurred_at_ms: u64::from(byte),
            ..fixtures::envelope(
                u64::from(byte),
                session_id,
                SessionEvent::SessionCreated { session: summary },
            )
        }));
    }
    app.open_session_picker_with("", selected, None);

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 12);
    let text = frame_text(&frame);

    assert!(text.contains("SESSIONS"));
    assert!(text.contains("search: all sessions"));
    assert!(text.contains("Session 10"));
}

#[test]
fn session_picker_renders_an_empty_search_result() {
    let mut app = app_with_messages(0);
    app.open_session_picker_with("missing", None, None);

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 12);
    let text = frame_text(&frame);

    assert!(text.contains("search: missing"));
    assert!(text.contains("No matching sessions."));
}

#[test]
fn session_picker_renders_delete_and_prune_confirmations() {
    let mut app = app_with_messages(0);
    let session_id = SESSION;
    app.open_session_picker_with(
        "",
        Some(session_id),
        Some(SessionConfirm::Delete(session_id)),
    );

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("y confirms, n or Esc cancels"));
    assert!(text.contains("delete 'Session'? y deletes, n keeps"));

    app.overlay
        .as_mut()
        .unwrap()
        .set_confirm(Some(SessionConfirm::Prune));
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("delete every empty session in this workspace?"));

    // Without a pending confirmation the hint advertises both actions.
    app.overlay.as_mut().unwrap().set_confirm(None);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("Ctrl-D deletes"));
    assert!(!text.contains("Ctrl-P"), "Ctrl-P is not a picker chord");
}

#[test]
fn model_picker_hint_reflects_apply_versus_create() {
    let mut app = app_with_messages(0);
    app.models.push(crate::app::ModelOption {
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        name: Some("GPT Test".to_owned()),
        context_window: None,
        selection: ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: None,
            organization: None,
        },
    });
    app.open_model_picker_for_test();

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("Enter sets the session model, Ctrl-N creates a session"));

    app.view = View::Transcript(None);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("Enter creates session"));
}

#[test]
fn transcript_viewport_renders_rows_above_the_tail_and_clamps_at_the_top() {
    let lines = (0..8)
        .map(|row| Line::styled(row.to_string(), normal()))
        .collect::<Vec<_>>();

    let scrolled = transcript_viewport(lines.clone(), 3, 2);
    let top = transcript_viewport(lines, 3, usize::MAX);
    let text = |rows: &[Line]| {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(text(&scrolled), ["3", "4", "5"]);
    assert_eq!(text(&top), ["0", "1", "2"]);
}

#[test]
fn page_up_replaces_the_rendered_live_tail_with_older_transcript_rows() {
    let mut app = app_with_messages(10);
    let mut renderer = FrameRenderer::default();
    let tail = renderer.frame_and_commit(&mut app, 80, 12);

    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )));
    let scrolled = renderer.frame_and_commit(&mut app, 80, 12);

    assert!(frame_text(&tail).contains("row 9"));
    assert!(!frame_text(&scrolled).contains("row 9"));
    assert!(frame_text(&scrolled).contains("row 6"));
}

#[test]
fn page_up_reaches_the_beginning_of_a_long_completed_message() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions
        .get_mut(&session_id)
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0]
        .output = format!(
        "BEGIN-LONG-MESSAGE\n{}\nEND-LONG-MESSAGE",
        (0..400)
            .map(|row| format!("long response row {row}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut renderer = FrameRenderer::default();
    let tail = renderer.frame_and_commit(&mut app, 80, 12);

    for _ in 0..100 {
        app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
    }
    let top = renderer.frame_and_commit(&mut app, 80, 12);

    assert!(frame_text(&tail).contains("END-LONG-MESSAGE"));
    assert!(!frame_text(&tail).contains("BEGIN-LONG-MESSAGE"));
    assert!(frame_text(&top).contains("BEGIN-LONG-MESSAGE"));
}

#[test]
fn sidebar_appears_at_wide_widths_and_shows_live_status_for_cold_sessions() {
    let mut app = app_with_messages(1);
    app.connection = crate::ConnectionState::Live;
    let parent = app.focused().unwrap();
    let child_id = SessionId::from_bytes([7; 16]);
    let run_id = RunId::from_bytes([8; 16]);
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            child_id,
            SessionEvent::SessionCreated {
                session: SessionSummary {
                    parent_id: Some(parent),
                    title: "Survey callers".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(run_id),
                    activity: Some(qq_protocol::RunActivity::GeneratingResponse),
                    model: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    ..fixtures::session_summary(child_id)
                },
            },
        )
    }));
    // The child is cold (no body) but streams text; the sidebar must
    // still show its tail.
    let message = MessageSnapshot {
        run_id,
        state: MessageState::Streaming,
        created_at_ms: 3,
        ..fixtures::message(MessageId::from_bytes([9; 16]), child_id, "")
    };
    for (sequence, event) in [
        (3, SessionEvent::AssistantMessageStarted { message }),
        (
            4,
            SessionEvent::TextAppended {
                message_id: MessageId::from_bytes([9; 16]),
                channel: qq_protocol::TextChannel::Output,
                text: "Found twelve call sites".to_owned(),
            },
        ),
    ] {
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, child_id, event)
        }));
    }
    assert!(!app.sessions[&child_id].is_warm());

    let rows_at = |app: &mut App, width| {
        frame_rows(&FrameRenderer::default().frame_and_commit(app, width, 24)).join("\n")
    };
    let narrow = rows_at(&mut app, 90);
    assert!(!narrow.contains("WORKING  1"), "auto-hidden when narrow");

    let wide_frame = FrameRenderer::default().frame_and_commit(&mut app, 160, 24);
    let wide = frame_rows(&wide_frame).join("\n");
    assert!(wide.contains("WORKING  1"), "{wide}");
    // The narrow frame shows the agent strip instead so the child is not
    // invisible below the auto width.
    assert!(narrow.contains("2 agents"), "{narrow}");
    assert!(wide.contains("Survey callers"));
    assert!(wide.contains("Found twelve cal"), "{wide}");
    // With the sidebar glued on, every body row is exactly the terminal
    // width: the border column lines up and nothing overflows.
    for row in &wide_frame[1..wide_frame.len() - 3] {
        assert_eq!(
            row.width(),
            160,
            "{:?}",
            frame_rows(std::slice::from_ref(row))
        );
    }

    // Ctrl-\ hides it even when wide; a second press shows it again.
    let toggle = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL));
    app.handle_terminal_event(toggle.clone());
    assert!(!rows_at(&mut app, 160).contains("WORKING  1"));
    app.handle_terminal_event(toggle);
    assert!(
        rows_at(&mut app, 90).contains("WORKING  1"),
        "explicitly shown wins over width"
    );
}

#[test]
fn the_sidebar_stays_hidden_with_one_session_and_scales_with_width() {
    let mut app = app_with_messages(1);
    let rows_at = |app: &mut App, width| {
        frame_rows(&FrameRenderer::default().frame_and_commit(app, width, 24)).join("\n")
    };
    assert!(
        !rows_at(&mut app, 200).contains("IDLE  1"),
        "one session: nothing to list"
    );
    let sidebar = crate::app::Sidebar::Auto;
    assert_eq!(sidebar.width(100, 2), 25);
    assert_eq!(sidebar.width(200, 2), crate::app::SIDEBAR_MAX_WIDTH);
    assert_eq!(sidebar.width(99, 2), 0);
    assert_eq!(crate::app::Sidebar::Shown.width(80, 1), 20);
}

#[test]
fn spawned_children_render_under_their_spawn_call_and_never_fold() {
    let mut app = app_with_messages(1);
    let parent = app.focused().unwrap();
    let run_id = RunId::from_bytes([2; 16]);
    let spawn_call = tool_call_snapshot(
        0x21,
        "spawn_agent",
        r#"{"task":"survey callers"}"#,
        ToolCallState::Running,
        None,
        false,
    );
    // Four quiet reads plus the spawn call: without the child this run
    // would fold into one counted line at collapsed detail.
    let mut calls = vec![spawn_call.clone()];
    for byte in 0x22..0x26 {
        calls.push(tool_call_snapshot(
            byte,
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolCallState::Completed,
            Some("ok"),
            false,
        ));
    }
    app.sessions.get_mut(&parent).unwrap().tool_calls = Some(calls);
    let child_id = SessionId::from_bytes([0x30; 16]);
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(RunId::from_bytes([0x31; 16])),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            child_id,
            SessionEvent::SessionCreated {
                session: SessionSummary {
                    parent_id: Some(parent),
                    spawned_by: Some(qq_protocol::SpawnOrigin {
                        run_id,
                        tool_call_id: Some(spawn_call.id),
                        depth: 1,
                    }),
                    title: "survey callers".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(RunId::from_bytes([0x31; 16])),
                    activity: Some(qq_protocol::RunActivity::Reasoning),
                    model: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    ..fixtures::session_summary(child_id)
                },
            },
        )
    }));
    app.sidebar = crate::app::Sidebar::Hidden;

    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 40));
    let spawn_row = rows
        .iter()
        .position(|row| row.contains("Spawn"))
        .expect("spawn call is rendered, not folded");
    assert!(rows[spawn_row + 1].contains("↳"));
    assert!(rows[spawn_row + 1].contains("survey callers"));
    assert!(rows[spawn_row + 2].contains("reasoning"));
    assert!(
        rows.iter().all(|row| !row.contains("tool calls")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("related sessions")),
        "an inline child is not repeated below"
    );

    // A child with no recorded call attaches nowhere in the transcript
    // but still appears in related sessions.
    app.sessions.get_mut(&child_id).unwrap().summary.spawned_by = None;
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 40));
    let spawn_row = rows.iter().position(|row| row.contains("Spawn")).unwrap();
    assert!(!rows[spawn_row + 1].contains("↳"));
    assert!(rows.iter().any(|row| row.contains("related sessions")));
}

#[test]
fn background_approvals_surface_a_banner_that_ctrl_g_jumps_to() {
    let mut app = app_with_messages(1);
    app.sidebar = crate::app::Sidebar::Hidden;
    let parent = app.focused().unwrap();
    let child_id = SessionId::from_bytes([0x40; 16]);
    let run_id = RunId::from_bytes([0x41; 16]);
    let mut sequence = 1;
    let mut event = |session_id, event| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    app.apply_client_update(event(
        child_id,
        SessionEvent::SessionCreated {
            session: SessionSummary {
                parent_id: Some(parent),
                title: "Deploy helper".to_owned(),
                status: SessionStatus::Running,
                active_run_id: Some(run_id),
                model: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 2,
                ..fixtures::session_summary(child_id)
            },
        },
    ));
    let call = ToolCallSnapshot {
        run_id,
        call_ordinal: 0,
        provider_call_id: "c".to_owned(),
        arguments: r#"{"command":"rm -rf build"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(ToolCallId::from_bytes([0x42; 16]), child_id, "shell")
    };
    app.apply_client_update(event(
        child_id,
        SessionEvent::ToolApprovalRequested {
            tool_call: call,
            shell: None,
            edit: None,
        },
    ));

    // Focused on the parent: no modal, but the banner names the child.
    assert_eq!(app.mode(), Mode::Compose);
    let text = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24)).join("\n");
    assert!(text.contains("Deploy helper needs approval"), "{text}");
    assert!(text.contains("Ctrl-G"));

    let (changed, requests) = app
        .handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        )))
        .split();
    assert!(changed);
    assert_eq!(app.focused(), Some(child_id));
    // The child is cold, so the jump fetches its body...
    assert_eq!(requests.len(), 1);
    // ...and the banner no longer names the session we are now in.
    let text = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24)).join("\n");
    assert!(!text.contains("approval needed in"));
}

#[test]
fn alt_arrows_walk_the_session_tree_in_spawn_order() {
    let mut app = app_with_messages(0);
    app.sidebar = crate::app::Sidebar::Hidden;
    let root = app.focused().unwrap();
    let mut sequence = 1;
    let mut created = |app: &mut App, byte: u8, parent: Option<SessionId>, at: u64| {
        sequence += 1;
        let id = SessionId::from_bytes([byte; 16]);
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            occurred_at_ms: sequence,
            ..fixtures::envelope(
                sequence,
                id,
                SessionEvent::SessionCreated {
                    session: SessionSummary {
                        parent_id: parent,
                        title: format!("s{byte}"),
                        model: None,
                        estimated_cost_usd_nanos: None,
                        updated_at_ms: at,
                        ..fixtures::session_summary(id)
                    },
                },
            )
        }));
        id
    };
    let a = created(&mut app, 0x51, Some(root), 10);
    let b = created(&mut app, 0x52, Some(root), 20);
    let c = created(&mut app, 0x53, Some(root), 30);
    let key = |code| TerminalEvent::Key(KeyEvent::new(code, KeyModifiers::ALT));

    app.handle_terminal_event(key(KeyCode::Down));
    assert_eq!(app.focused(), Some(a), "first child is the oldest");
    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(app.focused(), Some(b));
    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(app.focused(), Some(c));
    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(app.focused(), Some(a), "siblings wrap");
    app.handle_terminal_event(key(KeyCode::Left));
    assert_eq!(app.focused(), Some(c));
    // Esc walks up to the parent (Alt-Up belongs to the draft queue).
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.focused(), Some(root));
    // A lone root has no siblings; the key is a no-op.
    let (changed, _) = app.handle_terminal_event(key(KeyCode::Right)).split();
    assert!(!changed);
    assert_eq!(app.focused(), Some(root));
}

#[test]
fn reasoning_renders_collapsed_above_the_runs_message_and_expands_on_toggle() {
    let mut app = app_with_messages(0);
    app.sidebar = crate::app::Sidebar::Hidden;
    let session_id = app.focused().unwrap();
    let run_id = RunId::from_bytes([0x66; 16]);
    let mut sequence = 1;
    let mut event = |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    let kind = qq_protocol::ReasoningKind::Summary;
    app.apply_client_update(event(SessionEvent::ReasoningStarted { run_id, kind }));
    app.apply_client_update(event(SessionEvent::ReasoningDelta {
        run_id,
        kind,
        text: "First consider the callers.\n\nThen the tests.".to_owned(),
    }));
    app.apply_client_update(event(SessionEvent::ReasoningCompleted { run_id, kind }));
    app.apply_client_update(event(SessionEvent::AssistantMessageStarted {
        message: MessageSnapshot {
            run_id,
            state: MessageState::Streaming,
            ..fixtures::message(MessageId::from_bytes([0x67; 16]), session_id, "The answer.")
        },
    }));

    let rows = frame_rows(&transcript_lines(&app, 80));
    let reasoning_row = rows
        .iter()
        .position(|row| row.contains("thought for"))
        .expect("collapsed reasoning row");
    let message_row = rows.iter().position(|row| row.contains("QQ")).unwrap();
    assert!(reasoning_row < message_row, "{rows:?}");
    assert!(rows[reasoning_row].contains("First consider the callers."));
    assert!(!rows.iter().any(|row| row.contains("Then the tests.")));
    // Reasoning never leaks into the assistant message body.
    assert!(
        !app.sessions[&session_id].messages.as_ref().unwrap()[0]
            .output
            .contains("consider")
    );

    app.execute(crate::commands::Command::ToggleReasoning);
    let rows = frame_rows(&transcript_lines(&app, 80));
    assert!(rows.iter().any(|row| row.contains("Then the tests.")));
    assert!(rows.iter().any(|row| row.contains("┆")));
}

/// `app_with_messages` plus a second warm session titled "Other" whose
/// messages read `other N`.
fn app_with_two_sessions(count: u8) -> (App, SessionId, SessionId) {
    let mut app = app_with_messages(count);
    let first = app.focused().unwrap();
    let other = SessionId::from_bytes([9; 16]);
    let mut summary = app.sessions[&first].summary.clone();
    summary.id = other;
    summary.title = "Other".to_owned();
    let messages = (0..count)
        .map(|row| {
            let mut message = completed_message(0x80 + row, format!("other {row}"));
            message.session_id = other;
            message
        })
        .collect();
    app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
        included: vec![SessionSnapshot {
            messages,
            ..fixtures::session_snapshot(summary.clone())
        }],
        cursor: fixtures::cursor(2),
        sessions: vec![summary],
        focused: None,
        ..fixtures::workspace_snapshot()
    }));
    (app, first, other)
}

#[test]
fn a_height_only_resize_keeps_the_transcript_cache() {
    let (mut app, _, other) = app_with_two_sessions(4);
    app.sidebar = crate::app::Sidebar::Hidden;
    app.focus_session(other);
    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 101, 24);
    assert_eq!(renderer.markdown().len(), 4);
    let width = renderer.markdown().values().next().unwrap().width;

    renderer.frame_and_commit(&mut app, 101, 30);
    assert_eq!(renderer.markdown().len(), 4);
    assert!(
        renderer
            .markdown()
            .values()
            .all(|cached| cached.width == width)
    );
}

#[test]
fn switching_theme_repaints_every_row_in_the_new_palette() {
    let mut app = app_with_messages(2);
    app.themes.push(crate::Theme::from_roles(
        "magenta",
        [crate::ThemeColor::Rgb(0xff, 0x00, 0xff); 8],
    ));
    let mut renderer = FrameRenderer::default();
    renderer.draw(&mut app, (80, 24)).unwrap();
    let brand_before = renderer.previous[0].spans[0].style.color;
    assert_eq!(brand_before, Some(Palette::QQ.brand));
    // A settled frame with nothing changed writes nothing.
    let idle = renderer.draw(&mut app, (80, 24)).unwrap();
    let idle_rows = String::from_utf8_lossy(&idle).matches("\x1b[2K").count();
    assert_eq!(idle_rows, 0);

    app.execute(Command::OpenThemes);
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.theme().name, "magenta");
    let repaint = renderer.draw(&mut app, (80, 24)).unwrap();
    let repainted_rows = String::from_utf8_lossy(&repaint).matches("\x1b[2K").count();
    assert_eq!(repainted_rows, 24, "every row is rewritten");
    let magenta = crossterm::style::Color::Rgb {
        r: 0xff,
        g: 0,
        b: 0xff,
    };
    assert_eq!(renderer.previous[0].spans[0].style.color, Some(magenta));
    // Only the picker's swatches (painted in each theme's own colors)
    // may show anything but the new palette.
    assert!(
        renderer
            .previous
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.text != "██")
            .filter_map(|span| span.style.color)
            .all(|color| color == magenta),
        "no row keeps a color from the previous theme"
    );
    // Style helpers on this thread keep the last activated palette;
    // restore the default so later tests see the compiled look.
    theme::activate(Palette::QQ);
}

/// An app whose focused session has an active run, plus the ids to drive it.
fn running_view_app() -> (App, SessionId, RunId, u64) {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let run_id = RunId::from_bytes([0x90; 16]);
    let mut summary = app.sessions[&session_id].summary.clone();
    summary.status = SessionStatus::Running;
    summary.active_run_id = Some(run_id);
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::RunStarted {
                session: summary,
                run_id,
                plan: None,
            },
        )
    }));
    (app, session_id, run_id, 2)
}

#[test]
fn a_finished_run_ends_with_a_completion_line_and_a_running_one_does_not() {
    let (mut app, session_id, run_id, _) = running_view_app();
    let message_id = MessageId::from_bytes([0x71; 16]);
    let mut sequence = 2;
    let mut event = |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: 10_000 + sequence * 1_000,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    app.apply_client_update(event(SessionEvent::AssistantMessageStarted {
        message: MessageSnapshot {
            run_id,
            state: MessageState::Streaming,
            ..fixtures::message(message_id, session_id, "working on it")
        },
    }));
    app.apply_client_update(event(SessionEvent::ToolCallFinished {
        tool_call: ToolCallSnapshot {
            run_id,
            result: Some("ok".to_owned()),
            ..fixtures::tool_call(ToolCallId::from_bytes([0x72; 16]), session_id, "shell")
        },
    }));
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 20);
    assert!(
        !frame_text(&frame).contains(" ✓ "),
        "no completion line while running"
    );

    let mut summary = app.sessions[&session_id].summary.clone();
    summary.status = SessionStatus::Idle;
    summary.active_run_id = None;
    // Three committed turns show as a count on the completion line.
    for turn in 1..=3 {
        app.apply_client_update(event(SessionEvent::ModelTurnCompleted {
            run_id,
            turn_ordinal: turn,
            model: ModelSelection::default(),
            usage: None,
            estimated_cost_usd_nanos: None,
        }));
    }
    // The run started at the fixture's occurred_at_ms (1) and finishes here;
    // duration comes from the envelopes, tokens from usage.
    app.apply_client_update(event(SessionEvent::RunFinished {
        session: summary,
        run_id,
        outcome: qq_protocol::RunOutcome::Completed,
        usage: Some(qq_protocol::TokenUsage {
            input_tokens: 12_000,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 300,
            reasoning_tokens: None,
        }),
        context_tokens: None,
    }));
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 20);
    let text = frame_text(&frame);
    let rows = frame_rows(&frame);
    let line = rows
        .iter()
        .find(|row| row.contains(" ✓ "))
        .unwrap_or_else(|| panic!("completion line in {text}"));
    assert!(line.contains("3 turns"), "{line}");
    assert!(line.contains("1 tool"), "{line}");
    assert!(line.contains("12.3k tok"), "{line}");
    assert!(line.contains('s'), "duration: {line}");
}

#[test]
fn no_role_style_relies_on_dim_and_muted_is_a_color_step_only() {
    // Dim is unreliable across terminals; every role reads by color and
    // weight alone so a theme can map roles to any palette.
    for style in [
        normal(),
        muted(),
        accent(),
        brand(),
        warning(),
        failure(),
        success(),
        info(),
        border(),
    ] {
        assert!(!style.dim, "{style:?}");
    }
    assert_ne!(muted().color, normal().color);
}

#[test]
fn an_expanded_running_shell_shows_started_live_elapsed_and_last_output_times() {
    let (mut app, session_id, run_id, _) = running_view_app();
    let call_id = ToolCallId::from_bytes([0x81; 16]);
    let mut sequence = 2;
    let mut event = |at_ms: u64, event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: at_ms,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    // 14:32:07 UTC on some day.
    let started = (14 * 3600 + 32 * 60 + 7) * 1000;
    app.apply_client_update(event(
        started - 1,
        SessionEvent::AssistantMessageStarted {
            message: MessageSnapshot {
                run_id,
                state: MessageState::Streaming,
                ..fixtures::message(
                    MessageId::from_bytes([0x80; 16]),
                    session_id,
                    "Running tests.",
                )
            },
        },
    ));
    app.apply_client_update(event(
        started,
        SessionEvent::ToolCallStarted {
            tool_call: ToolCallSnapshot {
                run_id,
                arguments: r#"{"command":"cargo test -p qq-auth"}"#.to_owned(),
                state: ToolCallState::Running,
                ..fixtures::tool_call(call_id, session_id, "shell")
            },
        },
    ));
    app.apply_client_update(event(
        started + 4 * 60 * 1000,
        SessionEvent::ToolCallOutputDelta {
            tool_call_id: call_id,
            chunk: "Compiling qq-core\n".to_owned(),
        },
    ));
    // Time passes: the animation tick advances the clock 125 ms at a time.
    for _ in 0..(12 * 8) {
        app.advance_animation();
    }

    // Collapsed: relative duration only, no wall-clock time.
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 24);
    let rows = frame_rows(&frame);
    let row = rows
        .iter()
        .find(|row| row.contains("Run "))
        .unwrap_or_else(|| panic!("tool row in {rows:#?}"));
    assert!(squash(row).contains("cargo test -p qq-auth"), "{row}");
    assert!(row.contains("4m12s"), "live elapsed: {row}");
    assert!(
        !row.contains("14:32:07"),
        "no wall-clock when collapsed: {row}"
    );

    // Expanded: started, running, and last output timestamps.
    app.expanded_tool_calls.insert(call_id);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 24);
    let text = frame_rows(&frame).join("\n");
    assert!(text.contains("started 14:32:07"), "{text}");
    assert!(text.contains("running 4m12s"), "{text}");
    assert!(text.contains("last output 14:36:07"), "{text}");

    // Another tick moves the elapsed clock.
    for _ in 0..8 {
        app.advance_animation();
    }
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 24);
    assert!(frame_rows(&frame).join("\n").contains("running 4m13s"));
}

#[test]
fn the_transcript_cursor_selects_a_call_and_enter_expands_only_that_one() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    session.tool_calls = Some(vec![
        tool_call_snapshot(
            1,
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolCallState::Completed,
            Some("alpha\n"),
            false,
        ),
        tool_call_snapshot(
            2,
            "read_file",
            r#"{"path":"b.rs"}"#,
            ToolCallState::Completed,
            Some("beta\n"),
            false,
        ),
    ]);
    let key = |code| TerminalEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL));
    // Ctrl-Up from nothing selects the newest call.
    app.handle_terminal_event(key(KeyCode::Up));
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24));
    let selected = rows
        .iter()
        .find(|row| row.contains("▶"))
        .expect("cursor row");
    assert!(squash(selected).contains("Read b.rs"), "{selected}");
    // Enter expands that call alone.
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let text = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24)).join("\n");
    assert!(text.contains("beta"), "{text}");
    assert!(!text.contains("alpha"), "{text}");
    // Ctrl-Up again moves to the older call; Esc clears the cursor.
    app.handle_terminal_event(key(KeyCode::Up));
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24));
    let selected = rows
        .iter()
        .find(|row| row.contains("▶"))
        .expect("cursor row");
    assert!(squash(selected).contains("Read a.rs"), "{selected}");
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24));
    assert!(rows.iter().all(|row| !row.contains("▶")));
}

#[test]
fn diffs_render_head_first_with_new_file_line_numbers() {
    let diff =
        "--- a/x.rs\n+++ b/x.rs\n@@ -10,3 +10,4 @@\n context\n-old\n+new one\n+new two\n tail\n";
    let rows = frame_rows(&diff_lines(diff, 20, 60));
    assert_eq!(
        rows.iter().map(|row| squash(row)).collect::<Vec<_>>(),
        [
            " @@ -10,3 +10,4 @@",
            " 10 context",
            " -old",
            " 11 +new one",
            " 12 +new two",
            " 13 tail",
        ]
    );
    // A long diff shows its head and says how much follows.
    let long: String = (0..30).map(|index| format!("+line {index}\n")).collect();
    let rows = frame_rows(&diff_lines(&format!("@@ -0,0 +1,30 @@\n{long}"), 5, 60));
    assert_eq!(rows.len(), 6);
    assert!(rows[1].contains("+line 0"));
    assert!(rows[5].contains("… 26 lines more"), "{:?}", rows[5]);
}

#[test]
fn paths_elide_from_the_middle_and_keep_the_file_name() {
    assert_eq!(
        elide_path("crates/qq-tui/src/view/tools.rs", 40),
        "crates/qq-tui/src/view/tools.rs"
    );
    assert_eq!(
        elide_path("crates/qq-tui/src/view/tools.rs", 24),
        "crates/qq-tui/…/tools.rs"
    );
    assert_eq!(
        elide_path("crates/qq-tui/src/view/tools.rs", 12),
        "…/tools.rs"
    );
    assert_eq!(elide_path("crates/qq-tui/src/view/tools.rs", 8), "…ools.rs");
}

/// A parent with a child session that is running and waiting on a `shell`
/// approval; the child's body is warm so the call is known client-side.
fn app_with_child_awaiting_approval() -> (App, SessionId, SessionId, RunId, ToolCallId) {
    let mut app = app_with_messages(1);
    app.sidebar = crate::app::Sidebar::Hidden;
    let parent = app.focused().unwrap();
    let child_id = SessionId::from_bytes([0x40; 16]);
    let run_id = RunId::from_bytes([0x41; 16]);
    let call_id = ToolCallId::from_bytes([0x42; 16]);
    let child = SessionSummary {
        parent_id: Some(parent),
        title: "Deploy helper".to_owned(),
        status: SessionStatus::Running,
        active_run_id: Some(run_id),
        model: None,
        estimated_cost_usd_nanos: None,
        updated_at_ms: 2,
        ..fixtures::session_summary(child_id)
    };
    // Warm the child through an included body so its calls are known.
    app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
        cursor: fixtures::cursor(1),
        sessions: vec![child.clone()],
        focused: None,
        included: vec![fixtures::session_snapshot(child.clone())],
        ..fixtures::workspace_snapshot()
    }));
    let call = ToolCallSnapshot {
        run_id,
        call_ordinal: 0,
        provider_call_id: "c".to_owned(),
        arguments: r#"{"command":"rm -rf build"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(call_id, child_id, "shell")
    };
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: 3,
        ..fixtures::envelope(
            2,
            child_id,
            SessionEvent::ToolApprovalRequested {
                tool_call: call,
                shell: None,
                edit: None,
            },
        )
    }));
    (app, parent, child_id, run_id, call_id)
}

#[test]
fn the_agent_strip_names_a_sibling_needing_approval_below_the_sidebar_width() {
    let (mut app, parent, _, _, _) = app_with_child_awaiting_approval();
    assert_eq!(app.focused(), Some(parent));
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 90, 24));
    let strip = rows
        .iter()
        .find(|row| row.contains("2 agents"))
        .unwrap_or_else(|| panic!("agent strip in {rows:#?}"));
    assert!(strip.contains("◇ 1"), "{strip}");
    assert!(strip.contains("Ctrl-G"), "{strip}");
    // The rule offers the in-place answer chords.
    let rule = rows
        .iter()
        .find(|row| row.contains("needs approval"))
        .unwrap_or_else(|| panic!("rule in {rows:#?}"));
    assert!(rule.contains("Alt-A/Alt-D answer"), "{rule}");
}

#[test]
fn a_background_approval_is_answered_in_place_without_moving_focus() {
    let (mut app, parent, child_id, run_id, call_id) = app_with_child_awaiting_approval();
    let (changed, requests) = app
        .handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
        )))
        .split();
    assert!(changed);
    assert_eq!(app.focused(), Some(parent), "focus stays put");
    assert!(matches!(
        requests.as_slice(),
        [ClientRequest::Command(CommandRequest {
            command: SessionCommand::RespondToolApproval {
                run_id: r,
                tool_call_id: c,
                decision: qq_protocol::ApprovalDecision::ApproveOnce,
            },
            ..
        })] if *r == run_id && *c == call_id
    ));
    assert!(
        !app.sessions_needing_attention().contains(&child_id) || {
            // Answered approvals stop counting as needing attention once the
            // server confirms; locally the row is already suppressed.
            app.pending_approval().is_none()
        }
    );
}

#[test]
fn shift_n_denies_and_steers_with_an_amendment() {
    let (mut app, _, child_id, run_id, call_id) = app_with_child_awaiting_approval();
    app.focus_session(child_id);
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        fixtures::steering_capabilities(),
    )));
    assert_eq!(app.mode(), Mode::Approval);
    let key = |code, modifiers| TerminalEvent::Key(KeyEvent::new(code, modifiers));
    app.handle_terminal_event(key(KeyCode::Char('N'), KeyModifiers::SHIFT));
    // The composer becomes the amendment field and shows the caret.
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24));
    assert!(
        rows.iter().any(|row| row.contains("deny and steer:")),
        "{rows:#?}"
    );
    for character in "use cargo clean".chars() {
        app.handle_terminal_event(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let (_, requests) = app
        .handle_terminal_event(key(KeyCode::Enter, KeyModifiers::NONE))
        .split();
    // Decision first, then an interrupting steer with the note.
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert!(matches!(
        &requests[0],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::RespondToolApproval {
                tool_call_id: c,
                decision: qq_protocol::ApprovalDecision::Deny,
                ..
            },
            ..
        }) if *c == call_id
    ));
    assert!(matches!(
        &requests[1],
        ClientRequest::Command(CommandRequest {
            command: SessionCommand::SteerRun { run_id: r, interrupt: true, input },
            ..
        }) if *r == run_id && input.len() == 1
    ));
    assert!(app.composer.text.is_empty());
    assert!(app.approval_amendment.is_none());
}

#[test]
fn the_sidebar_groups_sessions_by_what_the_user_should_do() {
    let (mut app, parent, child_id, _, _) = app_with_child_awaiting_approval();
    app.sidebar = crate::app::Sidebar::Shown;
    // A third session that finished while unfocused.
    let done_id = SessionId::from_bytes([0x50; 16]);
    let done_run = RunId::from_bytes([0x51; 16]);
    let mut done = SessionSummary {
        title: "Refactor".to_owned(),
        status: SessionStatus::Running,
        active_run_id: Some(done_run),
        ..fixtures::session_summary(done_id)
    };
    let mut sequence = 2;
    let mut event = |session_id, run_id, event| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    app.apply_client_update(event(
        done_id,
        done_run,
        SessionEvent::SessionCreated {
            session: done.clone(),
        },
    ));
    done.status = SessionStatus::Idle;
    done.active_run_id = None;
    done.last_outcome = Some(qq_protocol::RunOutcome::Completed);
    app.apply_client_update(event(
        done_id,
        done_run,
        SessionEvent::RunFinished {
            session: done,
            run_id: done_run,
            outcome: qq_protocol::RunOutcome::Completed,
            usage: None,
            context_tokens: None,
        },
    ));

    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 160, 30));
    let sidebar: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            row.split_once('│')
                .map(|(_, right)| right.trim_end().to_owned())
        })
        .collect();
    let text = sidebar.join("\n");
    let needs = text.find("NEEDS YOU").expect("needs-you group");
    let idle = text
        .find("IDLE")
        .expect("idle group for the focused parent");
    assert!(needs < idle, "needs-you first: {text}");
    // The awaiting child and the unread finish both need the user.
    assert!(text.contains("NEEDS YOU  2"), "{text}");
    assert!(text.contains("Deploy helper"), "{text}");
    assert!(text.contains("Refactor"), "{text}");
    assert!(text.contains("1 new"), "unread count: {text}");
    // Focusing the finished session clears its unread state and moves it to DONE.
    app.focus_session(done_id);
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 160, 30));
    let text = rows.join("\n");
    assert!(text.contains("DONE  1"), "{text}");
    assert!(text.contains("NEEDS YOU  1"), "{text}");
    let _ = (parent, child_id);
}

#[test]
fn the_attention_pane_lists_needs_most_urgent_first_and_the_changes_pane_flags_overlap() {
    let (mut app, _, child_id, _, _) = app_with_child_awaiting_approval();
    // A completed edit in the parent and an edit to the same file in the
    // child so the change board has an overlap to flag.
    let parent = app.focused().unwrap();
    for (session_id, byte) in [(parent, 0x61_u8), (child_id, 0x62)] {
        let mut calls = app.sessions[&session_id]
            .tool_calls
            .clone()
            .unwrap_or_default();
        calls.push(ToolCallSnapshot {
            display: Some(qq_protocol::ToolCallDisplay::Diff {
                path: "src/lib.rs".to_owned(),
                diff: "@@ -1 +1,2 @@\n-a\n+b\n+c\n".to_owned(),
            }),
            ..fixtures::tool_call(ToolCallId::from_bytes([byte; 16]), session_id, "edit_file")
        });
        app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(calls);
    }

    app.execute(Command::ShowAttention);
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24));
    let text = rows.join("\n");
    assert!(text.contains("NEEDS YOU"), "{text}");
    assert!(
        text.contains("Deploy helper") && text.contains("needs approval"),
        "{text}"
    );
    assert!(squash(&text).contains("Run rm -rf build"), "{text}");

    app.execute(Command::ShowChanges);
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24));
    let text = rows.join("\n");
    assert!(text.contains("CHANGES"), "{text}");
    let flagged = rows
        .iter()
        .find(|row| row.contains("src/lib.rs"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(flagged.contains("! "), "overlap flagged: {flagged}");
    assert!(flagged.contains("+4 −2"), "{flagged}");
    assert!(flagged.contains("2 agents"), "{flagged}");

    // Focusing a session returns to its transcript.
    app.focus_session(parent);
    assert_eq!(app.view, View::Transcript(Some(parent)));
}

#[test]
fn the_composer_rule_carries_run_telemetry_notices_and_hints_in_priority_order() {
    let (mut app, session_id, run_id, _) = running_view_app();
    let rule_at = |app: &mut App, width| {
        let rows = frame_rows(&FrameRenderer::default().frame_and_commit(app, width, 12));
        rows[rows.len() - 2].clone()
    };
    // Running: activity glyph, elapsed since the run started, hints right.
    // The chrome is two rows: no separate hint row exists below the composer.
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 12));
    assert!(
        rows[11].starts_with(" ⇥ "),
        "composer is the last row: {rows:#?}"
    );
    let started = 1_000;
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: started,
        ..fixtures::envelope(
            3,
            session_id,
            SessionEvent::RunActivityChanged {
                run_id,
                activity: qq_protocol::RunActivity::GeneratingResponse,
            },
        )
    }));
    app.sessions
        .get_mut(&session_id)
        .unwrap()
        .runs
        .get_mut(&run_id)
        .unwrap()
        .started_at_ms = Some(started);
    let message = MessageSnapshot {
        run_id,
        ..fixtures::message(MessageId::from_bytes([0x72; 16]), session_id, "")
    };
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: started + 600,
        ..fixtures::envelope(
            4,
            session_id,
            SessionEvent::AssistantMessageStarted { message },
        )
    }));
    for (sequence, at, text) in [(5, started + 600, "hi"), (6, started + 4_200, " there")] {
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: at,
            ..fixtures::envelope(
                sequence,
                session_id,
                SessionEvent::TextAppended {
                    message_id: MessageId::from_bytes([0x72; 16]),
                    channel: qq_protocol::TextChannel::Output,
                    text: text.to_owned(),
                },
            )
        }));
    }
    let rule = rule_at(&mut app, 100);
    assert!(rule.contains("generating 4.2s  ttft 0.6s"), "{rule}");
    assert!(rule.contains("F1 help"), "{rule}");
    assert!(rule.contains("─"), "{rule}");

    // Committed turns and their running cost join the rule as the run goes.
    for (sequence, turn, cost) in [(7, 1, 40_000_000), (8, 2, 60_000_000)] {
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: started + 5_000,
            ..fixtures::envelope(
                sequence,
                session_id,
                SessionEvent::ModelTurnCompleted {
                    run_id,
                    turn_ordinal: turn,
                    model: ModelSelection::default(),
                    usage: None,
                    estimated_cost_usd_nanos: Some(cost),
                },
            )
        }));
    }
    let rule = rule_at(&mut app, 140);
    assert!(rule.contains("turn 2  $0.10"), "{rule}");

    // A notice takes the left side and the hints step aside.
    app.apply_notice(None, crate::app::NoticeLevel::Info, "saved".to_owned());
    let rule = rule_at(&mut app, 100);
    assert!(rule.starts_with(" saved "), "{rule}");
    assert!(!rule.contains("F1 help"), "{rule}");
    app.status = None;

    // Cramped: status outranks hints, and some rule always shows.
    let rule = rule_at(&mut app, 40);
    assert!(rule.contains("generating"), "{rule}");
    assert!(!rule.contains("F1 help"), "{rule}");
    assert!(rule.contains("────"), "{rule}");
}

#[test]
fn profile_picker_lists_mode_pack_and_the_active_profile() {
    let mut app = app_with_messages(0);
    let mut capabilities = fixtures::steering_capabilities();
    capabilities.profiles = Some(vec![
        qq_protocol::AgentProfileSummary {
            id: qq_protocol::AgentProfileId::default(),
            model: Some("openai/gpt-test".to_owned()),
            approval_mode: qq_protocol::ApprovalMode::Auto,
            pack: None,
        },
        qq_protocol::AgentProfileSummary {
            id: qq_protocol::AgentProfileId::new("reviewer").unwrap(),
            model: None,
            approval_mode: qq_protocol::ApprovalMode::ReadOnly,
            pack: Some(qq_protocol::PackSummary {
                id: "review-kit".to_owned(),
                version: "1.0.0".to_owned(),
            }),
        },
    ]);
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        capabilities,
    )));
    app.open_profiles();

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let rows = squashed_rows(&frame);
    let text = rows.join("\n");
    assert!(text.contains("PROFILES"), "{text}");
    assert!(text.contains("Enter sets the session profile"), "{text}");
    let default_row = rows.iter().find(|row| row.contains("default")).unwrap();
    assert!(
        default_row.contains("auto") && default_row.contains("active"),
        "{default_row}"
    );
    let reviewer_row = rows.iter().find(|row| row.contains("reviewer")).unwrap();
    assert!(
        reviewer_row.contains("read_only") && reviewer_row.contains("pack review-kit@1.0.0"),
        "{reviewer_row}"
    );
}

#[test]
fn top_row_names_a_non_default_profile_only() {
    let mut app = app_with_messages(0);
    app.connection = crate::ConnectionState::Live;
    let plain = frame_rows(&[top_row(&app, 80)])[0].clone();
    assert!(!plain.contains("as "), "{plain}");

    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.profile = qq_protocol::AgentProfileId::new("reviewer").unwrap();
    let badged = frame_rows(&[top_row(&app, 80)])[0].clone();
    assert!(badged.contains("as reviewer"), "{badged}");
}

#[test]
fn approval_mode_picker_and_badge_name_the_mode_in_effect() {
    let mut app = app_with_messages(0);
    app.connection = crate::ConnectionState::Live;
    // `auto` is the default and shows no badge.
    let plain = frame_rows(&[top_row(&app, 80)])[0].clone();
    assert!(
        !plain.contains("auto") && !plain.contains("read_only"),
        "{plain}"
    );

    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.approval_mode = qq_protocol::ApprovalMode::ReadOnly;
    let badged = frame_rows(&[top_row(&app, 80)])[0].clone();
    assert!(badged.contains("read_only"), "{badged}");

    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        fixtures::steering_capabilities(),
    )));
    app.open_approval_modes();
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let rows = squashed_rows(&frame);
    let text = rows.join("\n");
    assert!(text.contains("APPROVAL MODE"), "{text}");
    let read_only = rows
        .iter()
        .find(|row| row.contains("read_only") && row.contains("deny"))
        .unwrap();
    assert!(read_only.contains("active"), "{read_only}");
    let full = rows
        .iter()
        .find(|row| row.contains("full") && row.contains("every tool"))
        .unwrap();
    assert!(full.contains("without asking"), "{full}");
}

#[test]
fn skills_picker_groups_commands_before_skills_with_sources() {
    let mut app = app_with_messages(0);
    let mut capabilities = fixtures::steering_capabilities();
    capabilities.workspace_tools = Some(qq_protocol::WorkspaceToolCapabilities {
        catalog_digest: qq_protocol::ContentHash::from_bytes([5; 32]),
        exposure: qq_protocol::ToolExposure::Full,
        hosts: Vec::new(),
        excluded_tools: 0,
        skills: qq_protocol::SkillCapabilities {
            digest: qq_protocol::ContentHash::from_bytes([6; 32]),
            indexed: 2,
            disclosed: 1,
            truncated: false,
            entries: vec![
                qq_protocol::SkillSummary {
                    name: "ship".to_owned(),
                    kind: qq_protocol::GuidanceKind::Command,
                    source: ".qq/commands/ship.md".to_owned(),
                    description: "Ship the current branch.".to_owned(),
                    disclosed: true,
                },
                qq_protocol::SkillSummary {
                    name: "audit".to_owned(),
                    kind: qq_protocol::GuidanceKind::Skill,
                    source: "pack:review-kit/skills/audit/SKILL.md".to_owned(),
                    description: String::new(),
                    disclosed: false,
                },
            ],
        },
    });
    app.apply_client_update(ClientUpdate::Capabilities(std::sync::Arc::new(
        capabilities,
    )));
    app.open_skills();
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 16);
    let rows = squashed_rows(&frame);
    let text = rows.join("\n");
    let commands = text.find("COMMANDS").unwrap();
    let ship = text.find("/ship").unwrap();
    let skills = text
        .find("SKILLS\n")
        .unwrap_or_else(|| text.rfind("SKILLS").unwrap());
    let audit = text.find("/audit").unwrap();
    assert!(commands < ship && ship < skills && skills < audit, "{text}");
    assert!(text.contains("Ship the current branch."), "{text}");
    assert!(
        text.contains("pack:review-kit/skills/audit/SKILL.md"),
        "{text}"
    );
    let audit_row = rows.iter().find(|row| row.contains("/audit")).unwrap();
    assert!(audit_row.contains("explicit only"), "{audit_row}");
}

#[test]
fn shell_approvals_show_the_server_preview_not_the_arguments() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    // The arguments say one thing; the server's preview (what will really
    // run, after its own normalization) says another. The preview wins.
    let tool_call = tool_call_snapshot(
        9,
        "shell",
        r#"{"command":"rm -rf build","cwd":"ignored"}"#,
        ToolCallState::AwaitingApproval,
        None,
        false,
    );
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(tool_call.run_id),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::ToolApprovalRequested {
                tool_call,
                shell: Some(qq_protocol::ShellCommandPreview {
                    command: "rm -rf ./build".to_owned(),
                    cwd: Some("crates/qq-tui".to_owned()),
                }),
                edit: None,
            },
        )
    }));

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 24);
    let rows = squashed_rows(&frame);
    let command_row = rows.iter().find(|row| row.contains("$ ")).unwrap();
    assert!(
        command_row.contains("rm -rf ./build") && command_row.contains("(in crates/qq-tui)"),
        "{command_row}"
    );
    assert!(!command_row.contains("ignored"), "{command_row}");
}

#[test]
fn the_completion_line_names_the_plan_and_an_overridden_route() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let run_id = RunId::from_bytes([0x91; 16]);
    let mut summary = app.sessions[&session_id].summary.clone();
    summary.status = SessionStatus::Running;
    summary.active_run_id = Some(run_id);
    let mut sequence = 1;
    let mut event = |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: 10_000 + sequence * 1_000,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    app.apply_client_update(event(SessionEvent::RunStarted {
        session: summary.clone(),
        run_id,
        plan: Some(Box::new(qq_protocol::RunPlanIdentity {
            profile: qq_protocol::AgentProfileId::new("reviewer").unwrap(),
            descriptor_version: 4,
            digest: qq_protocol::AgentPlanDigest::from_hash(qq_protocol::ContentHash::from_bytes(
                [0xab; 32],
            )),
            credential_epoch: qq_protocol::CredentialEpoch::new(1),
        })),
    }));
    app.apply_client_update(event(SessionEvent::AssistantMessageStarted {
        message: MessageSnapshot {
            run_id,
            ..fixtures::message(MessageId::from_bytes([0x73; 16]), session_id, "done")
        },
    }));
    // The reviewer profile pins a different model than the session selected.
    app.apply_client_update(event(SessionEvent::ModelTurnCompleted {
        run_id,
        turn_ordinal: 1,
        model: ModelSelection {
            model: Some("anthropic/claude-opus".to_owned()),
            max_output_tokens: None,
            organization: None,
        },
        usage: None,
        estimated_cost_usd_nanos: None,
    }));
    summary.status = SessionStatus::Idle;
    summary.active_run_id = None;
    app.apply_client_update(event(SessionEvent::RunFinished {
        session: summary,
        run_id,
        outcome: qq_protocol::RunOutcome::Completed,
        usage: None,
        context_tokens: None,
    }));
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 20);
    let rows = frame_rows(&frame);
    let line = rows.iter().find(|row| row.contains(" ✓ ")).unwrap();
    assert!(line.contains("on anthropic/claude-opus"), "{line}");
    assert!(line.contains("as reviewer · plan abababab"), "{line}");

    // A run on the session's own model with the default profile shows only
    // the digest: nothing to call out.
    let session = app.sessions.get_mut(&session_id).unwrap();
    let stats = session.runs.get_mut(&run_id).unwrap();
    stats.resolved_route = session.summary.model.clone();
    stats.plan = Some((
        qq_protocol::AgentProfileId::default(),
        "abababab".to_owned(),
    ));
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 20);
    let line = frame_rows(&frame)
        .into_iter()
        .find(|row| row.contains(" ✓ "))
        .unwrap();
    assert!(!line.contains("on "), "{line}");
    assert!(
        line.contains("plan abababab") && !line.contains("as "),
        "{line}"
    );
}
