//! Markdown layout: prose, lists, quotes, tables, and fenced code panels with
//! optional tree-sitter highlighting. Produces styled lines already wrapped to
//! the requested width.

use std::sync::OnceLock;

use crate::{
    app::terminal_safe_character,
    render::{
        Line, Span, Style, accent, border, code_comment, code_constant, code_function,
        code_keyword, code_property, code_punctuation, code_string, code_type, diff_line_style,
        inline_code, link, muted, normal, text_width,
    },
    theme,
    view::wrap::{truncate_line, wrap_line, wrap_line_chars},
};
use crossterm::style::Color;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

/// Byte offset where a streaming markdown source can be split so the prefix
/// renders the same whether laid out alone or as part of the whole.
///
/// The split lands after a blank line that is neither inside an open fenced
/// code block nor directly below an indented (four-space) code line, because
/// pulldown-cmark treats such a blank line as a hard block boundary: nothing
/// after it changes how earlier blocks lay out. The blank line stays with the
/// prefix so it ends at a block end. Returns 0 when no boundary exists yet.
/// Link reference definitions are the one construct this ignores; a reference
/// defined after the split renders literally until the message completes.
pub(crate) fn settled_prefix_end(source: &str) -> usize {
    let mut settled = 0;
    let mut in_fence: Option<(u8, usize)> = None;
    let mut previous_indented = false;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let blank = line.trim().is_empty();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let fence_len = trimmed
            .bytes()
            .take_while(|byte| *byte == b'`' || *byte == b'~')
            .count();
        if fence_len >= 3 && line.len() - trimmed.len() < 4 {
            let marker = trimmed.as_bytes()[0];
            let uniform = trimmed.as_bytes()[..fence_len]
                .iter()
                .all(|byte| *byte == marker);
            match in_fence {
                None if uniform => in_fence = Some((marker, fence_len)),
                Some((open_marker, open_len))
                    if uniform
                        && marker == open_marker
                        && fence_len >= open_len
                        && trimmed[fence_len..].trim().is_empty() =>
                {
                    in_fence = None;
                }
                None | Some(_) => {}
            }
        }
        offset += line.len();
        if blank {
            if in_fence.is_none() && !previous_indented && line.ends_with('\n') {
                settled = offset;
            }
        } else {
            previous_indented =
                in_fence.is_none() && (line.starts_with("    ") || line.starts_with('\t'));
        }
    }
    settled
}

