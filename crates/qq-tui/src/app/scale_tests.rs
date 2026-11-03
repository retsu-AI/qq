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
fn changing_session_closes_find_so_its_matches_never_act_on_another_transcript() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "needle");
    assert!(app.pane().reveal.is_some());
    app.set_view(crate::viewport::View::Transcript(Some(
        fixtures::session_id(9),
    )));
    assert_eq!(app.mode(), Mode::Compose);
    assert_eq!(app.pane().reveal, None);
    assert_eq!(app.block_cursor, None);
}

#[test]
fn ctrl_c_with_find_open_closes_find_and_a_second_press_quits() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "needle");
    assert!(app.block_cursor.is_some());
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let first = app.handle_key(ctrl_c);
    assert!(
        !first
            .into_iter()
            .any(|effect| matches!(effect, crate::effect::Effect::Copy(_))),
        "Ctrl-C does not copy the find match"
    );
    assert_eq!(app.mode(), Mode::Compose);
    assert_eq!(app.block_cursor, None);
    app.handle_key(ctrl_c);
    let quit = app.handle_key(ctrl_c);
    assert!(
        quit.into_iter()
            .any(|effect| matches!(effect, crate::effect::Effect::Quit))
    );
}

#[test]
fn find_ignores_case_beyond_ascii_and_clears_the_cursor_without_a_match() {
    let mut app = app(30);
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    session.messages.as_mut().unwrap()[3].output = "Résumé ÉTÉ".to_owned();
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "été");
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("/ été  1 of 1")),
        "{rows:#?}"
    );
    app.handle_key(key(KeyCode::Backspace));
    app.handle_key(key(KeyCode::Backspace));
    app.handle_key(key(KeyCode::Backspace));
    type_text(&mut app, "ÉTÉ");
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("/ ÉTÉ  1 of 1")),
        "{rows:#?}"
    );
    type_text(&mut app, "zzz");
    assert_eq!(app.block_cursor, None, "no match leaves no cursor behind");
}

#[test]
fn page_keys_scroll_the_transcript_even_with_an_open_call_selected() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let call = ToolCallId::from_bytes([3; 16]);
    app.block_cursor = Some(Block::Tool(call));
    app.handle_key(key(KeyCode::Enter));
    draw(&mut app, &mut renderer);
    let before = app.viewport().offset();
    app.handle_key(key(KeyCode::PageUp));
    draw(&mut app, &mut renderer);
    let up = app.viewport().offset();
    assert!(up > before, "PageUp scrolls the transcript");
    app.handle_key(key(KeyCode::PageDown));
    draw(&mut app, &mut renderer);
    assert!(app.viewport().offset() < up, "PageDown scrolls it back");
}

#[test]
fn find_shows_the_match_inside_a_call_the_user_already_expanded_and_leaves_it_open() {
    let mut app = app(1);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let call = ToolCallId::from_bytes([3; 16]);
    app.block_cursor = Some(Block::Tool(call));
    app.handle_key(key(KeyCode::Enter));
    draw(&mut app, &mut renderer);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "needle-in-tool");
    let text = draw(&mut app, &mut renderer).join("\n");
    assert!(text.contains("needle-in-tool-output"), "{text}");
    app.handle_key(key(KeyCode::Esc));
    assert!(
        app.expanded_tool_calls.contains(&call),
        "find does not fold what the user opened"
    );
}

#[test]
fn end_with_find_open_returns_to_the_tail_even_with_a_draft() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    type_text(&mut app, "draft");
    app.handle_key(key(KeyCode::PageUp));
    draw(&mut app, &mut renderer);
    assert!(app.viewport().offset() > 0);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::End));
    assert_eq!(app.viewport().offset(), 0);
    assert_eq!(app.composer.text, "draft");
}

#[test]
fn find_scrolls_to_the_matching_row_inside_a_block_taller_than_the_view() {
    let mut app = app(30);
    let text = (0..60)
        .map(|row| format!("thought {row}"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("thought 55", "thought 55 gemstone");
    app.sessions
        .get_mut(&fixtures::SESSION)
        .unwrap()
        .reasoning
        .get_mut(&fixtures::run_id(2))
        .unwrap()
        .text = text;
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "gemstone");
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("thought 55 gemstone")),
        "{rows:#?}"
    );
    app.handle_key(key(KeyCode::Enter));
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("thought 55 gemstone")),
        "stepping to the sole match stays on its row: {rows:#?}"
    );
}

#[test]
fn expanding_a_block_while_scrolled_up_during_a_run_is_not_new_output() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    stream(&mut app, "");
    draw(&mut app, &mut renderer);
    for _ in 0..3 {
        app.handle_key(key(KeyCode::PageUp));
    }
    draw(&mut app, &mut renderer);
    app.block_cursor = Some(Block::Tool(ToolCallId::from_bytes([3; 16])));
    app.handle_key(key(KeyCode::Enter));
    let rows = draw(&mut app, &mut renderer);
    assert_eq!(app.viewport().unseen(), 0, "{}", rule(&rows));
    app.block_cursor = Some(Block::Reasoning(fixtures::run_id(2)));
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
    let rows = draw(&mut app, &mut renderer);
    assert_eq!(app.viewport().unseen(), 0, "{}", rule(&rows));
    stream(&mut app, "\n\nreal output");
    draw(&mut app, &mut renderer);
    assert_eq!(app.viewport().unseen(), 2, "real output still counts");
}

