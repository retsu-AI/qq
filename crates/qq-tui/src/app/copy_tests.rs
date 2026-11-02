use crossterm::event::MouseEvent;
use qq_protocol::{MessageId, MessageRole, SessionSnapshot, ToolCallId, ToolCallState};

use super::*;
use crate::{effect::Effect, fixtures, view::FrameRenderer};

const SIZE: (u16, u16) = (100, 30);

fn answer() -> &'static str {
    "Run this:\n\n```sh\ncargo test -p qq-tui\n```\n\nThen push."
}

fn fixed_instant() -> std::time::Instant {
    static AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *AT.get_or_init(|| std::time::Instant::now() + std::time::Duration::from_secs(60))
}

fn app() -> App {
    let summary = fixtures::session_summary(fixtures::SESSION);
    let mut app = App::new(TuiOptions::default());
    app.clock = fixed_instant;
    let user = qq_protocol::MessageSnapshot {
        role: MessageRole::User,
        turn_ordinal: 0,
        ..fixtures::message(
            MessageId::from_bytes([1; 16]),
            fixtures::SESSION,
            "how do I test?",
        )
    };
    let assistant = qq_protocol::MessageSnapshot {
        turn_ordinal: 0,
        ..fixtures::message(MessageId::from_bytes([2; 16]), fixtures::SESSION, answer())
    };
    let call = qq_protocol::ToolCallSnapshot {
        arguments: r#"{"command":"cargo test"}"#.to_owned(),
        state: ToolCallState::Completed,
        result: Some("test result: ok".to_owned()),
        run_id: assistant.run_id,
        turn_ordinal: 0,
        ..fixtures::tool_call(ToolCallId::from_bytes([3; 16]), fixtures::SESSION, "shell")
    };
    app.apply_client_update(ClientUpdate::Snapshot(qq_protocol::WorkspaceSnapshot {
        sessions: vec![summary.clone()],
        focused: Some(SessionSnapshot {
            messages: vec![user, assistant],
            tool_calls: vec![call],
            ..fixtures::session_snapshot(summary)
        }),
        ..fixtures::workspace_snapshot()
    }));
    app
}

fn draw(app: &mut App, renderer: &mut FrameRenderer) -> String {
    String::from_utf8(renderer.draw(app, SIZE).unwrap()).unwrap()
}

fn rows(app: &mut App, renderer: &mut FrameRenderer) -> Vec<String> {
    renderer
        .frame_and_commit(app, usize::from(SIZE.0), usize::from(SIZE.1))
        .iter()
        .map(|line| {
            let mut text = " ".repeat(usize::from(line.indent));
            text.extend(line.spans.iter().map(|span| span.text.as_str()));
            text
        })
        .collect()
}

fn find(rows: &[String], needle: &str) -> (u16, u16) {
    let row = rows.iter().position(|row| row.contains(needle)).unwrap();
    let column = rows[row].find(needle).unwrap();
    let column = rows[row][..column].chars().count();
    (u16::try_from(column).unwrap(), u16::try_from(row).unwrap())
}

fn mouse(kind: MouseEventKind, (column, row): (u16, u16)) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn copies(effects: Effects, renderer: &FrameRenderer) -> Vec<String> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Copy(copied) => Some(copied.text),
            Effect::CopySelection(selection) => Some(renderer.selection_text(&selection).text),
            _ => None,
        })
        .collect()
}

fn selection_background() -> String {
    let mut bytes = Vec::new();
    crossterm::queue!(
        bytes,
        crossterm::style::SetBackgroundColor(crate::theme::active().selection_bg)
    )
    .unwrap();
    String::from_utf8(bytes).unwrap()
}

fn drag(
    app: &mut App,
    renderer: &mut FrameRenderer,
    from: (u16, u16),
    to: (u16, u16),
) -> Vec<String> {
    app.last_click = None;
    app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), from));
    app.handle_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), to));
    draw(app, renderer);
    assert!(app.selection.is_some(), "the frame kept the selection");
    copies(
        app.handle_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), to)),
        renderer,
    )
}