/// Lays markdown out as styled lines. `highlight` enables tree-sitter syntax
/// coloring inside fenced code panels; it is only worth paying for content
/// that renders once (terminal-state messages on the cached path), so
/// streaming render paths pass `false` and get plain panels.
pub(crate) fn markdown_lines(source: &str, width: usize, highlight: bool) -> Vec<Line> {
    if source.is_empty() {
        return Vec::new();
    }
    let width = width.max(1);
    let mut lines = vec![Line::default()];
    // Lines marked literal (code blocks, laid-out tables) keep character
    // wrapping so column alignment survives; prose lines wrap at words.
    let mut literal = vec![false];
    // Per-line hanging prefix: list markers and quote rails. The body wraps
    // at `width - hang` and the prefix repeats (as blanks for list items, as
    // the rail for quotes) on every continuation row.
    let mut hangs: Vec<Option<Hang>> = vec![None];
    let mut styles = vec![normal()];
    // One entry per open list: `Some(next)` for ordered lists with the
    // marker column width, `None` for bullets. Depth is the length.
    let mut lists: Vec<Option<(u64, usize)>> = Vec::new();
    let mut quote_depth = 0_usize;
    let mut table: Option<TableBuffer> = None;
    let mut code_block: Option<CodeBlockBuffer> = None;
    let mut heading_level: Option<HeadingLevel> = None;
    let mut heading_start = 0;
    // True from an item's marker until its first inline content, so a loose
    // item's opening paragraph joins the marker line.
    let mut item_marker_open = false;
    // Direct item counts per list in document order, so ordered markers can
    // be sized before their items arrive. One cheap pass over the events.
    let mut list_lengths: Vec<u64> = Vec::new();
    {
        let mut open: Vec<(usize, u64)> = Vec::new();
        for event in Parser::new_ext(source, Options::all()) {
            match event {
                Event::Start(Tag::List(_)) => {
                    open.push((list_lengths.len(), 0));
                    list_lengths.push(0);
                }
                Event::Start(Tag::Item) => {
                    if let Some((_, count)) = open.last_mut() {
                        *count += 1;
                    }
                }
                Event::End(TagEnd::List(_)) => {
                    if let Some((index, count)) = open.pop() {
                        list_lengths[index] = count;
                    }
                }
                _ => {}
            }
        }
    }
    let mut list_ordinal = 0;
    let parser = Parser::new_ext(source, Options::all());
    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                // Block starts: one blank row separates this block from the
                // previous one. Items inside a list stay tight; the list
                // itself is one block. Blank rows at the very top are
                // trimmed with the leading empty line below.
                Tag::Paragraph => {
                    if lists.is_empty() {
                        block_gap(&mut lines, &mut literal, &mut hangs);
                    } else if !item_marker_open {
                        // A later paragraph inside a loose item starts its
                        // own line under the item text; the first one
                        // continues on the marker line.
                        ensure_line(&mut lines, &mut literal, &mut hangs);
                        hangs.truncate(lines.len() - 1);
                        hangs.push(current_hang(&lists, quote_depth));
                    }
                    item_marker_open = false;
                }
                Tag::Heading { level, .. } => {
                    block_gap(&mut lines, &mut literal, &mut hangs);
                    heading_level = Some(level);
                    heading_start = lines.len() - 1;
                    styles.push(match level {
                        HeadingLevel::H1 => normal().bold(),
                        HeadingLevel::H2 => accent().bold(),
                        HeadingLevel::H3
                        | HeadingLevel::H4
                        | HeadingLevel::H5
                        | HeadingLevel::H6 => normal().bold(),
                    });
                }
                Tag::Strong => {
                    styles.push(styles.last().expect("base style remains").bold());
                }
                Tag::Emphasis => {
                    styles.push(styles.last().expect("base style remains").italic());
                }
                Tag::CodeBlock(kind) => {
                    block_gap(&mut lines, &mut literal, &mut hangs);
                    code_block = Some(CodeBlockBuffer::new(&kind));
                }
                Tag::List(start) => {
                    if lists.is_empty() {
                        block_gap(&mut lines, &mut literal, &mut hangs);
                    }
                    lists.push(start.map(|first| {
                        // Numbers right-align to the widest the list reaches;
                        // pulldown does not expose the count, so the list's
                        // direct items are counted from the event stream at
                        // this position. Bounded by the list's own length.
                        let count = list_lengths.get(list_ordinal).copied().unwrap_or(1).max(1);
                        let last = first.saturating_add(count - 1);
                        (first, last.to_string().len())
                    }));
                    list_ordinal += 1;
                }
                Tag::Item => {
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                    let depth = lists.len();
                    let indent = "  ".repeat(depth.saturating_sub(1));
                    let marker = match lists.last_mut() {
                        Some(Some((next, digits))) => {
                            let number = *next;
                            *next += 1;
                            let digits = (*digits).max(number.to_string().len());
                            format!("{indent}{number:>digits$}. ")
                        }
                        Some(None) if depth > 1 => format!("{indent}◦ "),
                        Some(None) | None => format!("{indent}• "),
                    };
                    let line = lines.last_mut().expect("line exists");
                    let hang_width = quote_rail_width(quote_depth) + marker.chars().count();
                    push_quote_rail(line, quote_depth);
                    line.push(marker, accent());
                    *hangs.last_mut().expect("hang exists") = Some(Hang {
                        width: hang_width,
                        rail: quote_depth,
                    });
                    item_marker_open = true;
                }
                Tag::BlockQuote(_) => {
                    if quote_depth == 0 && lists.is_empty() {
                        block_gap(&mut lines, &mut literal, &mut hangs);
                    }
                    quote_depth += 1;
                    styles.push(muted().italic());
                }
                Tag::Table(_) => {
                    block_gap(&mut lines, &mut literal, &mut hangs);
                    table = Some(TableBuffer::default());
                }
                Tag::TableHead => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.has_header = true;
                        buffer.begin_row();
                    }
                    styles.push(styles.last().expect("base style remains").bold());
                }
                Tag::TableRow => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.begin_row();
                    }
                }
                Tag::TableCell => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.begin_cell();
                    }
                }
                Tag::FootnoteDefinition(_) => {
                    block_gap(&mut lines, &mut literal, &mut hangs);
                }
                Tag::Link { .. } => {
                    styles.push(link());
                }
                Tag::Image { .. }
                | Tag::HtmlBlock
                | Tag::DefinitionList
                | Tag::DefinitionListTitle
                | Tag::DefinitionListDefinition
                | Tag::Strikethrough
                | Tag::Subscript
                | Tag::Superscript
                | Tag::MetadataBlock(_) => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                }
                TagEnd::Heading(_) => {
                    styles.pop();
                    if heading_level.take() == Some(HeadingLevel::H1) {
                        // An underline the width of the title, capped at the
                        // measure, marks the one top-level heading style.
                        let title_width = lines[heading_start..]
                            .iter()
                            .map(Line::width)
                            .max()
                            .unwrap_or(0)
                            .clamp(1, width);
                        ensure_line(&mut lines, &mut literal, &mut hangs);
                        *lines.last_mut().expect("line exists") =
                            Line::styled("─".repeat(title_width), border());
                        *literal.last_mut().expect("literal exists") = true;
                    }
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    styles.pop();
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                }
                TagEnd::CodeBlock => {
                    if let Some(buffer) = code_block.take() {
                        let rendered = layout_code_panel(&buffer, width, highlight);
                        if lines.last().is_some_and(Line::is_empty) {
                            lines.pop();
                            literal.pop();
                            hangs.pop();
                        }
                        let count = rendered.len();
                        lines.extend(rendered);
                        literal.extend(std::iter::repeat_n(true, count));
                        hangs.extend(std::iter::repeat_n(None, count));
                        lines.push(Line::default());
                        literal.push(false);
                        hangs.push(None);
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Link => {
                    styles.pop();
                }
                TagEnd::List(_) => {
                    lists.pop();
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                }
                TagEnd::Item => {
                    item_marker_open = false;
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                }
                TagEnd::Table => {
                    if let Some(mut buffer) = table.take() {
                        buffer.end_row();
                        let rendered = layout_table(&buffer.rows, buffer.has_header, width);
                        if !rendered.is_empty() {
                            if lines.last().is_some_and(Line::is_empty) {
                                lines.pop();
                                literal.pop();
                                hangs.pop();
                            }
                            let count = rendered.len();
                            lines.extend(rendered);
                            literal.extend(std::iter::repeat_n(true, count));
                            hangs.extend(std::iter::repeat_n(None, count));
                            lines.push(Line::default());
                            literal.push(false);
                            hangs.push(None);
                        }
                    }
                }
                TagEnd::TableHead => {
                    styles.pop();
                    if let Some(buffer) = table.as_mut() {
                        buffer.end_row();
                    }
                }
                TagEnd::TableRow => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.end_row();
                    }
                }
                TagEnd::FootnoteDefinition => {
                    ensure_line(&mut lines, &mut literal, &mut hangs);
                }
                TagEnd::Image
                | TagEnd::HtmlBlock
                | TagEnd::DefinitionList
                | TagEnd::DefinitionListTitle
                | TagEnd::DefinitionListDefinition
                | TagEnd::Strikethrough
                | TagEnd::Subscript
                | TagEnd::Superscript
                | TagEnd::TableCell
                | TagEnd::MetadataBlock(_) => {}
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if let Some(buffer) = code_block.as_mut() {
                    buffer.text.push_str(&text);
                } else {
                    let style = *styles.last().expect("base style remains");
                    match table.as_mut() {
                        Some(buffer) => buffer.append(&text, style),
                        None => {
                            begin_inline(&mut lines, &mut hangs, &lists, quote_depth);
                            append_safe_text(&mut lines, &text, style);
                            item_marker_open = false;
                        }
                    }
                }
            }
            Event::Code(code) => {
                if table.is_none() {
                    begin_inline(&mut lines, &mut hangs, &lists, quote_depth);
                }
                push_inline(table.as_mut(), &mut lines, &code, inline_code());
            }
            // A soft break is a source-formatting line break: render it as a
            // space so paragraphs reflow to the terminal width.
            Event::SoftBreak => {
                push_inline(
                    table.as_mut(),
                    &mut lines,
                    " ",
                    *styles.last().expect("base style remains"),
                );
            }
            Event::HardBreak => match table.as_mut() {
                Some(buffer) => buffer.append(" ", normal()),
                None => {
                    lines.push(Line::default());
                    literal.push(false);
                    hangs.push(current_hang(&lists, quote_depth));
                }
            },
            Event::Rule => {
                block_gap(&mut lines, &mut literal, &mut hangs);
                *lines.last_mut().expect("line exists") = Line::styled("─".repeat(width), border());
                *literal.last_mut().expect("literal exists") = true;
                lines.push(Line::default());
                literal.push(false);
                hangs.push(None);
            }
            Event::TaskListMarker(checked) => {
                // Replace the bullet the item pushed: the box is the marker.
                let line = lines.last_mut().expect("line exists");
                if let Some(last) = line.spans.last_mut()
                    && (last.text.ends_with("• ") || last.text.ends_with("◦ "))
                {
                    let cut = last.text.len() - "• ".len();
                    last.text.truncate(cut);
                }
                line.push(if checked { "☑ " } else { "☐ " }, accent());
            }
            Event::FootnoteReference(reference) => push_inline(
                table.as_mut(),
                &mut lines,
                &format!("[{reference}]"),
                accent(),
            ),
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                if table.is_none() {
                    begin_inline(&mut lines, &mut hangs, &lists, quote_depth);
                }
                push_inline(table.as_mut(), &mut lines, &format!("${math}$"), muted());
            }
        }
        literal.resize(lines.len(), false);
        hangs.resize(lines.len(), None);
    }
    while lines.last().is_some_and(Line::is_empty) {
        lines.pop();
        literal.pop();
        hangs.pop();
    }
    // Leading blank rows come from the seed line or a gap before the first
    // block; the transcript owns spacing above the message.
    let leading = lines.iter().take_while(|line| line.is_empty()).count();
    lines.drain(..leading);
    literal.drain(..leading);
    hangs.drain(..leading);
    // A source that ends at a block boundary keeps the gap that follows its
    // last block, so a settled prefix laid out alone concatenates with the
    // rest into exactly the whole message's rows (`settled_prefix_end`).
    if !lines.is_empty() && source.trim_end_matches([' ', '\t']).ends_with("\n\n") {
        lines.push(Line::default());
        literal.push(false);
        hangs.push(None);
    }
    let mut output = Vec::with_capacity(lines.len());
    for ((line, literal), hang) in lines.into_iter().zip(literal).zip(hangs) {
        match hang {
            Some(hang) if hang.width > 0 && hang.width < width => {
                // The first row already carries its rail and marker; wrap the
                // body alone, then re-attach the prefix as blanks (or the rail
                // for quotes) so continuation rows hang under the text.
                let mut prefix = Line::default();
                let mut body = Line::default();
                let mut taken = 0;
                for span in line.spans {
                    if taken >= hang.width {
                        body.push(span.text, span.style);
                        continue;
                    }
                    let mut head = String::new();
                    let mut rest = String::new();
                    for character in span.text.chars() {
                        if taken < hang.width {
                            head.push(character);
                            taken += unicode_width::UnicodeWidthChar::width(character)
                                .unwrap_or_default();
                        } else {
                            rest.push(character);
                        }
                    }
                    prefix.push(head, span.style);
                    body.push(rest, span.style);
                }
                let body_width = width - hang.width;
                let wrapped = if literal {
                    wrap_line_chars(body, body_width)
                } else {
                    wrap_line(body, body_width)
                };
                for (index, mut row) in wrapped.into_iter().enumerate() {
                    trim_trailing_whitespace(&mut row);
                    let mut assembled = if index == 0 {
                        prefix.clone()
                    } else {
                        let mut continuation = Line::default();
                        push_quote_rail(&mut continuation, hang.rail);
                        continuation.push(
                            " ".repeat(hang.width - quote_rail_width(hang.rail)),
                            normal(),
                        );
                        continuation
                    };
                    for span in row.spans {
                        assembled.push(span.text, span.style);
                    }
                    output.push(assembled);
                }
            }
            Some(_) | None => {
                if literal {
                    output.extend(wrap_line_chars(line, width));
                } else {
                    for mut row in wrap_line(line, width) {
                        trim_trailing_whitespace(&mut row);
                        output.push(row);
                    }
                }
            }
        }
    }
    output
}