#[test]
fn alt_r_twice_hides_reasoning_opened_one_at_a_time() {
    let mut app = app(1);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    app.block_cursor = Some(Block::Reasoning(fixtures::run_id(2)));
    app.handle_key(key(KeyCode::Enter));
    let rows = draw(&mut app, &mut renderer);
    assert!(rows.iter().any(|row| row.contains("weighed the")));
    let alt_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
    app.handle_key(alt_r);
    app.handle_key(alt_r);
    let rows = draw(&mut app, &mut renderer);
    assert!(
        !rows.iter().any(|row| row.contains("weighed the")),
        "{rows:#?}"
    );
}

#[test]
fn find_reveals_a_match_in_running_output_above_its_live_tail() {
    let mut app = app(1);
    let call = ToolCallId::from_bytes([3; 16]);
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    let running = &mut session.tool_calls.as_mut().unwrap()[0];
    running.state = qq_protocol::ToolCallState::Running;
    running.result = None;
    session.live_tool_output.insert(
        call,
        format!("{}\nlive-needle\n{}\n", shell_output(5), shell_output(20)),
    );
    let mut renderer = FrameRenderer::default();
    let rows = draw(&mut app, &mut renderer);
    assert!(!rows.iter().any(|row| row.contains("live-needle")));
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "live-needle");
    let text = draw(&mut app, &mut renderer).join("\n");
    assert!(text.contains("│ live-needle"), "{text}");
    assert!(!text.contains("PgUp"), "{text}");
}

#[test]
fn find_picks_up_text_that_streams_in_after_the_query() {
    let mut app = app(1);
    let mut renderer = FrameRenderer::default();
    stream(&mut app, "");
    draw(&mut app, &mut renderer);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "latecomer");
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("no matches")),
        "{rows:#?}"
    );
    app.apply_client_update(ClientUpdate::Event(fixtures::envelope(
        2,
        fixtures::SESSION,
        qq_protocol::SessionEvent::TextAppended {
            message_id: MessageId::from_bytes([10; 16]),
            channel: qq_protocol::TextChannel::Output,
            text: " latecomer".to_owned(),
        },
    )));
    let rows = draw(&mut app, &mut renderer);
    assert!(
        rows.iter().any(|row| row.contains("/ latecomer  1 of 1")),
        "the first frame after the text already shows the match: {rows:#?}"
    );
}

#[test]
fn output_that_lands_with_the_run_finishing_still_counts_as_new() {
    let mut app = app(30);
    let mut renderer = FrameRenderer::default();
    stream(&mut app, "");
    draw(&mut app, &mut renderer);
    app.handle_key(key(KeyCode::PageUp));
    draw(&mut app, &mut renderer);
    app.apply_client_update(ClientUpdate::Event(fixtures::envelope(
        2,
        fixtures::SESSION,
        qq_protocol::SessionEvent::TextAppended {
            message_id: MessageId::from_bytes([39; 16]),
            channel: qq_protocol::TextChannel::Output,
            text: "\n\nfinal paragraph".to_owned(),
        },
    )));
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    session.summary.active_run_id = None;
    session.summary.status = qq_protocol::SessionStatus::Idle;
    let rows = draw(&mut app, &mut renderer);
    assert_eq!(app.viewport().unseen(), 2, "{}", rule(&rows));
}

#[test]
fn find_reveals_an_early_error_in_a_failed_command() {
    let mut app = app(1);
    let session = app.sessions.get_mut(&fixtures::SESSION).unwrap();
    let failed = &mut session.tool_calls.as_mut().unwrap()[0];
    failed.state = qq_protocol::ToolCallState::Failed;
    failed.is_error = true;
    failed.result = Some(format!(
        "shell exit=101\nerror[E0308]: mismatched types\n{}",
        shell_output(20)
    ));
    let mut renderer = FrameRenderer::default();
    let rows = draw(&mut app, &mut renderer);
    assert!(!rows.iter().any(|row| row.contains("E0308")), "{rows:#?}");
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "E0308");
    let text = draw(&mut app, &mut renderer).join("\n");
    assert!(text.contains("│ error[E0308]: mismatched types"), "{text}");
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
    session.summary.active_run_id = Some(fixtures::run_id(2));
    session.summary.status = qq_protocol::SessionStatus::Running;
    let message = session.messages.as_mut().unwrap().last_mut().unwrap();
    message.state = MessageState::Streaming;
    let message_id = message.id;
    if text.is_empty() {
        return;
    }
    let sequence = app.last_sequence + 1;
    app.apply_client_update(ClientUpdate::Event(fixtures::envelope(
        sequence.max(2),
        fixtures::SESSION,
        qq_protocol::SessionEvent::TextAppended {
            message_id,
            channel: qq_protocol::TextChannel::Output,
            text: text.to_owned(),
        },
    )));
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
fn an_open_call_shows_its_whole_output_and_folding_it_returns_the_answer() {
    let mut app = app(1);
    let mut renderer = FrameRenderer::default();
    draw(&mut app, &mut renderer);
    let call = ToolCallId::from_bytes([3; 16]);
    app.block_cursor = Some(Block::Tool(call));
    app.handle_key(key(KeyCode::Enter));
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..8 {
        for row in draw(&mut app, &mut renderer) {
            if let Some(line) = row.trim_start().strip_prefix("│ out ") {
                seen.insert(line.to_owned());
            }
        }
        app.handle_key(key(KeyCode::PageDown));
    }
    assert_eq!(
        seen.len(),
        20,
        "every output row is reachable by scrolling: {seen:?}"
    );
    assert!(!draw(&mut app, &mut renderer).join("\n").contains("PgUp"));

    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));
    let text = draw(&mut app, &mut renderer).join("\n");
    assert!(!text.contains("│ out 05"), "folded again: {text}");
    assert!(text.contains("answer 0 paragraph"), "{text}");
}
