//! QQ-owned mouse selection. With mouse capture on, a terminal's own drag
//! selects whole rows across every pane, so the TUI selects instead: a drag
//! that starts in the transcript pane selects that pane's cells only
//! (clamped to its columns and body rows, never the rail, inspector, borders,
//! top row or composer), the frame paints them on the selection background,
//! and release copies their text to the system clipboard with OSC 52.
//!
//! A selection is screen-anchored: cells, not message offsets. The text is
//! read from the last drawn frame when it is copied, so no transcript cache
//! changes and nothing is paid per frame while no selection exists.

use base64::Engine as _;
use qq_protocol::SessionId;
use unicode_width::UnicodeWidthChar;

use crate::render::{Line, Style, normal, selection};

/// Most text one copy puts on the clipboard, before encoding. A frame is
/// bounded (1024 × 512 cells), so this only binds a pathological frame.
pub(crate) const MAX_COPY_BYTES: usize = 1024 * 1024;

/// A rectangle of terminal cells; zero-based from the top-left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Area {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Area {
    const fn contains(self, column: usize, row: usize) -> bool {
        column >= self.x
            && column < self.x + self.width
            && row >= self.y
            && row < self.y + self.height
    }

    fn clamp(self, column: usize, row: usize) -> (usize, usize) {
        (
            column.clamp(self.x, (self.x + self.width).saturating_sub(1)),
            row.clamp(self.y, (self.y + self.height).saturating_sub(1)),
        )
    }
}

/// Where the last frame put what the mouse can act on, handed to the app on
/// commit like pane state: the transcript pane's cells, and the session each
/// visible rail row focuses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FrameGeometry {
    pub transcript: Option<Area>,
    /// The column transcript text starts at: cells left of it are the
    /// pane's margin and message rail, not text, and a copy drops them.
    pub text_x: usize,
    pub rail: Option<Area>,
    /// One entry per rail body row, top to bottom.
    pub rail_targets: Vec<Option<SessionId>>,
}

impl FrameGeometry {
    /// The session a click at this cell focuses, when it lands on a rail
    /// session or agent row.
    pub(crate) fn rail_target(&self, column: usize, row: usize) -> Option<SessionId> {
        let rail = self.rail?;
        if !rail.contains(column, row) {
            return None;
        }
        self.rail_targets.get(row - rail.y).copied().flatten()
    }
}

/// A drag in progress or finished: from `anchor` to `head`, both inside
/// `area`. Reads like text: the first row from the anchor to the pane's
/// right edge, whole rows between, the last row up to the head. `seen` is
/// a fingerprint of the selected rows' text when the selection last moved,
/// so a frame whose rows changed under it (streamed text reflowed) can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    area: Area,
    /// Cells left of this column are margin and rail, never copied.
    text_x: usize,
    anchor: (usize, usize),
    head: (usize, usize),
    seen: Option<u64>,
}

/// The text a selection copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Copied {
    pub text: String,
    pub lines: usize,
    /// The text was cut at [`MAX_COPY_BYTES`].
    pub truncated: bool,
}

impl Selection {
    /// A selection anchored at a press inside `area`; `None` outside it.
    /// Copies keep text from `text_x` on.
    pub(crate) fn start(area: Area, text_x: usize, column: usize, row: usize) -> Option<Self> {
        area.contains(column, row).then_some(Self {
            area,
            text_x,
            anchor: (column, row),
            head: (column, row),
            seen: None,
        })
    }

    /// Move the head to the pointer, clamped to the pane: dragging over the
    /// rail or the composer extends to the pane's edge and no further. The
    /// fingerprint is retaken on the next frame.
    pub(crate) fn extend(&mut self, column: usize, row: usize) {
        self.head = self.area.clamp(column, row);
        self.seen = None;
    }

    /// Whether `lines` still hold what this selection was made over: true
    /// the first time a frame is seen after a press or drag (which records
    /// it), then only while the selected rows' text is unchanged.
    pub(crate) fn matches(&self, lines: &[Line]) -> bool {
        self.seen.is_none_or(|seen| seen == self.fingerprint(lines))
    }

    /// Record the selected rows' text of the frame the user is looking at.
    pub(crate) fn remember(&mut self, lines: &[Line]) {
        if self.seen.is_none() {
            self.seen = Some(self.fingerprint(lines));
        }
    }