/// Drop whitespace a word-wrap left at the end of a prose row. Rows compare
/// by text in goldens and diffs; a dangling space is invisible and costly.
fn trim_trailing_whitespace(row: &mut Line) {
    while let Some(last) = row.spans.last_mut() {
        let trimmed = last.text.trim_end().len();
        if trimmed == last.text.len() {
            break;
        }
        last.text.truncate(trimmed);
        if last.text.is_empty() {
            row.spans.pop();
        } else {
            break;
        }
    }
}

/// Hanging prefix of one laid-out line: how many cells the marker and rail
/// occupy, and how many quote rails to repeat on continuation rows.
#[derive(Clone, Copy)]
struct Hang {
    width: usize,
    rail: usize,
}

/// Cells taken by `depth` nested quote rails.
const fn quote_rail_width(depth: usize) -> usize {
    depth * 2
}

fn push_quote_rail(line: &mut Line, depth: usize) {
    for _ in 0..depth {
        line.push("▎ ", muted());
    }
}

/// The hang a fresh prose line inside the current lists/quotes should get:
/// continuation text of a list item indents under the item; quoted prose
/// repeats the rail.
fn current_hang(lists: &[Option<(u64, usize)>], quote_depth: usize) -> Option<Hang> {
    if lists.is_empty() && quote_depth == 0 {
        return None;
    }
    // A paragraph inside an item aligns with the item text: the marker's
    // width for bullets is 2 cells per depth; ordered markers vary, so use
    // the depth-based indent plus the widest marker seen so far.
    let list_indent = lists.iter().enumerate().fold(0, |acc, (index, list)| {
        let indent = if index + 1 == lists.len() {
            match list {
                Some((_, digits)) => *digits + 2,
                None => 2,
            }
        } else {
            2
        };
        acc + indent
    });
    Some(Hang {
        width: quote_rail_width(quote_depth) + list_indent,
        rail: quote_depth,
    })
}

