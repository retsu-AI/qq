//! Width-aware line manipulation: wrapping, truncation, indentation, and
//! viewport slicing. Every function preserves span styles across breaks and
//! never emits a row wider than the requested width.

use unicode_width::UnicodeWidthChar;

use crate::render::{Line, Style, muted};

/// One whitespace or word run inside a single span: a byte range plus its
/// display width. Tokens never cross span boundaries so styles are preserved
/// by construction; a word spanning two spans becomes two tokens that the
/// fitter treats as one group.
struct WrapToken {
    span: usize,
    range: std::ops::Range<usize>,
    whitespace: bool,
    width: usize,
}

/// Wraps prose at whitespace, preserving span styles across breaks. A single
/// word wider than the width falls back to character breaking, and the
/// whitespace a break lands on is dropped rather than carried over.
pub(crate) fn wrap_line(line: Line, width: usize) -> Vec<Line> {
    if line.width() <= width {
        return vec![line];
    }
    let spans = line.spans;
    let mut tokens: Vec<WrapToken> = Vec::new();
    for (span_index, span) in spans.iter().enumerate() {
        let mut start = 0;
        let mut kind = None;
        let mut token_width = 0;
        for (offset, character) in span.text.char_indices() {
            let whitespace = character.is_whitespace();
            let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
            match kind {
                Some(current) if current == whitespace => token_width += character_width,
                Some(current) => {
                    tokens.push(WrapToken {
                        span: span_index,
                        range: start..offset,
                        whitespace: current,
                        width: token_width,
                    });
                    start = offset;
                    kind = Some(whitespace);
                    token_width = character_width;
                }
                None => {
                    kind = Some(whitespace);
                    token_width = character_width;
                }
            }
        }
        if let Some(whitespace) = kind {
            tokens.push(WrapToken {
                span: span_index,
                range: start..span.text.len(),
                whitespace,
                width: token_width,
            });
        }
    }

    let mut output = vec![Line::default()];
    let mut used = 0_usize;
    let mut index = 0;
    while index < tokens.len() {
        let whitespace = tokens[index].whitespace;
        let mut end = index + 1;
        let mut group_width = tokens[index].width;
        while end < tokens.len() && tokens[end].whitespace == whitespace {
            group_width += tokens[end].width;
            end += 1;
        }
        let group = &tokens[index..end];
        index = end;
        if used + group_width <= width {
            let line = output.last_mut().expect("output starts populated");
            for token in group {
                let span = &spans[token.span];
                line.push(&span.text[token.range.clone()], span.style);
            }
            used += group_width;
        } else if whitespace {
            if used > 0 {
                output.push(Line::default());
                used = 0;
            }
        } else if group_width <= width {
            output.push(Line::default());
            let line = output.last_mut().expect("output starts populated");
            for token in group {
                let span = &spans[token.span];
                line.push(&span.text[token.range.clone()], span.style);
            }
            used = group_width;
        } else {
            for token in group {
                let span = &spans[token.span];
                for character in span.text[token.range.clone()].chars() {
                    let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
                    if used > 0 && used + character_width > width {
                        output.push(Line::default());
                        used = 0;
                    }
                    let mut encoded = [0; 4];
                    output
                        .last_mut()
                        .expect("output starts populated")
                        .push(&*character.encode_utf8(&mut encoded), span.style);
                    used += character_width;
                }
            }
        }
    }
    while output.len() > 1 && output.last().is_some_and(Line::is_empty) {
        output.pop();
    }
    output
}

/// Character wrapping for literal content (code blocks, table rows) where
/// dropping or moving whitespace would break alignment.
pub(crate) fn wrap_line_chars(line: Line, width: usize) -> Vec<Line> {
    let mut output = vec![Line::default()];
    let mut current_width = 0;
    for span in line.spans {
        let mut start = 0;
        for (offset, character) in span.text.char_indices() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
            if current_width > 0 && current_width + character_width > width {
                output
                    .last_mut()
                    .expect("output starts populated")
                    .push(&span.text[start..offset], span.style);
                output.push(Line::default());
                current_width = 0;
                start = offset;
            }
            current_width += character_width;
        }
        output
            .last_mut()
            .expect("output starts populated")
            .push(&span.text[start..], span.style);
    }
    output
}

