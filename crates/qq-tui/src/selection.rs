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
                for (start, end, character, _) in cells(line) {
                    if start < to && end > from {
                        character.hash(&mut hasher);
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
            let mut painted = Line::default();
            for (start, end, character, style) in cells(line) {
                let style = if start < to && end > from {
                    selection(style)
                } else {
                    style
                };
                painted.push(character.to_string(), style);
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
        let mut rows: Vec<String> = self
            .rows()
            .map(|(row, from, to)| {
                let mut text = String::new();
                if let Some(line) = lines.get(row) {
                    for (start, end, character, _) in cells(line) {
                        if start < self.text_x {
                            continue;
                        }
                        // A zero-width mark (start == end) belongs to the
                        // glyph before it.
                        let selected = if start == end {
                            start > from && start <= to
                        } else {
                            start < to && end > from
                        };
                        if selected {
                            text.push(character);
                        }
                    }
                }
                text.truncate(text.trim_end().len());
                text
            })
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        let mut text = rows.join("\n");
        let mut truncated = false;
        if text.len() > MAX_COPY_BYTES {
            let mut cut = MAX_COPY_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
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

/// Each character of `line` with the columns it occupies (`start..end`;
/// equal for a zero-width mark) and its style; the line's indent reads as
/// leading blank cells.
fn cells(line: &Line) -> Vec<(usize, usize, char, Style)> {
    let mut out = Vec::with_capacity(line.indent + 64);
    out.extend((0..line.indent).map(|column| (column, column + 1, ' ', normal())));
    let mut column = line.indent;
    for span in &line.spans {
        for character in span.text.chars() {
            let width = UnicodeWidthChar::width(character).unwrap_or_default();
            out.push((column, column + width, character, span.style));
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