#[test]
fn a_drag_copies_what_it_covers_in_either_direction() {
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    let start = find(&shown, "Run this");
    let end = find(&shown, "Then push.");
    let end = (end.0 + 9, end.1);
    let want = "Run this:\n\ncargo test -p qq-tui\n\nThen push.";
    app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), start));
    app.handle_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), end));
    let painted = draw(&mut app, &mut renderer);
    assert!(
        painted.contains(&selection_background()),
        "the selection is painted"
    );
    app.selection = None;
    draw(&mut app, &mut renderer);
    assert_eq!(drag(&mut app, &mut renderer, start, end), [want]);
    assert!(
        app.selection.is_some(),
        "the selection stays painted after release"
    );
    assert_eq!(drag(&mut app, &mut renderer, end, start), [want]);
    let below = (start.0, u16::try_from(shown.len()).unwrap() + 5);
    let all = drag(&mut app, &mut renderer, find(&shown, "how do I"), below);
    assert!(all[0].starts_with("how do I test?\n"), "{all:?}");
    assert!(
        !all[0].contains('┃') && !all[0].contains("Ask QQ"),
        "{all:?}"
    );
}

#[test]
fn code_panel_fill_is_not_copied_when_the_sessions_column_is_shown() {
    let mut app = app();
    app.layout.rail = crate::view::PanePref::Shown;
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    assert!(
        crate::view::rail_visible(usize::from(SIZE.0), app.layout, app.sessions.listed_count()),
        "the sessions column is on screen"
    );
    let code = find(&shown, "cargo test -p");
    let triple = {
        app.last_click = None;
        let mut clicked = Vec::new();
        for _ in 0..3 {
            app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), code));
            clicked = copies(
                app.handle_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), code)),
                &renderer,
            );
        }
        clicked
    };
    assert_eq!(triple, ["cargo test -p qq-tui"]);
}

#[test]
fn a_click_copies_nothing_and_shift_is_left_to_the_terminal() {
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let at = find(&rows(&mut app, &mut renderer), "Then push");
    app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), at));
    let up = app.handle_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), at));
    assert!(copies(up, &renderer).is_empty());
    assert!(app.selection.is_none());
    let shifted = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: at.0,
        row: at.1,
        modifiers: KeyModifiers::SHIFT,
    });
    assert!(app.handle_terminal_event(shifted).is_empty());
    assert!(app.selection.is_none());
}

#[test]
fn double_click_copies_a_word_and_triple_click_the_code_block() {
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    let word = find(&shown, "push");
    let mut clicked = Vec::new();
    for _ in 0..2 {
        app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), word));
        clicked = copies(
            app.handle_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), word)),
            &renderer,
        );
    }
    draw(&mut app, &mut renderer);
    assert_eq!(clicked, ["push."]);
    let code = find(&shown, "-p qq-tui");
    for _ in 0..3 {
        app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), code));
        clicked = copies(
            app.handle_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), code)),
            &renderer,
        );
    }
    assert_eq!(clicked, ["cargo test -p qq-tui"]);
    let stale = fixed_instant()
        - std::time::Duration::from_millis(u64::try_from(MULTI_CLICK_MS).unwrap() + 1);
    app.last_click = Some((stale, word, 1));
    app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), word));
    assert_eq!(app.last_click.map(|(_, _, clicks)| clicks), Some(1));
}

#[test]
fn keys_resize_and_changed_rows_clear_the_selection() {
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    let (from, to) = (find(&shown, "Run this"), find(&shown, "Then push"));
    drag(&mut app, &mut renderer, from, to);
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .redraws()
    );
    assert!(app.selection.is_none());
    drag(&mut app, &mut renderer, from, to);
    let typed = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(typed.redraws());
    assert!(app.selection.is_none());
    assert_eq!(app.composer.text, "x", "the key still does its job");
    app.composer.clear();
    drag(&mut app, &mut renderer, from, to);
    app.handle_terminal_event(Event::Resize(120, 30));
    assert!(app.selection.is_none());
    drag(&mut app, &mut renderer, from, to);
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    session.messages.as_mut().unwrap()[1].output = "Rewritten.".to_owned();
    draw(&mut app, &mut renderer);
    assert!(app.selection.is_none(), "rows changed under the selection");
}

