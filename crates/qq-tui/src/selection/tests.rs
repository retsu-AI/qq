use qq_protocol::MessageRole;

use super::*;
use crate::render::{Join, Line, RowKind, normal};

fn area(width: usize, height: usize) -> Area {
    Area {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn shapes(frame: &[Line]) -> Vec<RowShape> {
    frame.iter().map(RowShape::of).collect()
}

fn select_all(frame: &[Line], width: usize) -> String {
    let mut selection = Selection::start(area(width, frame.len()), 0, 0, Unit::Cell).unwrap();
    selection.extend(width - 1, frame.len() - 1);
    selection.text(frame, &shapes(frame)).text
}

fn inset(mut frame: Vec<Line>, margin: u16) -> Vec<Line> {
    for line in &mut frame {
        line.indent = margin;
    }
    frame
}

#[test]
fn the_message_rail_and_pane_margin_are_chrome() {
    let frame = inset(
        crate::view::message_rows_for_test("make the test pass", MessageRole::User, 40),
        2,
    );
    assert_eq!(select_all(&frame, 44), "make the test pass");
    let answer = inset(
        crate::view::message_rows_for_test(
            "Done. Every file compiles.",
            MessageRole::Assistant,
            40,
        ),
        2,
    );
    assert_eq!(select_all(&answer, 44), "Done. Every file compiles.");
}

#[test]
fn soft_wraps_rejoin_and_hard_newlines_stay() {
    let source = "one two three four five six seven  \neight\n\nnext paragraph";
    let frame = crate::view::message_rows_for_test(source, MessageRole::User, 14);
    assert!(frame.len() > 4, "{frame:?}");
    assert_eq!(
        select_all(&frame, 14),
        "one two three four five six seven\neight\n\nnext paragraph"
    );
    let token = crate::view::wrap_for_test(
        Line::styled("see https://example.com/a/very/long/path ok", normal()),
        12,
    );
    assert_eq!(
        select_all(&token, 12),
        "see https://example.com/a/very/long/path ok"
    );
}

#[test]
fn code_panels_drop_the_gutter_and_padding_and_keep_indentation_and_blank_rows() {
    let source =
        "Here:\n\n```rust\nfn main() {\n\n    let value = compute(1, 2, 3, 4, 5, 6);\n}\n```\n";
    let frame = crate::view::message_rows_for_test(source, MessageRole::Assistant, 30);
    assert!(frame.iter().any(|line| line.kind == RowKind::Code));
    assert!(
        frame.iter().any(|line| line.join == Join::Glue),
        "the long line wraps"
    );
    let text = select_all(&frame, 30);
    assert_eq!(
        text,
        "Here:\n\nfn main() {\n\n    let value = compute(1, 2, 3, 4, 5, 6);\n}"
    );
    assert!(!text.contains('┃') && !text.contains('↪'));
}

#[test]
fn list_markers_and_quote_rails_are_chrome_on_every_row() {
    let source = "- first item that wraps onto a second row\n- second\n\n> quoted words that also wrap around";
    let frame = crate::view::message_rows_for_test(source, MessageRole::Assistant, 22);
    assert_eq!(
        select_all(&frame, 22),
        "first item that wraps onto a second row\nsecond\n\nquoted words that also wrap around"
    );
}

#[test]
fn tool_glyphs_and_tail_rails_are_chrome() {
    let frame = crate::view::tool_row_for_test("test a ... ok\n  test b ... ok", 40);
    assert_eq!(
        select_all(&frame, 40),
        "Ran  cargo test\ntest a ... ok\n  test b ... ok"
    );
}

#[test]
fn wide_and_joined_glyphs_copy_whole_from_any_cell() {
    let frame = vec![Line::styled("a界b 👩\u{200d}💻 é", normal())];
    let shapes = shapes(&frame);
    let mut half = Selection::start(area(20, 1), 0, 0, Unit::Cell).unwrap();
    half.extend(1, 0);
    assert_eq!(half.text(&frame, &shapes).text, "a界");
    let mut second = Selection::start(area(20, 1), 2, 0, Unit::Cell).unwrap();
    second.extend(3, 0);
    assert_eq!(second.text(&frame, &shapes).text, "界b");
    let mut emoji = Selection::start(area(20, 1), 6, 0, Unit::Cell).unwrap();
    emoji.extend(19, 0);
    assert_eq!(emoji.text(&frame, &shapes).text, "👩\u{200d}💻 é");
    let mut split = Line::styled("x👩\u{200d}", normal());
    split.push("💻y", crate::render::accent());
    let frame = vec![split];
    let mut across = Selection::start(area(20, 1), 0, 0, Unit::Cell).unwrap();
    across.extend(19, 0);
    assert_eq!(
        across.text(&frame, &self::shapes(&frame)).text,
        "x👩\u{200d}💻y"
    );
}

#[test]
fn a_copy_is_capped_at_one_mebibyte_on_a_grapheme_boundary() {
    let text = format!("{}e\u{301}tail", "a".repeat(MAX_COPY_BYTES - 1));
    let copied = Copied::new(&text);
    assert!(copied.truncated);
    assert_eq!(copied.text.len(), MAX_COPY_BYTES - 1);
    let small = Copied::new("one\ntwo");
    assert!(!small.truncated);
    assert_eq!(small.lines, 2);
}

#[test]
fn escape_sequences_and_controls_are_stripped() {
    let planted =
        "ok\x1b]52;c;ZXZpbA==\x07 \x1b[31mred\x1b[0m\x1bP+q\x1b\\ \u{9b}2Jx\u{7}\r\nnext\u{202e}";
    assert_eq!(strip_escapes(planted), "ok red x\nnext");
    assert_eq!(Copied::new("\x1b]52;c;AAAA\x1b\\").text, "");
}

#[test]
fn a_press_without_a_drag_selects_nothing_and_drags_clamp_to_the_pane() {
    let frame = vec![
        Line::styled("alpha beta", normal()),
        Line::styled("gamma", normal()),
    ];
    let shapes = shapes(&frame);
    let pane = area(10, 2);
    let click = Selection::start(pane, 3, 0, Unit::Cell).unwrap();
    assert!(click.is_click());
    assert!(click.ranges(&frame, &shapes).is_empty());
    assert!(
        Selection::start(pane, 12, 0, Unit::Cell).is_none(),
        "outside the pane"
    );
    let mut down = Selection::start(pane, 6, 0, Unit::Cell).unwrap();
    down.extend(90, 90);
    assert_eq!(down.text(&frame, &shapes).text, "beta\ngamma");
    let mut up = Selection::start(pane, 2, 1, Unit::Cell).unwrap();
    up.extend(6, 0);
    assert_eq!(up.text(&frame, &shapes).text, "beta\ngam");
}

#[test]
fn double_click_takes_a_word_and_triple_click_takes_the_block() {
    let source = "First paragraph here.\n\n```\nlet a = 1;\nlet b = 2;\n```\n\nLast words.";
    let frame = crate::view::message_rows_for_test(source, MessageRole::Assistant, 30);
    let shapes = shapes(&frame);
    let pane = area(30, frame.len());
    let word = Selection::start(pane, 8, 0, Unit::Word).unwrap();
    assert_eq!(word.text(&frame, &shapes).text, "paragraph");
    let code_row = frame
        .iter()
        .position(|line| line.spans.iter().any(|span| span.text.contains("let b")))
        .unwrap();
    let block = Selection::start(pane, 6, code_row, Unit::Block).unwrap();
    assert_eq!(block.text(&frame, &shapes).text, "let a = 1;\nlet b = 2;");
    let paragraph = Selection::start(pane, 4, 0, Unit::Block).unwrap();
    assert_eq!(
        paragraph.text(&frame, &shapes).text,
        "First paragraph here."
    );
    let tool = crate::view::tool_row_for_test("line one\nline two", 30);
    let output = Selection::start(area(30, 3), 4, 2, Unit::Block).unwrap();
    assert_eq!(
        output.text(&tool, &self::shapes(&tool)).text,
        "Ran  cargo test\nline one\nline two"
    );
}

#[test]
fn a_selection_clears_when_its_rows_change_or_the_pane_moves() {
    let frame = vec![
        Line::styled("alpha", normal()),
        Line::styled("beta", normal()),
    ];
    let shapes = shapes(&frame);
    let mut selection = Selection::start(area(10, 2), 0, 0, Unit::Cell).unwrap();
    selection.extend(4, 1);
    assert!(selection.still_shows(&frame, &shapes, Some(area(10, 2))));
    assert!(selection.still_shows(&frame, &shapes, Some(area(10, 2))));
    let changed = vec![
        Line::styled("alpha", normal()),
        Line::styled("betas", normal()),
    ];
    assert!(!selection.still_shows(&changed, &shapes, Some(area(10, 2))));
    let mut moved = Selection::start(area(10, 2), 0, 0, Unit::Cell).unwrap();
    moved.extend(4, 1);
    assert!(!moved.still_shows(&frame, &shapes, Some(area(8, 2))));
    assert!(!moved.still_shows(&frame, &shapes, None));
}

#[test]
fn painting_marks_only_selected_text_cells() {
    let mut frame = vec![Line::styled("  alpha", normal()).with_chrome(2)];
    let shapes = shapes(&frame);
    let mut selection = Selection::start(area(10, 1), 0, 0, Unit::Cell).unwrap();
    selection.extend(3, 0);
    selection.paint(&mut frame, &shapes);
    let painted: String = frame[0]
        .spans
        .iter()
        .filter(|span| span.style.background.is_some())
        .map(|span| span.text.as_str())
        .collect();
    assert_eq!(painted, "al");
}
