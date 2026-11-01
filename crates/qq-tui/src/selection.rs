use unicode_width::UnicodeWidthChar;

use crate::render::{Join, Line, RowKind, Span, Style, normal, selection};

pub(crate) const MAX_COPY_BYTES: usize = 1024 * 1024;
pub(crate) const MULTI_CLICK_MS: u128 = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Area {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Area {
    pub(crate) const fn contains(self, column: usize, row: usize) -> bool {
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

    const fn right(self) -> usize {
        self.x + self.width
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RowShape {
    pub start: usize,
    pub end: usize,
    pub kind: RowKind,
    pub join: Join,
}

impl RowShape {
    pub(crate) fn of(line: &Line) -> Self {
        let end = if line.pad == 0 {
            usize::MAX
        } else {
            line.width().saturating_sub(usize::from(line.pad))
        };
        Self {
            start: usize::from(line.indent) + usize::from(line.chrome),
            end,
            kind: line.kind,
            join: line.join,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FrameGeometry {
    pub transcript: Option<Area>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Cell,
    Word,
    Block,
}

impl Unit {
    pub(crate) const fn for_clicks(clicks: u8) -> Self {
        match clicks {
            0 | 1 => Self::Cell,
            2 => Self::Word,
            _ => Self::Block,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) area: Area,
    anchor: (usize, usize),
    head: (usize, usize),
    pub(crate) unit: Unit,
    pub(crate) seen: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Copied {
    pub text: String,
    pub lines: usize,
    pub truncated: bool,
}

impl Copied {
    pub(crate) fn new(text: &str) -> Self {
        let clean = strip_escapes(text);
        let (text, truncated) = bounded(clean);
        Self {
            lines: if text.is_empty() {
                0
            } else {
                text.matches('\n').count() + 1
            },
            text,
            truncated,
        }
    }
}

type Cell = (usize, usize, String, Style);

impl Selection {
    pub(crate) fn start(area: Area, column: usize, row: usize, unit: Unit) -> Option<Self> {
        area.contains(column, row).then_some(Self {
            area,
            anchor: (column, row),
            head: (column, row),
            unit,
            seen: None,
        })
    }

    pub(crate) fn extend(&mut self, column: usize, row: usize) -> bool {
        let head = self.area.clamp(column, row);
        if head == self.head {
            return false;
        }
        self.head = head;
        self.seen = None;
        true
    }

    pub(crate) fn still_shows(
        &mut self,
        frame: &[Line],
        shapes: &[RowShape],
        area: Option<Area>,
    ) -> bool {
        if area != Some(self.area) {
            return false;
        }
        let fingerprint = self.fingerprint(frame, shapes);
        *self.seen.get_or_insert(fingerprint) == fingerprint
    }

    pub(crate) fn is_click(&self) -> bool {
        self.unit == Unit::Cell && self.anchor == self.head
    }

    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.1, self.anchor.0) <= (self.head.1, self.head.0) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub(crate) fn fingerprint(&self, frame: &[Line], shapes: &[RowShape]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (row, _, _) in self.ranges(frame, shapes) {
            if let Some(line) = frame.get(row) {
                for (start, _, grapheme, _) in cells(line) {
                    if start >= self.area.right() {
                        break;
                    }
                    grapheme.hash(&mut hasher);
                }
            }
            row.hash(&mut hasher);
        }
        hasher.finish()
    }

    pub(crate) fn ranges(&self, frame: &[Line], shapes: &[RowShape]) -> Vec<(usize, usize, usize)> {
        if self.is_click() {
            return Vec::new();
        }
        let shape = |row: usize| -> RowShape {
            shapes
                .get(row.saturating_sub(self.area.y))
                .copied()
                .unwrap_or_default()
        };
        let left = self.area.x;
        let right = self.area.right();
        match self.unit {
            Unit::Cell => {
                let (start, end) = self.ordered();
                (start.1..=end.1)
                    .map(|row| {
                        let from = if row == start.1 { start.0 } else { left };
                        let to = if row == end.1 { end.0 + 1 } else { right };
                        (row, from, to.min(right))
                    })
                    .collect()
            }
            Unit::Word => {
                let (column, row) = self.anchor;
                let Some(line) = frame.get(row) else {
                    return Vec::new();
                };
                let row_shape = shape(row);
                let cells: Vec<Cell> = cells(line)
                    .into_iter()
                    .filter(|(start, end, _, _)| {
                        *start >= row_shape.start && *end <= row_shape.end.min(right)
                    })
                    .collect();
                let Some(hit) = cells
                    .iter()
                    .position(|(start, end, _, _)| *start <= column && column < *end)
                else {
                    return Vec::new();
                };
                let blank = |cell: &Cell| cell.2.chars().all(char::is_whitespace);
                if blank(&cells[hit]) {
                    return Vec::new();
                }
                let first = cells[..hit]
                    .iter()
                    .rposition(blank)
                    .map_or(0, |index| index + 1);
                let last = cells[hit..]
                    .iter()
                    .position(blank)
                    .map_or(cells.len(), |index| hit + index);
                vec![(row, cells[first].0, cells[last - 1].1)]
            }
            Unit::Block => {
                let row = self.anchor.1;
                let top_row = self.area.y;
                let bottom_row = self.area.y + self.area.height;
                let filled = |row: usize| {
                    let shape = shape(row);
                    shape.join == Join::Fence
                        || frame.get(row).is_some_and(|line| {
                            !row_text(line, left, right, shape).trim().is_empty()
                        })
                };
                if !filled(row) {
                    return Vec::new();
                }
                let kind = shape(row).kind;
                let (mut top, mut bottom) = (row, row);
                if matches!(kind, RowKind::Tool | RowKind::Output) {
                    if kind == RowKind::Output {
                        while top > top_row && shape(top - 1).kind == RowKind::Output {
                            top -= 1;
                        }
                        if top > top_row && shape(top - 1).kind == RowKind::Tool {
                            top -= 1;
                        }
                    }
                    while bottom + 1 < bottom_row && shape(bottom + 1).kind == RowKind::Output {
                        bottom += 1;
                    }
                } else {
                    let same =
                        |candidate: usize| filled(candidate) && shape(candidate).kind == kind;
                    while top > top_row && same(top - 1) {
                        top -= 1;
                    }
                    while bottom + 1 < bottom_row && same(bottom + 1) {
                        bottom += 1;
                    }
                }
                (top..=bottom).map(|row| (row, left, right)).collect()
            }
        }
    }

    pub(crate) fn text(&self, frame: &[Line], shapes: &[RowShape]) -> Copied {
        let right = self.area.right();
        let mut text = String::new();
        let mut previous: Option<RowKind> = None;
        for (row, from, to) in self.ranges(frame, shapes) {
            let shape = shapes
                .get(row.saturating_sub(self.area.y))
                .copied()
                .unwrap_or_default();
            if shape.join == Join::Fence {
                continue;
            }
            let content = frame
                .get(row)
                .map(|line| row_text(line, from, to.min(right), shape))
                .unwrap_or_default();
            if let Some(kind) = previous {
                match shape.join {
                    Join::Glue => {}
                    Join::Space => {
                        text.truncate(text.trim_end().len());
                        text.push(' ');
                    }
                    Join::Break | Join::Fence => {
                        if kind != RowKind::Code {
                            text.truncate(text.trim_end_matches(' ').len());
                        }
                        text.push('\n');
                    }
                }
            }
            text.push_str(&content);
            previous = Some(shape.kind);
            if text.len() > MAX_COPY_BYTES + 4 {
                break;
            }
        }
        if previous.is_some_and(|kind| kind != RowKind::Code) {
            text.truncate(text.trim_end_matches(' ').len());
        }
        Copied::new(&text)
    }

    pub(crate) fn paint(&self, frame: &mut [Line], shapes: &[RowShape]) {
        for (row, from, to) in self.ranges(frame, shapes) {
            let shape = shapes
                .get(row.saturating_sub(self.area.y))
                .copied()
                .unwrap_or_default();
            let Some(line) = frame.get_mut(row) else {
                continue;
            };
            let text_to = to.min(shape.end);
            if from.max(shape.start) >= text_to {
                continue;
            }
            let mut painted = Line {
                indent: 0,
                chrome: line.chrome.saturating_add(line.indent),
                pad: line.pad,
                kind: line.kind,
                join: line.join,
                spans: Vec::with_capacity(line.spans.len() + 2),
            };
            for (start, end, grapheme, style) in cells(line) {
                let style = if start >= shape.start && start < text_to && end > from {
                    selection(style)
                } else {
                    style
                };
                painted.push(grapheme, style);
            }
            *line = painted;
        }
    }
}

fn row_text(line: &Line, from: usize, to: usize, shape: RowShape) -> String {
    let to = to.min(shape.end);
    let mut text = String::new();
    for (start, end, grapheme, _) in cells(line) {
        if start >= to {
            break;
        }
        if start >= shape.start && end > from {
            text.push_str(&grapheme);
        }
    }
    text
}

const ZWJ: char = '\u{200D}';

fn continues_grapheme(character: char) -> bool {
    character == ZWJ
        || matches!(character, '\u{FE00}'..='\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}')
        || (UnicodeWidthChar::width(character) == Some(0) && !character.is_control())
}

fn is_regional_indicator(character: char) -> bool {
    matches!(character, '\u{1F1E6}'..='\u{1F1FF}')
}

pub(crate) fn cells(line: &Line) -> Vec<Cell> {
    let indent = usize::from(line.indent);
    let mut out: Vec<Cell> = Vec::with_capacity(indent + 64);
    out.extend((0..indent).map(|column| (column, column + 1, " ".to_owned(), normal())));
    let mut column = indent;
    let mut joined = false;
    let mut open_flag = false;
    for Span { text, style } in &line.spans {
        for character in text.chars() {
            let pairs = open_flag && is_regional_indicator(character);
            let continues =
                (continues_grapheme(character) || joined || pairs) && out.len() > indent;
            joined = character == ZWJ;
            open_flag = !continues && is_regional_indicator(character);
            if continues && let Some(last) = out.last_mut() {
                last.2.push(character);
                last.1 = last.0 + crate::render::text_width(&last.2).max(last.1 - last.0);
                column = last.1;
                continue;
            }
            let width = UnicodeWidthChar::width(character).unwrap_or_default();
            out.push((column, column + width, character.to_string(), *style));
            column += width;
        }
    }
    out
}

pub(crate) fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.next() {
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(next) = characters.next() {
                        if next == '\x07' || next == '\u{9c}' {
                            break;
                        }
                        if next == '\x1b' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\u{9b}' => {
                for next in characters.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            '\u{9d}' | '\u{90}' | '\u{9e}' | '\u{9f}' | '\u{98}' => {
                for next in characters.by_ref() {
                    if next == '\x07' || next == '\u{9c}' {
                        break;
                    }
                }
            }
            '\n' | '\t' => out.push(character),
            '\r' => {
                if characters.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            other if other.is_control() => {}
            other => {
                if crate::app::terminal_safe_character(other).is_some() {
                    out.push(other);
                }
            }
        }
    }
    out
}

fn bounded(mut text: String) -> (String, bool) {
    if text.len() <= MAX_COPY_BYTES {
        return (text, false);
    }
    let mut cut = MAX_COPY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    while cut > 0 {
        let next = text[cut..].chars().next();
        let previous = text[..cut].chars().next_back();
        let splits = next.is_some_and(continues_grapheme) || previous == Some(ZWJ);
        if !splits {
            break;
        }
        cut -= previous.map_or(1, char::len_utf8);
    }
    text.truncate(cut);
    (text, true)
}

#[cfg(test)]
mod tests;