    fn fingerprint(&self, lines: &[Line]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (row, from, to) in self.rows() {
            if let Some(line) = lines.get(row) {
                for (start, end, grapheme, _) in cells(line) {
                    if start < to && end > from {
                        grapheme.hash(&mut hasher);
                    }
                }
            }
            row.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// A press released where it started: a click, not a selection.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The same cells, whatever fingerprint either carries.
    pub(crate) fn same_range(&self, other: &Self) -> bool {
        (self.area, self.anchor, self.head) == (other.area, other.anchor, other.head)
    }

    /// `(row, first column, end column)` per selected row, end exclusive,
    /// in reading order.
    fn rows(&self) -> impl Iterator<Item = (usize, usize, usize)> {
        let (start, end) = if (self.anchor.1, self.anchor.0) <= (self.head.1, self.head.0) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let left = self.area.x;
        let right = self.area.x + self.area.width;
        (start.1..=end.1).map(move |row| {
            let from = if row == start.1 { start.0 } else { left };
            let to = if row == end.1 { end.0 + 1 } else { right };
            (row, from, to.min(right))
        })
    }

    /// Paint the selected cells of `lines` (the frame's rows, top row first)
    /// on the selection background. Only selected rows are rebuilt. A glyph
    /// is selected when any of its cells is, so a wide character never shows
    /// half selected. A press that has not moved paints nothing.
    pub(crate) fn paint(&self, lines: &mut [Line]) {
        if self.is_empty() {
            return;
        }
        for (row, from, to) in self.rows() {
            let Some(line) = lines.get_mut(row) else {
                continue;
            };
            // `cells` turns the indent into leading blank cells, so the
            // painted row has none; its wrap and chrome carry over.
            let mut painted = Line {
                indent: 0,
                wrapped: line.wrapped,
                chrome: line.chrome,
                spans: Vec::new(),
            };
            for (start, end, grapheme, style) in cells(line) {
                let style = if start < to && end > from {
                    selection(style)
                } else {
                    style
                };
                painted.push(grapheme, style);
            }
            // Selected cells past the row's last character still show.
            let width = line.width();
            if to > width {
                let blank = from.max(width);
                if blank > width {
                    painted.push(" ".repeat(blank - width), normal());
                }
                painted.push(" ".repeat(to - blank), selection(normal()));
            }
            *line = painted;
        }
    }

    /// The selected text of `lines`: each row's selected characters (a glyph
    /// counts when any of its cells is selected, and a combining mark goes
    /// with its base), with the pane's margin and message rail (everything
    /// left of `text_x`) and trailing spaces removed, and trailing blank rows
    /// dropped. Indentation that is part of the text (code) is kept.
    /// Bounded by [`MAX_COPY_BYTES`].
    pub(crate) fn text(&self, lines: &[Line]) -> Copied {
        // Each selected row's text and whether it soft-wraps into the next.
        let mut rows: Vec<(String, crate::render::Wrap)> = self
            .rows()
            .map(|(row, from, to)| {
                let mut text = String::new();
                let mut wrapped = crate::render::Wrap::Break;
                if let Some(line) = lines.get(row) {
                    wrapped = line.wrapped;
                    // Chrome cells start where the row's spans start.
                    let chrome_end = line.indent + line.chrome;
                    for (start, end, grapheme, _) in cells(line) {
                        if start < self.text_x || start < chrome_end {
                            continue;
                        }
                        if start < to && end > from {
                            text.push_str(&grapheme);
                        }
                    }
                }
                // A literal wrap can break right after a space that belongs
                // to the code line, and a literal row that wrapped is full,
                // so it carries no padding: only other rows are trimmed.
                if wrapped != crate::render::Wrap::Literal {
                    text.truncate(text.trim_end().len());
                }
                (text, wrapped)
            })
            .collect();
        while rows.last().is_some_and(|(row, _)| row.is_empty()) {
            rows.pop();
        }
        // Blank rows before the first text (a panel's padding row) too.
        let first_text = rows
            .iter()
            .position(|(row, _)| !row.is_empty())
            .unwrap_or(rows.len());
        rows.drain(..first_text);
        // A soft wrap joins with a space (the wrap dropped the space it broke
        // on); a real line break stays a newline.
        let mut text = String::new();
        for (index, (row, _)) in rows.iter().enumerate() {
            if index > 0 {
                match rows[index - 1].1 {
                    crate::render::Wrap::Break => text.push('\n'),
                    crate::render::Wrap::Prose => text.push(' '),
                    crate::render::Wrap::Literal => {}
                }
            }
            text.push_str(row);
        }
        let mut truncated = false;
        if text.len() > MAX_COPY_BYTES {
            let mut cut = MAX_COPY_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            // Back up to a grapheme boundary: never end on a joiner, before
            // a mark that belongs to the last kept character, or between the
            // two regional indicators of a flag.
            while cut > 0 {
                let next = text[cut..].chars().next();
                let prev = text[..cut].chars().next_back();
                let odd_indicators = text[..cut]
                    .chars()
                    .rev()
                    .take_while(|character| is_regional_indicator(*character))
                    .count()
                    % 2
                    == 1;
                let splits = next.is_some_and(continues_grapheme)
                    || prev.is_some_and(|character| character == ZWJ || is_virama(character))
                    || (odd_indicators && next.is_some_and(is_regional_indicator));
                if !splits {
                    break;
                }
                cut -= prev.map_or(1, char::len_utf8);
            }
            text.truncate(cut);
            truncated = true;
        }
        Copied {
            lines: if text.is_empty() {
                0
            } else {
                text.lines().count()
            },
            text,
            truncated,
        }
    }
}

const ZWJ: char = '\u{200D}';

/// A flag is two regional indicators shown as one glyph.
fn is_regional_indicator(character: char) -> bool {
    matches!(character, '\u{1F1E6}'..='\u{1F1FF}')
}

/// Whether `character` continues the grapheme before it rather than starting
/// one: a zero-width mark, a variation selector, an emoji skin-tone modifier,
/// or a ZWJ (and so the character a ZWJ joins). Covers what a transcript
/// realistically shows without a segmentation dependency.
/// A virama joins the consonant after it into one conjunct (`क्ष`), the
/// way a ZWJ joins emoji: the Indic scripts' signs.
fn is_virama(character: char) -> bool {
    matches!(
        character,
        '\u{094D}'
            | '\u{09CD}'
            | '\u{0A4D}'
            | '\u{0ACD}'
            | '\u{0B4D}'
            | '\u{0BCD}'
            | '\u{0C4D}'
            | '\u{0CCD}'
            | '\u{0D4D}'
            | '\u{0DCA}'
    )
}

fn continues_grapheme(character: char) -> bool {
    character == ZWJ
        || matches!(character, '\u{FE00}'..='\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}')
        || (UnicodeWidthChar::width(character) == Some(0) && !character.is_control())
}

/// Each grapheme of `line` with the columns it occupies (`start..end`) and
/// its style, as one cell group: a ZWJ sequence, a base with its marks, or
/// an emoji with its modifier is copied and painted whole. The line's indent
/// reads as leading blank cells.
fn cells(line: &Line) -> Vec<(usize, usize, String, Style)> {
    let mut out: Vec<(usize, usize, String, Style)> = Vec::with_capacity(line.indent + 64);
    out.extend((0..line.indent).map(|column| (column, column + 1, " ".to_owned(), normal())));
    let mut column = line.indent;
    let mut joined = false;
    // Whether the last cell is a lone regional indicator awaiting its pair.
    let mut open_flag = false;
    for span in &line.spans {
        for character in span.text.chars() {
            let pairs = open_flag && is_regional_indicator(character);
            let attaches =
                (continues_grapheme(character) || joined || pairs) && out.len() > line.indent;
            if attaches && let Some(last) = out.last_mut() {
                last.2.push(character);
                // Two one-column indicators show as one two-column flag.
                if pairs && last.1 - last.0 < 2 {
                    last.1 = last.0 + 2;
                    column = last.1;
                }
                joined = character == ZWJ || is_virama(character);
                open_flag = false;
                continue;
            }
            joined = false;
            open_flag = is_regional_indicator(character);
            let width = UnicodeWidthChar::width(character).unwrap_or_default();
            out.push((column, column + width, character.to_string(), span.style));
            column += width;
        }
    }
    out
}

/// OSC 52: put `text` on the system clipboard through the terminal. Most
/// terminals and multiplexers accept it (tmux with `set-clipboard on`).
pub(crate) fn osc52(text: &str) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{accent, muted};

    /// A 30-column frame: a transcript pane of 20 columns at x 0, rows 1..=3,
    /// and a rail glued on at column 20. Transcript rows carry the pane's
    /// 3-column measure inset as `Line::indent`, as the renderer does;
    /// `delta` has two columns of its own (code) indentation.
    fn frame() -> (Vec<Line>, Area) {
        let row = |transcript: &str, rail: &str| {
            let mut line = Line::styled(format!("{transcript:<17}"), normal());
            line.indent = 3;
            line.push(rail, muted());
            line
        };
        let mut first = Line::styled("alpha beta", accent());
        first.indent = 3;
        first.push(format!("{:<7}", ""), normal());
        first.push("│ RAIL", muted());
        (
            vec![
                Line::styled(" qq  top row", normal()),
                first,
                row("gamma", "│ ○ other"),
                row("  delta", "│"),
                Line::styled("─── rule", muted()),
            ],
            Area {
                x: 0,
                y: 1,
                width: 20,
                height: 3,
            },
        )
    }

    #[test]
    fn a_drag_selects_text_in_the_pane_only_and_drops_only_the_margin() {
        let (lines, area) = frame();
        let mut selection = Selection::start(area, 3, 3, 1).unwrap();
        // Dragged far right over the rail and down past the body: clamped.
        selection.extend(29, 9);
        let copied = selection.text(&lines);
        assert_eq!(copied.text, "alpha beta\ngamma\n  delta");
        assert_eq!(copied.lines, 3);
        assert!(!copied.truncated);
        assert!(!copied.text.contains('│') && !copied.text.contains("RAIL"));
        // Upward drags read the same way.
        let mut backwards = Selection::start(area, 3, 9, 3).unwrap();
        backwards.extend(9, 1);
        assert_eq!(backwards.text(&lines).text, "beta\ngamma\n  delta");
    }

    #[test]
    fn a_press_outside_the_pane_starts_nothing_and_a_click_is_empty() {
        let (_, area) = frame();
        assert!(Selection::start(area, 3, 22, 1).is_none(), "rail");
        assert!(Selection::start(area, 3, 3, 0).is_none(), "top row");
        assert!(Selection::start(area, 3, 3, 4).is_none(), "rule");
        let click = Selection::start(area, 3, 3, 2).unwrap();
        assert!(click.is_empty());
    }

    #[test]
    fn painting_restyles_only_the_selected_cells() {
        let (mut lines, area) = frame();
        let mut selection = Selection::start(area, 3, 3, 1).unwrap();
        selection.extend(7, 1);
        selection.paint(&mut lines);
        let row = &lines[1];
        let background = crate::render::selection(normal()).background;
        let selected: String = row
            .spans
            .iter()
            .filter(|span| span.style.background == background)
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(selected, "alpha");
        let rail = row
            .spans
            .iter()
            .find(|span| span.text.contains("RAIL"))
            .unwrap();
        assert_eq!(rail.style, muted(), "the rail keeps its style");
        assert_eq!(lines[2], frame().0[2], "unselected rows are untouched");
    }

    #[test]
    fn a_partial_row_selection_reads_like_a_terminal_in_either_direction() {
        let (lines, area) = frame();
        // From inside `alpha` on row 1 to inside `gamma` on row 2: the end
        // of row 1 from the anchor, then row 2 up to and including the head.
        let mut forward = Selection::start(area, 3, 5, 1).unwrap();
        forward.extend(5, 2);
        assert_eq!(forward.text(&lines).text, "pha beta\ngam");
        // The same cells dragged upward copy the same text.
        let mut reverse = Selection::start(area, 3, 5, 2).unwrap();
        reverse.extend(5, 1);
        assert_eq!(reverse.text(&lines).text, forward.text(&lines).text);
    }

    #[test]
    fn code_indentation_shared_by_every_selected_row_is_kept() {
        let code = |text: &str| {
            let mut line = Line::styled(text, normal());
            line.indent = 3;
            line
        };
        let lines = vec![code("    if x:"), code("        y()")];
        let area = Area {
            x: 0,
            y: 0,
            width: 20,
            height: 2,
        };
        let mut selection = Selection::start(area, 3, 0, 0).unwrap();
        selection.extend(19, 1);
        assert_eq!(selection.text(&lines).text, "    if x:\n        y()");
    }

    #[test]
    fn wide_glyphs_and_combining_marks_are_selected_whole() {
        // `界` occupies columns 0-1, `b́` is `b` plus a combining accent.
        let lines = vec![Line::styled("界ab\u{301}c", normal())];
        let area = Area {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        };
        // Forward from the wide glyph's second cell through `b`.
        let mut forward = Selection::start(area, 0, 1, 0).unwrap();
        forward.extend(3, 0);
        assert_eq!(forward.text(&lines).text, "界ab\u{301}");
        // Reverse, ending on the wide glyph's second cell.
        let mut reverse = Selection::start(area, 0, 3, 0).unwrap();
        reverse.extend(1, 0);
        assert_eq!(reverse.text(&lines).text, "界ab\u{301}");
        // Painting covers the whole glyph too.
        let mut painted = lines.clone();
        forward.paint(&mut painted);
        let background = crate::render::selection(normal()).background;
        assert!(
            painted[0]
                .spans
                .iter()
                .any(|span| span.text.contains('界') && span.style.background == background)
        );
    }

    #[test]
    fn a_selection_over_rows_that_changed_no_longer_matches() {
        let (lines, area) = frame();
        let mut selection = Selection::start(area, 3, 3, 2).unwrap();
        selection.extend(7, 2);
        assert!(selection.matches(&lines), "first frame is recorded");
        selection.remember(&lines);
        assert!(selection.matches(&lines));
        let mut moved = lines.clone();
        moved[2] = Line::styled("   streamed in", normal());
        assert!(!selection.matches(&moved));
        // A click that never moved paints nothing.
        let click = Selection::start(area, 3, 3, 2).unwrap();
        let mut painted = lines.clone();
        click.paint(&mut painted);
        assert_eq!(painted, lines);
    }

    #[test]
    fn joined_emoji_are_one_selectable_glyph_and_a_large_copy_cuts_between_graphemes() {
        let lines = vec![Line::styled("a👩\u{200D}💻b", normal())];
        let area = Area {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        };
        // A drag over the emoji's first cell copies the whole sequence.
        let mut selection = Selection::start(area, 0, 1, 0).unwrap();
        selection.extend(1, 0);
        assert_eq!(selection.text(&lines).text, "👩\u{200D}💻");
        // A selection larger than the bound never ends inside a grapheme.
        let row = "é".repeat(400) + &"👩\u{200D}💻".repeat(200);
        let wide = Area {
            x: 0,
            y: 0,
            width: 1024,
            height: 512,
        };
        let lines: Vec<Line> = (0..512)
            .map(|_| Line::styled(row.clone(), normal()))
            .collect();
        let mut selection = Selection::start(wide, 0, 0, 0).unwrap();
        selection.extend(1023, 511);
        let copied = selection.text(&lines);
        assert!(copied.truncated && copied.text.len() <= MAX_COPY_BYTES);
        let last = copied.text.chars().next_back().unwrap();
        assert_ne!(last, ZWJ, "never ends on a joiner");
        assert!(!continues_grapheme(last) || last == '💻');
    }

    #[test]
    fn flags_are_one_glyph_and_soft_wraps_copy_as_spaces() {
        let lines = vec![Line::styled("a🇺🇸b", normal())];
        let area = Area {
            x: 0,
            y: 0,
            width: 10,
            height: 2,
        };
        // A drag over the flag's second cell copies the whole flag.
        let mut selection = Selection::start(area, 0, 2, 0).unwrap();
        selection.extend(3, 0);
        assert_eq!(selection.text(&lines).text, "🇺🇸b");
        // A paragraph wrapped over two rows copies as one line; a real
        // break between rows stays a newline.
        let mut first = Line::styled("soft", normal());
        first.wrapped = crate::render::Wrap::Prose;
        let lines = vec![first, Line::styled("wrap", normal())];
        let mut selection = Selection::start(area, 0, 0, 0).unwrap();
        selection.extend(9, 1);
        assert_eq!(selection.text(&lines).text, "soft wrap");
        let lines = vec![
            Line::styled("hard", normal()),
            Line::styled("break", normal()),
        ];
        assert_eq!(selection.text(&lines).text, "hard\nbreak");
        // The prose wrapper marks every row but the last.
        let rows = crate::view::wrap_line_for_test(Line::styled("one two three", normal()), 5);
        assert!(
            rows[..rows.len() - 1]
                .iter()
                .all(|row| row.wrapped == crate::render::Wrap::Prose)
        );
        assert_eq!(rows.last().unwrap().wrapped, crate::render::Wrap::Break);
        // A literal (code) line broken by width joins back with nothing.
        let code = crate::view::wrap_line_chars_for_test(Line::styled("let x = 1;", normal()), 4);
        assert!(code.len() > 1);
        let lines = code;
        let area = Area {
            x: 0,
            y: 0,
            width: 10,
            height: lines.len(),
        };
        let mut selection = Selection::start(area, 0, 0, 0).unwrap();
        selection.extend(9, lines.len() - 1);
        assert_eq!(selection.text(&lines).text, "let x = 1;");
        // Through a real code panel: the `┏  ` / `↪  ` gutter and the
        // padding are chrome, and the wrapped line copies back whole.
        let panel = crate::view::panel_rows_for_test(
            [Line::styled("let value = compute(1, 2);", normal())],
            None,
            16,
        );
        let area = Area {
            x: 0,
            y: 0,
            width: 16,
            height: panel.len(),
        };
        let mut selection = Selection::start(area, 0, 0, 0).unwrap();
        selection.extend(15, panel.len() - 1);
        assert_eq!(
            selection.text(&panel).text,
            "let value = compute(1, 2);",
            "{:?}",
            panel
        );
    }

    #[test]
    fn fifth_review_round_on_copy() {
        // A code panel inside a message: `indent_lines` prepends the 3-cell
        // message rail, and the gutter after it is still chrome.
        let panel = crate::view::panel_rows_for_test(
            [Line::styled("let value = compute(1, 2);", normal())],
            None,
            16,
        );
        let rows = crate::view::indent_lines_for_test(panel, "   ", normal(), 19);
        let area = Area {
            x: 0,
            y: 0,
            width: 19,
            height: rows.len(),
        };
        let mut selection = Selection::start(area, 3, 0, 0).unwrap();
        selection.extend(18, rows.len() - 1);
        assert_eq!(selection.text(&rows).text, "let value = compute(1, 2);");

        // A conjunct is one grapheme: `क्ष` copies whole from its last cell.
        let lines = vec![Line::styled("aक्षb", normal())];
        let area = Area {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        };
        // The conjunct is one cell group at column 1: a drag ending on it
        // copies the whole cluster, not `क्` alone.
        let mut wide = Selection::start(area, 0, 0, 0).unwrap();
        wide.extend(1, 0);
        assert_eq!(wide.text(&lines).text, "aक्ष");

        // A long token wider than the row wraps by characters: the copy
        // joins it back with nothing; a break between words keeps a space.
        let rows = crate::view::wrap_line_for_test(
            Line::styled("see https://example.com/a/very/long/path ok", normal()),
            12,
        );
        let area = Area {
            x: 0,
            y: 0,
            width: 12,
            height: rows.len(),
        };
        let mut selection = Selection::start(area, 0, 0, 0).unwrap();
        selection.extend(11, rows.len() - 1);
        assert_eq!(
            selection.text(&rows).text,
            "see https://example.com/a/very/long/path ok",
            "{rows:?}"
        );
    }

    #[test]
    fn a_copy_is_bounded() {
        let wide = Area {
            x: 0,
            y: 0,
            width: 1024,
            height: 512,
        };
        let lines: Vec<Line> = (0..512)
            .map(|_| Line::styled("é".repeat(1024), normal()))
            .collect();
        let mut selection = Selection::start(wide, 0, 0, 0).unwrap();
        selection.extend(1023, 511);
        let copied = selection.text(&lines);
        assert!(copied.truncated);
        assert!(copied.text.len() <= MAX_COPY_BYTES);
    }

    #[test]
    fn rail_clicks_resolve_to_the_row_they_land_on() {
        let geometry = FrameGeometry {
            transcript: None,
            text_x: 0,
            rail: Some(Area {
                x: 20,
                y: 1,
                width: 10,
                height: 3,
            }),
            rail_targets: vec![None, Some(SessionId::from_bytes([7; 16])), None],
        };
        assert_eq!(
            geometry.rail_target(25, 2),
            Some(SessionId::from_bytes([7; 16]))
        );
        assert_eq!(geometry.rail_target(25, 1), None, "a header row");
        assert_eq!(geometry.rail_target(5, 2), None, "outside the rail");
    }

    #[test]
    fn osc52_puts_base64_text_on_the_clipboard() {
        assert_eq!(osc52("hi\n"), b"\x1b]52;c;aGkK\x07".to_vec());
    }
}