/// Start a fresh prose line, when the current one is empty, with the rail
/// and indent the enclosing quote and list context call for. Inline events
/// arriving on an item's marker line leave it alone.
fn begin_inline(
    lines: &mut [Line],
    hangs: &mut Vec<Option<Hang>>,
    lists: &[Option<(u64, usize)>],
    quote_depth: usize,
) {
    let line = lines.last_mut().expect("line exists");
    if !line.is_empty() {
        return;
    }
    let Some(hang) = current_hang(lists, quote_depth) else {
        return;
    };
    push_quote_rail(line, quote_depth);
    let indent = hang.width - quote_rail_width(quote_depth);
    if indent > 0 {
        line.push(" ".repeat(indent), normal());
    }
    hangs.resize(lines.len(), None);
    *hangs.last_mut().expect("hang exists") = Some(hang);
}

/// Ensure the last line is empty and mark one blank separator row before it,
/// so the next block starts exactly one row below the previous one.
fn block_gap(lines: &mut Vec<Line>, literal: &mut Vec<bool>, hangs: &mut Vec<Option<Hang>>) {
    ensure_line(lines, literal, hangs);
    // `ensure_line` leaves one empty trailing line; a gap needs an empty
    // separator plus the empty line the block will fill.
    if lines.len() >= 2 && !lines[lines.len() - 2].is_empty() {
        lines.push(Line::default());
        literal.push(false);
        hangs.push(None);
    }
}