#[test]
fn the_block_cursor_walks_every_block_and_y_copies_each_kind() {
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
    let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    let mut copied = Vec::new();
    for _ in 0..4 {
        app.handle_key(up);
        draw(&mut app, &mut renderer);
        copied.extend(copies(app.handle_key(y), &renderer));
    }
    assert_eq!(
        copied,
        [
            "$ cargo test\ntest result: ok",
            "cargo test -p qq-tui",
            answer(),
            "how do I test?",
        ]
    );
    assert!(app.composer.text.is_empty(), "y copied instead of typing");
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(
        copies(app.handle_key(ctrl_c), &renderer),
        ["how do I test?"]
    );
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.block_cursor.is_none());
    app.handle_key(y);
    assert_eq!(app.composer.text, "y", "without a cursor y types");
}

#[test]
fn y_on_a_code_block_in_a_long_streaming_answer_copies_that_block() {
    let mut app = app();
    let filler: String = (0..200).map(|line| format!("line {line}\n\n")).collect();
    let source = format!("```\nfirst\n```\n\n{filler}```\nsecond\n```\n");
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    let message = &mut session.messages.as_mut().unwrap()[1];
    message.output = source;
    message.state = qq_protocol::MessageState::Streaming;
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let code: Vec<Block> = app
        .pane()
        .blocks
        .iter()
        .map(|(block, _)| *block)
        .filter(|block| matches!(block, Block::Code(..)))
        .collect();
    assert_eq!(
        code.len(),
        1,
        "the first block scrolled out of the live window"
    );
    app.block_cursor = Some(code[0]);
    let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(copies(app.handle_key(y), &renderer), ["second"]);
}

#[test]
fn ctrl_c_copies_a_selection_and_quits_otherwise() {
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    let at = find(&shown, "Then push");
    app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), at));
    app.handle_terminal_event(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        (at.0 + 3, at.1),
    ));
    draw(&mut app, &mut renderer);
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let effects = app.handle_key(ctrl_c);
    assert!(!effects.iter().any(|effect| matches!(effect, Effect::Quit)));
    assert_eq!(copies(effects, &renderer), ["Then"]);
    assert!(!quits(app.handle_key(ctrl_c)));
    assert!(quits(app.handle_key(ctrl_c)));
}

fn quits(effects: Effects) -> bool {
    effects.iter().any(|effect| matches!(effect, Effect::Quit))
}

fn later_instant() -> std::time::Instant {
    fixed_instant() + std::time::Duration::from_millis(2001)
}

#[test]
fn ctrl_c_clears_a_draft_then_asks_before_quitting_within_two_seconds() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let mut app = app();
    app.composer.replace("half a thought".to_owned());
    assert!(!quits(app.handle_key(ctrl_c)));
    assert!(app.composer.text.is_empty());
    assert_eq!(app.visible_status(), None);

    assert!(!quits(app.handle_key(ctrl_c)));
    assert_eq!(
        app.visible_status(),
        Some(("Ctrl-C again to quit", NoticeLevel::Info))
    );
    app.clock = later_instant;
    assert!(!quits(app.handle_key(ctrl_c)), "the window is two seconds");
    assert!(quits(app.handle_key(ctrl_c)));

    let mut app = self::app();
    assert!(!quits(app.handle_key(ctrl_c)));
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    app.composer.clear();
    assert!(!quits(app.handle_key(ctrl_c)), "another key disarms it");
}

#[test]
fn a_copy_or_mouse_event_between_presses_disarms_the_quit() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let mut app = app();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    assert!(!quits(app.handle_terminal_event(Event::Key(ctrl_c))));
    let shown = rows(&mut app, &mut renderer);
    let at = find(&shown, "Then push");
    app.handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), at));
    app.handle_terminal_event(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        (at.0 + 3, at.1),
    ));
    draw(&mut app, &mut renderer);
    let copied = app.handle_terminal_event(Event::Key(ctrl_c));
    assert!(!quits(copied), "the press copies");
    assert!(
        !quits(app.handle_terminal_event(Event::Key(ctrl_c))),
        "the arm from before the drag is gone"
    );
    assert!(quits(app.handle_terminal_event(Event::Key(ctrl_c))));

    let mut app = self::app();
    assert!(!quits(app.handle_terminal_event(Event::Key(ctrl_c))));
    app.handle_terminal_event(mouse(MouseEventKind::ScrollUp, (1, 1)));
    assert!(!quits(app.handle_terminal_event(Event::Key(ctrl_c))));
}

