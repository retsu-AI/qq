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
/// right edge, whole rows between, the last row up to the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    area: Area,
    anchor: (usize, usize),
    head: (usize, usize),
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
    pub(crate) fn start(area: Area, column: usize, row: usize) -> Option<Self> {
        area.contains(column, row).then_some(Self {
            area,
            anchor: (column, row),
            head: (column, row),
        })
    }

    /// Move the head to the pointer, clamped to the pane: dragging over the
    /// rail or the composer extends to the pane's edge and no further.
    pub(crate) fn extend(&mut self, column: usize, row: usize) {
        self.head = self.area.clamp(column, row);
    }

    /// A press released where it started: a click, not a selection.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
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
    /// on the selection background. Only selected rows are rebuilt.
    pub(crate) fn paint(&self, lines: &mut [Line]) {
        for (row, from, to) in self.rows() {
            let Some(line) = lines.get_mut(row) else {
                continue;
            };
            let mut painted = Line::default();
            for (column, character, style) in cells(line) {
                let style = if (from..to).contains(&column) {
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

    /// The selected text of `lines`: each row's selected cells with trailing
    /// spaces removed, the indentation every row shares (the pane's measure
    /// inset) removed, and trailing blank rows dropped. Bounded by
    /// [`MAX_COPY_BYTES`].
    pub(crate) fn text(&self, lines: &[Line]) -> Copied {
        let mut rows: Vec<String> = self
            .rows()
            .map(|(row, from, to)| {
                let mut text = String::new();
                if let Some(line) = lines.get(row) {
                    for (column, character, _) in cells(line) {
                        if (from..to).contains(&column) {
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
        // The first row starts at the anchor, not the pane edge, so it does
        // not decide the inset shared by the rows below it.
        let shared = rows
            .iter()
            .skip(usize::from(rows.len() > 1))
            .filter(|row| !row.is_empty())
            .map(|row| row.len() - row.trim_start_matches(' ').len())
            .min()
            .unwrap_or(0);
        let mut text = String::new();
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            let row = if index == 0 && rows.len() > 1 {
                row.trim_start_matches(' ')
            } else {
                row.get(shared..).unwrap_or_default()
            };
            text.push_str(row);
        }
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

/// Each character of `line` with the column it starts at and its style; the
/// line's indent reads as leading blank cells.
fn cells(line: &Line) -> Vec<(usize, char, Style)> {
    let mut out = Vec::with_capacity(line.indent + 64);
    out.extend((0..line.indent).map(|column| (column, ' ', normal())));
    let mut column = line.indent;
    for span in &line.spans {
        for character in span.text.chars() {
            out.push((column, character, span.style));
            column += UnicodeWidthChar::width(character).unwrap_or_default();
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
    /// and a rail glued on at column 20.
    fn frame() -> (Vec<Line>, Area) {
        let row = |transcript: &str, rail: &str| {
            let mut line = Line::styled(format!("{transcript:<20}"), normal());
            line.push(rail, muted());
            line
        };
        let mut first = Line::styled("   ", normal());
        first.push("alpha beta", accent());
        first.push(format!("{:<7}", ""), normal());
        first.push("│ RAIL", muted());
        (
            vec![
                Line::styled(" qq  top row", normal()),
                first,
                row("   gamma", "│ ○ other"),
                row("     delta", "│"),
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
    fn a_drag_selects_text_in_the_pane_only_and_drops_the_shared_inset() {
        let (lines, area) = frame();
        let mut selection = Selection::start(area, 3, 1).unwrap();
        // Dragged far right over the rail and down past the body: clamped.
        selection.extend(29, 9);
        let copied = selection.text(&lines);
        assert_eq!(copied.text, "alpha beta\ngamma\n  delta");
        assert_eq!(copied.lines, 3);
        assert!(!copied.truncated);
        assert!(!copied.text.contains('│') && !copied.text.contains("RAIL"));
        // Upward drags read the same way.
        let mut backwards = Selection::start(area, 9, 3).unwrap();
        backwards.extend(9, 1);
        assert_eq!(backwards.text(&lines).text, "beta\ngamma\n  delta");
    }

    #[test]
    fn a_press_outside_the_pane_starts_nothing_and_a_click_is_empty() {
        let (_, area) = frame();
        assert!(Selection::start(area, 22, 1).is_none(), "rail");
        assert!(Selection::start(area, 3, 0).is_none(), "top row");
        assert!(Selection::start(area, 3, 4).is_none(), "rule");
        let click = Selection::start(area, 3, 2).unwrap();
        assert!(click.is_empty());
    }

    #[test]
    fn painting_restyles_only_the_selected_cells() {
        let (mut lines, area) = frame();
        let mut selection = Selection::start(area, 3, 1).unwrap();
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
        let mut selection = Selection::start(wide, 0, 0).unwrap();
        selection.extend(1023, 511);
        let copied = selection.text(&lines);
        assert!(copied.truncated);
        assert!(copied.text.len() <= MAX_COPY_BYTES);
    }

    #[test]
    fn rail_clicks_resolve_to_the_row_they_land_on() {
        let geometry = FrameGeometry {
            transcript: None,
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