/// Routes inline content to the open table cell when one exists, otherwise to
/// the current transcript line.
fn push_inline(table: Option<&mut TableBuffer>, lines: &mut [Line], text: &str, style: Style) {
    match table {
        Some(buffer) => buffer.append(text, style),
        None => lines
            .last_mut()
            .expect("line exists")
            .push(text.to_owned(), style),
    }
}

/// Narrowest useful column; below this per column the table stacks instead.
const TABLE_MIN_COLUMN_WIDTH: usize = 3;
/// Display width of the " │ " column separator.
const TABLE_SEPARATOR_WIDTH: usize = 3;

/// Buffers one table's rows of styled cells while the parser walks it, so the
/// layout can size columns from complete content.
#[derive(Default)]
struct TableBuffer {
    rows: Vec<Vec<Line>>,
    row: Option<Vec<Line>>,
    has_header: bool,
}

impl TableBuffer {
    fn begin_row(&mut self) {
        self.end_row();
        self.row = Some(Vec::new());
    }

    fn end_row(&mut self) {
        if let Some(row) = self.row.take()
            && !row.is_empty()
        {
            self.rows.push(row);
        }
    }

    fn begin_cell(&mut self) {
        self.row.get_or_insert_default().push(Line::default());
    }

    /// Appends inline content to the current cell, creating row and cell on
    /// demand so malformed or partial input never panics.
    fn append(&mut self, text: &str, style: Style) {
        let row = self.row.get_or_insert_default();
        if row.is_empty() {
            row.push(Line::default());
        }
        let safe = text
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        row.last_mut().expect("cell exists").push(safe, style);
    }
}

/// Lays a buffered table out as aligned columns sized from content. When the
/// natural table overflows the width, columns shrink proportionally and cells
/// wrap within their column; when even minimum columns cannot fit, rows stack
/// as `header: value` lines.
fn layout_table(rows: &[Vec<Line>], has_header: bool, width: usize) -> Vec<Line> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let overhead = TABLE_SEPARATOR_WIDTH * (columns - 1);
    let available = width.saturating_sub(overhead);
    if available < columns * TABLE_MIN_COLUMN_WIDTH {
        return layout_table_stacked(rows, has_header, width);
    }
    let natural = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row.get(column).map_or(0, Line::width))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect::<Vec<_>>();
    let mut widths = natural.clone();
    if natural.iter().sum::<usize>() > available {
        // Columns already within their fair share keep their natural width;
        // repeat because each fixed column raises the fair share of the rest.
        let mut remaining = available;
        let mut flexible = (0..columns).collect::<Vec<_>>();
        loop {
            let fair = remaining / flexible.len().max(1);
            let (fits, wide): (Vec<usize>, Vec<usize>) = flexible
                .into_iter()
                .partition(|column| natural[*column] <= fair);
            if fits.is_empty() || wide.is_empty() {
                flexible = if wide.is_empty() { fits } else { wide };
                break;
            }
            for column in fits {
                remaining = remaining.saturating_sub(natural[column]);
            }
            flexible = wide;
        }
        // The overflowing columns split the remaining space proportionally.
        let flexible_total = flexible
            .iter()
            .map(|column| natural[*column])
            .sum::<usize>()
            .max(1);
        for column in flexible {
            widths[column] = (natural[column] * remaining / flexible_total)
                .max(TABLE_MIN_COLUMN_WIDTH)
                .min(natural[column].max(TABLE_MIN_COLUMN_WIDTH));
        }
        // Rounding and minimums can leave a small excess; take it back from
        // the widest columns so the sum fits `available` again.
        while widths.iter().sum::<usize>() > available {
            let Some((index, _)) = widths
                .iter()
                .enumerate()
                .filter(|(_, allocated)| **allocated > TABLE_MIN_COLUMN_WIDTH)
                .max_by_key(|(_, allocated)| **allocated)
            else {
                break;
            };
            widths[index] -= 1;
        }
    }
    let mut output = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let cells = (0..columns)
            .map(|column| wrap_line(row.get(column).cloned().unwrap_or_default(), widths[column]))
            .collect::<Vec<_>>();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for cell_row in 0..height {
            let mut line = Line::default();
            for (column, cell) in cells.iter().enumerate() {
                if column > 0 {
                    line.push(" │ ", muted());
                }
                let content = cell.get(cell_row).cloned().unwrap_or_default();
                let content_width = content.width();
                for span in content.spans {
                    line.push(span.text, span.style);
                }
                if column + 1 < columns {
                    line.push(
                        " ".repeat(widths[column].saturating_sub(content_width)),
                        muted(),
                    );
                }
            }
            output.push(line);
        }
        if row_index == 0 && has_header && rows.len() > 1 {
            let mut rule = Line::default();
            for (column, column_width) in widths.iter().enumerate() {
                if column > 0 {
                    rule.push("─┼─", muted());
                }
                rule.push("─".repeat(*column_width), muted());
            }
            output.push(rule);
        }
    }
    output
}

