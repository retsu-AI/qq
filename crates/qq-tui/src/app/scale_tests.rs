use qq_client::state::Reasoning;
use qq_protocol::{MessageId, MessageRole, MessageState, SessionSnapshot, ToolCallId};

use super::*;
use crate::{fixtures, view::FrameRenderer};

const SIZE: (usize, usize) = (100, 24);

fn shell_output(lines: usize) -> String {
    (1..=lines)
        .map(|line| format!("out {line:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn app(answers: u8) -> App {
    let summary = fixtures::session_summary(fixtures::SESSION);
    let mut app = App::new(TuiOptions::default());
    let prompt = qq_protocol::MessageSnapshot {
        role: MessageRole::User,
        turn_ordinal: 0,
        ..fixtures::message(MessageId::from_bytes([1; 16]), fixtures::SESSION, "go")
    };
    let mut messages = vec![prompt];
    messages.extend((0..answers).map(|index| qq_protocol::MessageSnapshot {
        turn_ordinal: u32::from(index) + 1,
        ..fixtures::message(
            MessageId::from_bytes([index + 10; 16]),
            fixtures::SESSION,
            &format!("answer {index} paragraph"),
        )
    }));
    let call = qq_protocol::ToolCallSnapshot {
        arguments: r#"{"command":"cargo test"}"#.to_owned(),
        result: Some(format!(
            "{}\nneedle-in-tool-output\n{}",
            shell_output(20),
            shell_output(19)
        )),
        turn_ordinal: 0,
        ..fixtures::tool_call(ToolCallId::from_bytes([3; 16]), fixtures::SESSION, "shell")
    };
    app.apply_client_update(ClientUpdate::Snapshot(qq_protocol::WorkspaceSnapshot {
        sessions: vec![summary.clone()],
        focused: Some(SessionSnapshot {
            messages,
            tool_calls: vec![call],
            ..fixtures::session_snapshot(summary)
        }),
        ..fixtures::workspace_snapshot()
    }));
    app.layout.rail = crate::view::PanePref::Hidden;
    app.layout.inspector = crate::view::PanePref::Hidden;
    app.sessions
        .get_mut(&fixtures::SESSION)
        .unwrap()
        .reasoning
        .insert(
            fixtures::run_id(2),
            Reasoning {
                text: "weighed the needle-in-reasoning option".to_owned(),
                streaming: false,
                ticks: 16,
            },
        );
    app
}

fn draw(app: &mut App, renderer: &mut FrameRenderer) -> Vec<String> {
    renderer
        .frame_and_commit(app, SIZE.0, SIZE.1)
        .iter()
        .map(|line| {
            let mut text = " ".repeat(usize::from(line.indent));
            text.extend(line.spans.iter().map(|span| span.text.as_str()));
            text.trim_end().to_owned()
        })
        .collect()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
}

fn rule(rows: &[String]) -> &str {
    rows.iter()
        .rev()
        .find(|row| row.contains('─'))
        .expect("the dock has a rule")
}

#[test]
fn ctrl_f_finds_text_inside_collapsed_tool_output_and_reasoning_and_opens_each_match() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    let rows = draw(&mut app, &mut renderer);
    assert!(
        !rows.iter().any(|row| row.contains("needle")),
        "both matches start collapsed and off screen: {rows:#?}"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(app.mode(), Mode::Find);
    type_text(&mut app, "NEEDLE");
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("/ NEEDLE  2 of 2")),
        "from the tail, the nearest match above comes first: {rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("weighed the needle-in-reasoning")),
        "the reasoning opened and scrolled into view: {rows:#?}"
    );
    assert!(app.composer.text.is_empty(), "typing went to the query");

    app.handle_key(key(KeyCode::Enter));
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("1 of 2")),
        "Enter wraps: {rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("needle-in-tool-output")),
        "the tool call opened at the match: {rows:#?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("weighed the")),
        "the previous match folded back"
    );

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    let rows = draw(&mut app, &mut renderer);
    assert!(rows.iter().any(|row| row.contains("2 of 2")), "{rows:#?}");

    app.handle_key(key(KeyCode::Esc));
    let rows = draw(&mut app, &mut renderer);
    assert_eq!(app.mode(), Mode::Compose);
    assert!(
        app.expanded_tool_calls.is_empty(),
        "the tool match find opened earlier folded back"
    );
    assert_eq!(
        app.open_reasoning.len(),
        1,
        "the match being read stays open"
    );
    assert!(rows.iter().any(|row| row.contains("weighed the needle")));
    assert!(!rows.iter().any(|row| row.contains("/ NEEDLE")));
    assert_eq!(
        app.block_cursor, None,
        "closing find leaves no block cursor"
    );
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.handle_key(ctrl_c);
    let quit = app.handle_key(ctrl_c);
    assert!(
        quit.into_iter()
            .any(|effect| matches!(effect, crate::effect::Effect::Quit)),
        "Ctrl-C twice still quits after find"
    );
}