#[test]
fn releasing_ctrl_c_keeps_the_quit_armed() {
    let press = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let release = KeyEvent {
        kind: crossterm::event::KeyEventKind::Release,
        ..press
    };
    let mut app = app();
    assert!(!quits(app.handle_terminal_event(Event::Key(press))));
    assert!(!quits(app.handle_terminal_event(Event::Key(release))));
    assert!(quits(app.handle_terminal_event(Event::Key(press))));
}

#[test]
fn the_first_ctrl_c_counts_the_runs_that_stop_only_when_this_qq_owns_the_server() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    for (owns_server, expected) in [
        (true, "Ctrl-C again to quit; 1 run will stop"),
        (false, "Ctrl-C again to quit"),
    ] {
        let mut app = app();
        app.owns_server = owns_server;
        app.sessions
            .get_mut(&fixtures::SESSION)
            .unwrap()
            .summary
            .active_run_id = Some(qq_protocol::RunId::from_bytes([9; 16]));
        app.handle_key(ctrl_c);
        assert_eq!(app.visible_status().map(|(text, _)| text), Some(expected));
    }
}

#[test]
fn slash_copy_takes_the_last_answer_or_its_last_code_block() {
    let mut app = app();
    let renderer = FrameRenderer::default();
    app.composer.text = "/copy".to_owned();
    assert_eq!(copies(app.submit_prompt(), &renderer), [answer()]);
    app.composer.text = "/copy code".to_owned();
    assert_eq!(
        copies(app.submit_prompt(), &renderer),
        ["cargo test -p qq-tui"]
    );
    assert!(app.composer.text.is_empty());
    let mut empty = self::app();
    empty
        .sessions
        .get_mut(&fixtures::SESSION)
        .unwrap()
        .messages
        .as_mut()
        .unwrap()
        .retain(|message| message.role == MessageRole::User);
    empty.composer.text = "/copy".to_owned();
    assert!(copies(empty.submit_prompt(), &renderer).is_empty());
    assert_eq!(empty.status.as_deref(), Some("no answer to copy yet"));
}

#[test]
fn copies_keep_a_refusal_and_running_tool_output() {
    let mut app = app();
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    session.messages.as_mut().unwrap()[1].refusal = "I can't run that.".to_owned();
    let call = &mut session.tool_calls.as_mut().unwrap()[0];
    call.state = ToolCallState::Running;
    call.result = None;
    session.live_tool_output.insert(
        ToolCallId::from_bytes([3; 16]),
        "Compiling qq-core\n".to_owned(),
    );
    let renderer = FrameRenderer::default();
    app.composer.text = "/copy".to_owned();
    assert_eq!(
        copies(app.submit_prompt(), &renderer),
        [format!("{}\n\nI can't run that.", answer())]
    );
    app.block_cursor = Some(Block::Tool(ToolCallId::from_bytes([3; 16])));
    let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(
        copies(app.handle_key(y), &renderer),
        ["$ cargo test\nCompiling qq-core"]
    );
}

#[test]
fn the_code_cursor_keeps_its_block_when_another_streams_in() {
    let mut app = app();
    let set = |app: &mut App, output: &str| {
        let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
        let message = &mut session.messages.as_mut().unwrap()[1];
        message.output = output.to_owned();
        message.state = qq_protocol::MessageState::Streaming;
    };
    set(&mut app, "```\nfirst\n```\n\nmore\n");
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let code = app
        .pane()
        .blocks
        .iter()
        .map(|(block, _)| *block)
        .find(|block| matches!(block, Block::Code(..)))
        .unwrap();
    app.block_cursor = Some(code);
    set(&mut app, "```\nfirst\n```\n\nmore\n\n```\nsecond\n```\n");
    draw(&mut app, &mut renderer);
    let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(copies(app.handle_key(y), &renderer), ["first"]);
}