/// Very-narrow fallback: each data row becomes `header: value` lines with a
/// muted divider between rows.
fn layout_table_stacked(rows: &[Vec<Line>], has_header: bool, width: usize) -> Vec<Line> {
    let (header, data) = if has_header && rows.len() > 1 {
        (rows.first(), &rows[1..])
    } else {
        (None, rows)
    };
    let mut output = Vec::new();
    for (row_index, row) in data.iter().enumerate() {
        if row_index > 0 {
            output.push(Line::styled("---", muted()));
        }
        for (column, cell) in row.iter().enumerate() {
            let mut line = Line::default();
            if let Some(title) = header.and_then(|header| header.get(column)) {
                for span in &title.spans {
                    line.push(span.text.clone(), span.style);
                }
                line.push(": ", muted());
            }
            for span in &cell.spans {
                line.push(span.text.clone(), span.style);
            }
            output.extend(wrap_line(line, width.max(1)));
        }
    }
    output
}

/// The panel's left rail: a heavy bar plus one cell, on the first row of
/// every source line and on both padding rows.
const CODE_PANEL_GUTTER: &str = "┃  ";
/// The rail on a continuation row: a source line that wrapped keeps flowing
/// under the arrow, so a wrapped line never reads as a new one.
const CODE_PANEL_WRAP_GUTTER: &str = "↪  ";
/// Display width of the gutter plus the one cell of padding before content.
pub(super) const CODE_PANEL_INSET: usize = 3;

/// Bytes past which a code block skips tree-sitter and renders plain;
/// highlighting must never stall a frame.
pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;

/// A recognized highlight capture name paired with its theme style.
type HighlightCapture = (&'static str, fn() -> Style);

/// Highlight capture names recognized in grammar queries, each mapped to a
/// theme style. `HighlightConfiguration::configure` resolves query captures
/// against these by longest dotted prefix, so `keyword.control.repeat` lands
/// on `keyword`; captures matching nothing keep the plain panel style.
const HIGHLIGHT_CAPTURES: &[HighlightCapture] = &[
    ("attribute", code_constant),
    ("comment", code_comment),
    ("constant", code_constant),
    ("constructor", code_type),
    ("escape", code_constant),
    ("function", code_function),
    ("keyword", code_keyword),
    ("label", code_constant),
    ("number", code_constant),
    ("operator", code_property),
    ("property", code_property),
    ("punctuation.bracket", code_punctuation),
    ("punctuation.delimiter", code_punctuation),
    ("string", code_string),
    ("string.special.key", code_property),
    ("tag", code_function),
    ("type", code_type),
    ("variable.builtin", code_keyword),
    ("variable.parameter", code_property),
];

/// Builds one grammar's highlight configuration on first use. Compiling the
/// highlight query is the expensive step, so each grammar pays it once; a
/// grammar whose query fails to compile stays `None` and its blocks render
/// plain forever rather than retrying every frame.
fn highlight_configuration(
    cell: &'static OnceLock<Option<HighlightConfiguration>>,
    name: &str,
    language: Language,
    highlights: &str,
) -> Option<&'static HighlightConfiguration> {
    cell.get_or_init(|| {
        let mut configuration =
            HighlightConfiguration::new(language, name, highlights, "", "").ok()?;
        let names = HIGHLIGHT_CAPTURES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        configuration.configure(&names);
        Some(configuration)
    })
    .as_ref()
}

/// Maps a fence tag (with common aliases) to a bundled grammar's highlight
/// configuration. Unknown or absent tags return `None` and render plain.
/// TypeScript, TSX, JSX, and C++ queries only extend their base language's
/// query, so those arms concatenate the extension ahead of the base (earlier
/// patterns win in tree-sitter queries).
pub(crate) fn fence_highlight_configuration(tag: &str) -> Option<&'static HighlightConfiguration> {
    macro_rules! grammar {
        ($name:literal, $language:expr, $highlights:expr) => {{
            static CELL: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
            highlight_configuration(&CELL, $name, $language.into(), $highlights)
        }};
    }
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => grammar!(
            "rust",
            tree_sitter_rust::LANGUAGE,
            tree_sitter_rust::HIGHLIGHTS_QUERY
        ),
        "toml" => grammar!(
            "toml",
            tree_sitter_toml_ng::LANGUAGE,
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY
        ),
        "json" | "jsonc" | "json5" => grammar!(
            "json",
            tree_sitter_json::LANGUAGE,
            tree_sitter_json::HIGHLIGHTS_QUERY
        ),
        "yaml" | "yml" => grammar!(
            "yaml",
            tree_sitter_yaml::LANGUAGE,
            tree_sitter_yaml::HIGHLIGHTS_QUERY
        ),
        "bash" | "sh" | "shell" | "zsh" => grammar!(
            "bash",
            tree_sitter_bash::LANGUAGE,
            tree_sitter_bash::HIGHLIGHT_QUERY
        ),
        "python" | "py" | "python3" => grammar!(
            "python",
            tree_sitter_python::LANGUAGE,
            tree_sitter_python::HIGHLIGHTS_QUERY
        ),
        "javascript" | "js" | "mjs" | "cjs" => grammar!(
            "javascript",
            tree_sitter_javascript::LANGUAGE,
            tree_sitter_javascript::HIGHLIGHT_QUERY
        ),
        "jsx" => grammar!(
            "jsx",
            tree_sitter_javascript::LANGUAGE,
            &format!(
                "{}\n{}",
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            )
        ),
        "typescript" | "ts" => grammar!(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            &format!(
                "{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            )
        ),
        "tsx" => grammar!(
            "tsx",
            tree_sitter_typescript::LANGUAGE_TSX,
            &format!(
                "{}\n{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            )
        ),
        "go" | "golang" => grammar!(
            "go",
            tree_sitter_go::LANGUAGE,
            tree_sitter_go::HIGHLIGHTS_QUERY
        ),
        "c" | "h" => grammar!("c", tree_sitter_c::LANGUAGE, tree_sitter_c::HIGHLIGHT_QUERY),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" => grammar!(
            "cpp",
            tree_sitter_cpp::LANGUAGE,
            &format!(
                "{}\n{}",
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                tree_sitter_c::HIGHLIGHT_QUERY
            )
        ),
        _ => None,
    }
}