pub(crate) fn indent_lines(
    lines: Vec<Line>,
    prefix: &str,
    prefix_style: Style,
    width: usize,
) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            let mut indented = Line::styled(prefix, prefix_style);
            // A row's own margin (`Line::indent`) sits after the prefix, so
            // it becomes cells here rather than being dropped.
            if line.indent > 0 {
                indented.push(" ".repeat(line.indent), prefix_style);
            }
            for span in line.spans {
                indented.push(span.text, span.style);
            }
            truncate_line(indented, width)
        })
        .collect()
}

pub(crate) fn truncate_line(line: Line, width: usize) -> Line {
    if line.width() <= width {
        return line;
    }
    if width <= 3 {
        return Line::styled(".".repeat(width), muted());
    }
    let mut output = Line::default();
    let mut used = 0_usize;
    let content_width = width - 3;
    for span in line.spans {
        let mut text = String::new();
        for character in span.text.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
            if used + character_width > content_width {
                break;
            }
            text.push(character);
            used += character_width;
        }
        output.push(text, span.style);
        if used >= content_width {
            break;
        }
    }
    output.push("...", muted());
    output
}

pub(crate) fn selection_viewport(
    lines: Vec<Line>,
    height: usize,
    selected_row: usize,
) -> Vec<Line> {
    let start = selected_row
        .saturating_sub(height / 2)
        .min(lines.len().saturating_sub(height));
    lines.into_iter().skip(start).take(height).collect()
}

#[cfg(test)]
pub(crate) fn transcript_viewport(mut lines: Vec<Line>, height: usize, offset: usize) -> Vec<Line> {
    let offset = offset.min(lines.len().saturating_sub(height));
    let end = lines.len().saturating_sub(offset);
    let start = end.saturating_sub(height);
    lines.drain(end..);
    lines.drain(..start);
    fit_height(lines, height)
}

pub(crate) fn fit_height(mut lines: Vec<Line>, height: usize) -> Vec<Line> {
    lines.resize(height, Line::default());
    lines.truncate(height);
    lines
}

pub(crate) fn preview(text: &str, width: usize) -> String {
    let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if plain.chars().count() <= width {
        plain
    } else {
        format!(
            "{}...",
            plain
                .chars()
                .take(width.saturating_sub(3))
                .collect::<String>()
        )
    }
}

pub(crate) fn bounded_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{Span, normal};

    fn frame_rows(frame: &[Line]) -> Vec<String> {
        frame
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn indent_lines_keeps_a_rows_own_margin_after_the_prefix() {
        let row = Line {
            indent: 3,
            spans: vec![Span {
                text: "┃ body".to_owned(),
                style: normal(),
            }],
        };
        let [out] = <[Line; 1]>::try_from(indent_lines(vec![row], "│ ", muted(), 40))
            .expect("one row in, one out");
        let text: String = out.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, "│    ┃ body");
        assert_eq!(out.indent, 0);
    }

    #[test]
    fn word_wrap_breaks_at_whitespace_and_preserves_styles() {
        let mut line = Line::default();
        line.push("manage ", normal().bold());
        line.push("daemons cleanly", normal());

        let wrapped = wrap_line(line, 10);

        assert_eq!(frame_rows(&wrapped), ["manage ", "daemons ", "cleanly"]);
        assert_eq!(wrapped[0].spans[0].style, normal().bold());
        assert_eq!(wrapped[1].spans[0].style, normal());
    }

    #[test]
    fn overlong_tokens_fall_back_to_character_breaks() {
        let mut line = Line::default();
        line.push("ab", normal().bold());
        line.push("cdefgh", normal());

        let wrapped = wrap_line(line, 4);

        assert_eq!(frame_rows(&wrapped), ["abcd", "efgh"]);
        assert_eq!(wrapped[0].spans.len(), 2);
        assert_eq!(wrapped[0].spans[0].style, normal().bold());
        assert_eq!(wrapped[0].spans[1].style, normal());
    }

    #[test]
    fn truncated_rows_never_exceed_the_terminal_width() {
        for width in 0..10 {
            let line = truncate_line(Line::styled("a long row", normal()), width);
            assert!(line.width() <= width);
        }
    }
}