#[test]
fn a_wrapped_prose_word_beside_the_sessions_column_copies_without_margin() {
    let mut app = app();
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    session.messages.as_mut().unwrap()[1].output = format!("{}b", "a".repeat(80));
    app.layout.rail = crate::view::PanePref::Shown;
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    let start = find(&shown, "aaaa");
    let end = (start.0 + 30, start.1 + 1);
    let copied = drag(&mut app, &mut renderer, start, end);
    assert_eq!(copied, [format!("{}b", "a".repeat(80))]);
}

#[test]
fn an_exec_copy_quotes_each_argument_as_it_ran() {
    let call = qq_protocol::ToolCallSnapshot {
        arguments: r#"{"program":"cat","args":["file with spaces.txt","","it's","-n"]}"#.to_owned(),
        ..fixtures::tool_call(ToolCallId::from_bytes([9; 16]), fixtures::SESSION, "exec")
    };
    assert_eq!(
        crate::view::tool_copy_text(&call, None),
        r"$ cat 'file with spaces.txt' '' 'it'\''s' -n"
    );
}

#[test]
fn a_full_width_code_row_beside_the_sessions_column_copies_without_margin() {
    let code = format!("{}b", "a".repeat(80));
    let mut app = app();
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    session.messages.as_mut().unwrap()[1].output = format!("```\n{code}\n```");
    app.layout.rail = crate::view::PanePref::Shown;
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let shown = rows(&mut app, &mut renderer);
    let start = find(&shown, "aaaa");
    let end = (start.0 + 10, start.1 + 1);
    let copied = drag(&mut app, &mut renderer, start, end);
    assert!(copied[0].starts_with("aaaa"), "{copied:?}");
    assert!(!copied[0].contains(' '), "{copied:?}");
}

#[test]
fn folded_tool_calls_stay_reachable_by_the_block_cursor() {
    let mut app = app();
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    let calls = session.tool_calls.as_mut().unwrap();
    let first = calls[0].clone();
    calls.extend((4..7).map(|byte| qq_protocol::ToolCallSnapshot {
        id: ToolCallId::from_bytes([byte; 16]),
        ..first.clone()
    }));
    app.receipts_open = false;
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let tools: Vec<Block> = app
        .pane()
        .blocks
        .iter()
        .map(|(block, _)| *block)
        .filter(|block| matches!(block, Block::Tool(_)))
        .collect();
    assert_eq!(tools.len(), 4, "{tools:?}");
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
    draw(&mut app, &mut renderer);
    let selected = app.block_cursor.expect("the cursor landed");
    assert!(matches!(selected, Block::Tool(_)), "{selected:?}");
}

#[test]
fn copy_notices_say_whether_a_helper_confirmed_the_write() {
    let mut app = app();
    app.apply_copy_result(12, false, true);
    assert_eq!(app.status.as_deref(), Some("copied 12 lines"));
    app.apply_copy_result(1, false, false);
    assert_eq!(
        app.status.as_deref(),
        Some("sent 1 line to the terminal clipboard")
    );
    app.apply_copy_result(3, true, true);
    assert_eq!(app.status.as_deref(), Some("copied 3 lines, cut at 1 MiB"));
}

#[test]
fn mouse_off_says_what_it_does_and_draws_the_transcript_flush() {
    let mut app = app();
    let effects = app.execute(Command::ToggleMouse);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::MouseCapture(false)))
    );
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .starts_with("mouse off: the terminal selects")
    );
    let mut renderer = FrameRenderer::default();
    let shown = rows(&mut app, &mut renderer);
    assert!(
        shown.iter().any(|row| row.starts_with("┃ how do I test?")),
        "{shown:#?}"
    );
    app.execute(Command::ToggleMouse);
    assert!(
        app.status
            .as_deref()
            .unwrap()
            .starts_with("mouse on: drag copies")
    );
}
