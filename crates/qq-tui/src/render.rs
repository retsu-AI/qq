//! Styled text primitives and the terminal writer. Everything the TUI draws
//! is a `Line` of `Span`s; the frame differ compares whole lines, so equality
//! on these types is the redraw criterion.

use std::io::{self, Write};

use crossterm::{
    queue,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
};
use unicode_width::UnicodeWidthChar;

use crate::{app::terminal_safe_character, theme};

/// Text attributes packed into one byte. `Style` is copied and compared on
/// every span in the wrap and diff hot paths; adding a fourth `bool` field
/// measured +5 % on the 32 KiB streaming ceiling, so the flags live here and
/// `Style` stays at nine bytes (two `Option<Color>` plus this).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Attributes(u8);

impl Attributes {
    const BOLD: u8 = 1;
    const DIM: u8 = 1 << 1;
    const ITALIC: u8 = 1 << 2;
    const UNDERLINE: u8 = 1 << 3;

    const fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    /// Flags set in `self` but not in `other`.
    const fn added_over(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Style {
    pub(crate) color: Option<Color>,
    pub(crate) background: Option<Color>,
    attributes: Attributes,
}

impl Style {
    pub(crate) const fn color(color: Color) -> Self {
        Self {
            color: Some(color),
            background: None,
            attributes: Attributes(0),
        }
    }

    pub(crate) const fn on(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }

    pub(crate) const fn bold(mut self) -> Self {
        self.attributes.0 |= Attributes::BOLD;
        self
    }

    pub(crate) const fn dim(mut self) -> Self {
        self.attributes.0 |= Attributes::DIM;
        self
    }

    pub(crate) const fn italic(mut self) -> Self {
        self.attributes.0 |= Attributes::ITALIC;
        self
    }

    pub(crate) const fn underline(mut self) -> Self {
        self.attributes.0 |= Attributes::UNDERLINE;
        self
    }

    #[cfg(test)]
    pub(crate) const fn is_bold(self) -> bool {
        self.attributes.has(Attributes::BOLD)
    }

    #[cfg(test)]
    pub(crate) const fn is_dim(self) -> bool {
        self.attributes.has(Attributes::DIM)
    }

    #[cfg(test)]
    pub(crate) const fn is_italic(self) -> bool {
        self.attributes.has(Attributes::ITALIC)
    }

    #[cfg(test)]
    pub(crate) const fn is_underline(self) -> bool {
        self.attributes.has(Attributes::UNDERLINE)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) text: String,
    pub(crate) style: Style,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Line {
    /// Blank cells before the first span. A pane wider than its content
    /// shifts every row by the same margin; carrying it as a count keeps the
    /// shift free for rows shared with the width-keyed caches.
    pub(crate) indent: usize,
    pub(crate) spans: Vec<Span>,
}

impl Line {
    pub(crate) fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            indent: 0,
            spans: vec![Span {
                text: text.into(),
                style,
            }],
        }
    }

    pub(crate) fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(Span { text, style });
    }

    pub(crate) fn width(&self) -> usize {
        self.indent
            + self
                .spans
                .iter()
                .flat_map(|span| span.text.chars())
                .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
                .sum::<usize>()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }
}

/// Role styles read the thread's active palette (see `theme.rs`); the
/// renderer refreshes it once per frame so a theme switch repaints
/// everything without threading a theme through every leaf.
pub(crate) fn normal() -> Style {
    Style::color(theme::active().text)
}

/// Secondary text. Color alone carries the step back: `Dim` on top would
/// sink it below readability on many terminals.
pub(crate) fn muted() -> Style {
    Style::color(theme::active().muted)
}

pub(crate) fn accent() -> Style {
    Style::color(theme::active().accent)
}

pub(crate) fn brand() -> Style {
    Style::color(theme::active().brand)
}

pub(crate) fn warning() -> Style {
    Style::color(theme::active().warning)
}

pub(crate) fn failure() -> Style {
    Style::color(theme::active().error)
}

pub(crate) fn success() -> Style {
    Style::color(theme::active().success)
}

/// Running-state color for spinners and activity labels.
pub(crate) fn info() -> Style {
    Style::color(theme::active().info)
}

/// Pane dividers and rules at rest.
pub(crate) fn border() -> Style {
    Style::color(theme::active().border)
}

/// Background for the selected row in pickers and lists.
pub(crate) fn selection(style: Style) -> Style {
    style.on(theme::active().selection_bg)
}

/// Dark surface tint behind code-block panels, distinct from the terminal
/// background so a padded block reads as one solid slab.
pub(crate) fn surface_color() -> Color {
    theme::active().surface
}

pub(crate) fn surface(style: Style) -> Style {
    style.on(surface_color())
}

/// Inline code: plain text on the panel surface, so a `name` in prose shares
/// its tint with fenced blocks instead of shouting in the warning color.
pub(crate) fn inline_code() -> Style {
    surface(normal())
}

/// Link text: accent and underlined. The URL is not rendered (no OSC 8), so
/// the underline is what marks the span as a link.
pub(crate) fn link() -> Style {
    accent().underline()
}

/// Syntax palette for highlighted code panels, derived from theme roles so
/// every theme colors code in its own voice: keywords in brand, strings in
/// success, comments in muted, functions in accent, types in warning,
/// constants in error, properties in text. Anything a grammar leaves
/// uncaptured keeps the plain panel text style.
pub(crate) fn code_keyword() -> Style {
    Style::color(theme::active().brand)
}

pub(crate) fn code_string() -> Style {
    Style::color(theme::active().success)
}

pub(crate) fn code_comment() -> Style {
    Style::color(theme::active().muted).italic()
}

pub(crate) fn code_function() -> Style {
    Style::color(theme::active().accent)
}