/// Runs tree-sitter highlighting over one code block, returning one styled
/// line per source line. Tree-sitter is error-tolerant, so partial or invalid
/// code still highlights; any highlighter failure returns `None` and the
/// caller falls back to plain panel text.
///
/// Kept out of line: this is the cold, expensive path, and letting it inline
/// into the panel layout measured +7 % on the fence-free 32 KiB streaming
/// ceiling when the capture table grew (U4), through code layout alone.
#[inline(never)]
fn highlighted_code_lines(configuration: &HighlightConfiguration, text: &str) -> Option<Vec<Line>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(configuration, text.as_bytes(), None, |_| None)
        .ok()?;
    let mut lines = vec![Line::default()];
    let mut active: Vec<Highlight> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => active.push(highlight),
            HighlightEvent::HighlightEnd => {
                active.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = active
                    .last()
                    .and_then(|highlight| HIGHLIGHT_CAPTURES.get(highlight.0))
                    .map_or_else(normal, |(_, style)| style());
                for (index, part) in text.get(start..end)?.split('\n').enumerate() {
                    if index > 0 {
                        lines.push(Line::default());
                    }
                    let safe = part
                        .chars()
                        .filter_map(terminal_safe_character)
                        .collect::<String>();
                    lines
                        .last_mut()
                        .expect("lines starts populated")
                        .push(safe, style);
                }
            }
        }
    }
    // The block's trailing newline would otherwise read as an extra empty
    // content row that the plain `str::lines` path never produces.
    if text.ends_with('\n') && lines.last().is_some_and(Line::is_empty) {
        lines.pop();
    }
    Some(lines)
}

/// Buffers one code block's text while the parser walks it, so the panel can
/// be laid out from complete content. Streamed partial input closes the block
/// at end of input, so an unterminated fence still renders as a panel.
struct CodeBlockBuffer {
    language: Option<String>,
    text: String,
}

impl CodeBlockBuffer {
    fn new(kind: &CodeBlockKind) -> Self {
        let language = match kind {
            CodeBlockKind::Fenced(info) => info
                .split([',', ' ', '\t'])
                .next()
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
            CodeBlockKind::Indented => None,
        };
        Self {
            language,
            text: String::new(),
        }
    }
}

/// Lays a buffered code block out as a panel (see [`panel_rows`]) labelled
/// with the fence's language tag.
///
/// An unterminated fence reaches here too (pulldown closes it at end of
/// input), so a streaming panel has the same rows as the finished one and
/// does not jump when the closing fence arrives.
///
/// With `highlight` set, a recognized fence tag colors the content through
/// its tree-sitter grammar; diff fences keep their dedicated coloring, and
/// oversized blocks or highlighter failures fall back to plain text.
fn layout_code_panel(block: &CodeBlockBuffer, width: usize, highlight: bool) -> Vec<Line> {
    let diff = block.language.as_deref() == Some("diff");
    let label = block.language.as_deref();
    let highlighted = if highlight && !diff && block.text.len() <= MAX_HIGHLIGHT_BYTES {
        label
            .and_then(fence_highlight_configuration)
            .and_then(|configuration| highlighted_code_lines(configuration, &block.text))
    } else {
        None
    };
    match highlighted {
        Some(content) => panel_rows(content, label, width),
        None => panel_rows(
            block.text.lines().map(|source_line| {
                let safe = source_line
                    .chars()
                    .filter_map(terminal_safe_character)
                    .collect::<String>();
                let style = if diff {
                    diff_line_style(&safe)
                } else {
                    normal()
                };
                Line::styled(safe, style)
            }),
            label,
            width,
        ),
    }
}

/// Columns a panel at `width` leaves for content: the rail, its padding
/// cell, and never less than one cell.
pub(crate) fn panel_content_width(width: usize) -> usize {
    width.saturating_sub(CODE_PANEL_INSET).max(1)
}

/// The one panel implementation, shared by fenced code and tool detail: a
/// top padding row carrying `label` right-aligned in `muted` (blank without
/// one), the body lines character-wrapped to the content width, and a blank
/// bottom padding row. Every row is padded to `width` so the tint reads as
/// one solid slab. The rail is `┃` on the first row of each body line and
/// `↪` on the rows a long line wrapped onto, so a wrapped line never reads
/// as a new one. A line that already fits is not wrapped: `wrap_line_chars`
/// copies its text, and most panel rows (every pre-truncated tool row) fit.
pub(crate) fn panel_rows(
    body: impl IntoIterator<Item = Line>,
    label: Option<&str>,
    width: usize,
) -> Vec<Line> {
    panel_rows_at(body, label, width, 0)
}