#[test]
fn find_with_no_match_says_so_and_moves_nothing() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let before = app.viewport().offset();
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "absent");
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("/ absent  no matches")),
        "{rows:#?}"
    );
    app.handle_key(key(KeyCode::Esc));
    draw(&mut app, &mut renderer);
    assert_eq!(app.viewport().offset(), before);
    assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
}

fn stream(app: &mut App, text: &str) {
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    let message = session.messages.as_mut().unwrap().last_mut().unwrap();
    message.state = MessageState::Streaming;
    message.output.push_str(text);
}

#[test]
fn streaming_below_a_scrolled_view_keeps_it_still_and_counts_new_rows_on_the_rule() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    stream(&mut app, "");
    draw(&mut app, &mut renderer);
    app.handle_key(key(KeyCode::PageUp));
    let rows = draw(&mut app, &mut renderer);
    let shown: Vec<String> = rows[..10].to_vec();
    assert!(!rule(&rows).contains("new"), "{}", rule(&rows));

    for chunk in 0..3 {
        stream(&mut app, &format!("\n\nstreamed paragraph {chunk}"));
        let rows = draw(&mut app, &mut renderer);
        assert_eq!(rows[..10], shown[..], "the view did not move");
    }
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rule(&rows).contains("↓ 6 new  End"),
        "two rows per paragraph: {}",
        rule(&rows)
    );

    assert!(app.composer.text.is_empty());
    app.handle_key(key(KeyCode::End));
    let rows = draw(&mut app, &mut renderer);
    assert_eq!(app.viewport().offset(), 0);
    assert!(!rule(&rows).contains("new"), "{}", rule(&rows));
    assert!(
        rows.iter().any(|row| row.contains("streamed paragraph 2")),
        "{rows:#?}"
    );
}

#[test]
fn end_with_a_draft_moves_the_caret_and_ctrl_end_still_returns_to_the_tail() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    app.handle_key(key(KeyCode::PageUp));
    draw(&mut app, &mut renderer);
    type_text(&mut app, "draft");
    app.handle_key(key(KeyCode::Home));
    app.handle_key(key(KeyCode::End));
    assert_eq!(app.composer.cursor(), 5);
    assert!(app.viewport().offset() > 0, "End edited the draft instead");
    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    assert_eq!(app.viewport().offset(), 0);
}

#[test]
fn an_expanded_call_scrolls_inside_its_own_block_and_keeps_the_answer_on_screen() {
    let mut app = app(1);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let call = ToolCallId::from_bytes([3; 16]);
    app.block_cursor = Some(Block::Tool(call));
    app.handle_key(key(KeyCode::Enter));
    let rows = draw(&mut app, &mut renderer);
    let text = rows.join("\n");
    assert!(text.contains("33–40 of 40 · PgUp PgDn"), "{text}");
    assert!(
        text.contains("out 19") && !text.contains("out 11\n"),
        "{text}"
    );
    assert!(
        text.contains("answer 0 paragraph"),
        "the answer stays: {text}"
    );

    app.handle_key(key(KeyCode::PageUp));
    let rows = draw(&mut app, &mut renderer);
    let text = rows.join("\n");
    assert!(text.contains("25–32 of 40"), "{text}");
    assert_eq!(app.viewport().offset(), 0, "the transcript did not scroll");
    assert!(text.contains("answer 0 paragraph"), "{text}");

    for _ in 0..5 {
        app.handle_key(key(KeyCode::PageUp));
    }
    let text = draw(&mut app, &mut renderer).join("\n");
    assert!(text.contains("1–8 of 40"), "clamped at the top: {text}");

    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::PageUp));
    let text = draw(&mut app, &mut renderer).join("\n");
    assert!(!text.contains(" of 40"), "collapsed again: {text}");
    assert!(app.tool_scroll.is_empty(), "the window position is dropped");
}