pub(crate) fn code_type() -> Style {
    Style::color(theme::active().warning)
}

pub(crate) fn code_constant() -> Style {
    Style::color(theme::active().error)
}

pub(crate) fn code_property() -> Style {
    Style::color(theme::active().text)
}

/// Unified-diff line coloring: additions in success on the add tint,
/// removals in error on the delete tint, hunk headers muted, context lines
/// normal. Diff lines never reflow.
pub(crate) fn diff_line_style(line: &str) -> Style {
    let palette = theme::active();
    if line.starts_with("@@") {
        muted()
    } else if line.starts_with('+') {
        success().on(palette.diff_add_bg)
    } else if line.starts_with('-') {
        failure().on(palette.diff_del_bg)
    } else {
        normal()
    }
}

/// Write one row. The caller has reset attributes and colors at the start of
/// the row; from there only the differences between consecutive spans are
/// emitted, so a row of one style costs one color sequence instead of a
/// reset-and-set per span. Attributes that turn off (bold to plain) need a
/// reset, which then re-applies the surviving colors.
pub(crate) fn write_line(output: &mut impl Write, line: &Line) -> io::Result<()> {
    let mut current = Style::default();
    if line.indent > 0 {
        // The caller cleared the row, so a cursor move paints the margin
        // without emitting spaces.
        queue!(
            output,
            crossterm::cursor::MoveRight(u16::try_from(line.indent).unwrap_or(u16::MAX))
        )?;
    }
    for span in &line.spans {
        if span.text.is_empty() {
            continue;
        }
        let next = span.style;
        if next != current {
            // Any attribute that turns off needs SGR 0; bits set in current
            // and clear in next.
            let attribute_dropped = current.attributes.added_over(next.attributes).0 != 0;
            if attribute_dropped {
                // SGR 0 clears colors too; no separate color reset needed.
                queue!(output, SetAttribute(Attribute::Reset))?;
                current = Style::default();
            }
            match (current.color, next.color) {
                (same, Some(color)) if same != Some(color) => {
                    queue!(output, SetForegroundColor(color))?;
                }
                (Some(_), None) => queue!(output, SetForegroundColor(Color::Reset))?,
                _ => {}
            }
            match (current.background, next.background) {
                (same, Some(background)) if same != Some(background) => {
                    queue!(output, SetBackgroundColor(background))?;
                }
                (Some(_), None) => queue!(output, SetBackgroundColor(Color::Reset))?,
                _ => {}
            }
            let added = next.attributes.added_over(current.attributes);
            if added.has(Attributes::BOLD) {
                queue!(output, SetAttribute(Attribute::Bold))?;
            }
            if added.has(Attributes::DIM) {
                queue!(output, SetAttribute(Attribute::Dim))?;
            }
            if added.has(Attributes::ITALIC) {
                queue!(output, SetAttribute(Attribute::Italic))?;
            }
            if added.has(Attributes::UNDERLINE) {
                queue!(output, SetAttribute(Attribute::Underlined))?;
            }
            current = next;
        }
        // Most spans are already terminal-safe; scan once and only allocate
        // when something must be dropped.
        if span
            .text
            .chars()
            .all(|c| terminal_safe_character(c) == Some(c))
        {
            queue!(output, Print(&span.text))?;
        } else {
            let safe = span
                .text
                .chars()
                .filter_map(terminal_safe_character)
                .collect::<String>();
            queue!(output, Print(safe))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(line: &Line) -> String {
        let mut out = Vec::new();
        write_line(&mut out, line).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_single_style_row_sets_its_color_once() {
        let mut line = Line::styled("alpha ", Style::color(Color::Red));
        line.push("beta", Style::color(Color::Red));
        let out = bytes(&line);
        // `push` merges same-style spans, so one span, one color, no reset.
        assert_eq!(out.matches("\x1b[38;5;9m").count(), 1, "{out:?}");
        assert!(!out.contains("\x1b[0m"), "{out:?}");
    }

    #[test]
    fn only_differences_between_spans_are_emitted() {
        let mut line = Line::styled("a", Style::color(Color::Red));
        line.push("b", Style::color(Color::Red).bold());
        line.push("c", Style::color(Color::Green).bold());
        let out = bytes(&line);
        // red, then bold added (no new color), then green replaces red.
        assert_eq!(out, "\x1b[38;5;9ma\x1b[1mb\x1b[38;5;10mc", "{out:?}");
    }

    #[test]
    fn dropping_an_attribute_resets_then_reapplies_the_color() {
        let mut line = Line::styled("a", Style::color(Color::Red).bold());
        line.push("b", Style::color(Color::Red));
        let out = bytes(&line);
        assert_eq!(out, "\x1b[38;5;9m\x1b[1ma\x1b[0m\x1b[38;5;9mb", "{out:?}");
    }

    #[test]
    fn underline_is_emitted_once_and_cleared_by_a_reset() {
        let mut line = Line::styled("see ", Style::color(Color::Red));
        line.push("here", Style::color(Color::Red).underline());
        line.push(" now", Style::color(Color::Red));
        let out = bytes(&line);
        // red, underline on (color kept), reset then red again for the tail.
        assert_eq!(
            out, "\x1b[38;5;9msee \x1b[4mhere\x1b[0m\x1b[38;5;9m now",
            "{out:?}"
        );
    }

    #[test]
    fn style_stays_nine_bytes() {
        assert_eq!(std::mem::size_of::<Style>(), 9);
    }

    #[test]
    fn unsafe_characters_are_dropped_without_touching_safe_spans() {
        let mut line = Line::styled("ok", Style::default());
        line.push("b\u{7}ell", Style::default());
        assert_eq!(bytes(&line), "okbell");
    }
}