/// [`panel_rows`] with `indent` blank cells before the rail on the terminal
/// background, for panels that sit at the content column rather than the
/// pane edge. The margin rides in `Line::indent`, so it costs no span.
pub(crate) fn panel_rows_at(
    body: impl IntoIterator<Item = Line>,
    label: Option<&str>,
    width: usize,
    indent: usize,
) -> Vec<Line> {
    let styles = PanelStyles::current();
    let content_width = panel_content_width(width);
    let mut top = Line::default();
    if let Some(label) = label {
        // ` label ` sits flush with the right edge; its trailing cell mirrors
        // the padding cell after the gutter.
        let label = label
            .chars()
            .filter_map(terminal_safe_character)
            .take(content_width.saturating_sub(2))
            .collect::<String>();
        if !label.is_empty() {
            let label_width = label.chars().count() + 2;
            top.push(" ".repeat(content_width - label_width), normal());
            top.push(format!(" {label} "), muted());
        }
    }
    let mut output = vec![styles.row(top, width, false, indent)];
    for line in body {
        if line.width() <= content_width {
            output.push(styles.row(line, width, false, indent));
            continue;
        }
        for (index, wrapped) in wrap_line_chars(line, content_width).into_iter().enumerate() {
            output.push(styles.row(wrapped, width, index > 0, indent));
        }
    }
    output.push(styles.row(Line::default(), width, false, indent));
    output
}

/// The panel's styles, read from the active theme once per panel rather than
/// once per row: `theme::active()` copies the whole palette out of a
/// thread-local, and a frame of 32 expanded tool panels builds hundreds of
/// rows.
#[derive(Clone, Copy)]
struct PanelStyles {
    /// The rail glyph plus its padding cell.
    rail: Style,
    /// Trailing padding.
    fill: Style,
    surface: Color,
}

impl PanelStyles {
    fn current() -> Self {
        let palette = theme::active();
        Self {
            rail: Style::color(palette.border).on(palette.surface),
            fill: Style::color(palette.text).on(palette.surface),
            surface: palette.surface,
        }
    }

    /// One physical panel row: the rail (`┃`, or `↪` when `continuation`
    /// marks a wrapped source line) in `border`, one cell of padding, the
    /// content, and enough trailing padding to carry the surface tint to the
    /// full width. A span that brings its own background (diff add/remove
    /// tints) keeps it; the surface only fills spans that have none. A width
    /// too narrow for the rail itself truncates rather than overflowing.
    fn row(self, content: Line, width: usize, continuation: bool, indent: usize) -> Line {
        let rail = if continuation {
            CODE_PANEL_WRAP_GUTTER
        } else {
            CODE_PANEL_GUTTER
        };
        let mut row = Line {
            indent,
            spans: Vec::with_capacity(content.spans.len() + 2),
        };
        // Rail glyph, its trailing space, and the padding cell in one span:
        // the glyph color is invisible on the spaces, and a frame of 32
        // expanded panels allocates one string per span per row.
        row.spans.push(Span {
            text: rail.to_owned(),
            style: self.rail,
        });
        let mut used = CODE_PANEL_INSET;
        // Content spans are appended without the merge check `Line::push`
        // does: the rail stays its own span even when the first content span
        // happens to share its style, so callers can address it.
        for mut span in content.spans {
            used += text_width(&span.text);
            if span.style.background.is_none() {
                span.style = span.style.on(self.surface);
            }
            row.spans.push(span);
        }
        if used < width {
            row.spans.push(Span {
                text: " ".repeat(width - used),
                style: self.fill,
            });
        }
        if width <= CODE_PANEL_INSET {
            return truncate_line(row, width);
        }
        row
    }
}

/// Test entry for a single panel row; production code goes through
/// [`panel_rows`] so the palette is read once per panel.
#[cfg(test)]
pub(crate) fn code_panel_row(content: Line, width: usize, continuation: bool) -> Line {
    PanelStyles::current().row(content, width, continuation, 0)
}

pub(crate) fn append_safe_text(lines: &mut Vec<Line>, text: &str, style: Style) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        let safe = part
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        lines.last_mut().expect("line exists").push(safe, style);
    }
}

fn ensure_line(lines: &mut Vec<Line>, literal: &mut Vec<bool>, hangs: &mut Vec<Option<Hang>>) {
    if !lines.last().is_none_or(Line::is_empty) {
        lines.push(Line::default());
        literal.push(false);
        hangs.push(None);
    }
    literal.resize(lines.len(), false);
    hangs.resize(lines.len(), None);
}

/// Whether `source` contains a fenced code block that highlighting could
/// color. Cheap line scan; false positives only cost one skipped job.
pub(crate) fn has_fenced_code(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("```") || trimmed.starts_with("~~~")
    })
}

#[cfg(test)]
pub(super) mod tests;
