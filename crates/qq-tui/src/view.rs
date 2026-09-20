//! Frame assembly: composes chrome, transcript, and overlays into the lines
//! the renderer diffs against the previous frame.

mod chrome;
mod highlight;
mod layout;
mod markdown;
mod overlay;
mod sidebar;
mod tools;
mod transcript;
mod workspace;
mod wrap;

use std::{borrow::Cow, collections::HashMap, io, ops::Range};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use qq_protocol::{
    ApprovalMode, MessageId, MessageRole, MessageSnapshot, MessageState, RunId, SessionId,
    SessionStatus, ToolCallDisplay, ToolCallId, ToolCallSnapshot, ToolCallState,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    StatusItem,
    app::{App, SessionView, ToolDetail, terminal_safe_character},
    input::{Mode, SessionConfirm, approval_mode_label},
    render::{
        Line, Style, accent, border, brand, diff_line_style, failure, info, muted, normal,
        selection, success, warning, write_line,
    },
    theme,
    viewport::{View, Viewport},
};
use chrome::*;
pub(crate) use chrome::{ComposerMode, CursorPosition};
use highlight::HighlightKey;
pub(crate) use highlight::{Highlighted, Highlighter};
use layout::{FIXED_CHROME_ROWS, TranscriptPane, compute_layout};
pub(crate) use layout::{LayoutPrefs, PanePref};
use markdown::{has_fenced_code, markdown_lines, settled_prefix_end};
use overlay::*;
use sidebar::*;
use tools::*;
use transcript::*;
use workspace::*;
pub(crate) use wrap::preview;
#[cfg(test)]
use wrap::transcript_viewport;
use wrap::{
    bounded_tail, fit_height, indent_lines, selection_viewport, truncate_line, wrap_line,
    wrap_line_chars,
};

/// Bounds on the cells one frame lays out. A 48-inch display at a small font
/// is around 500 × 130; the bound exists so a pathological size costs a
/// bounded frame, not to describe any real terminal.
const MAX_RENDER_WIDTH: u16 = 1024;
const MAX_RENDER_HEIGHT: u16 = 512;
const MAX_LIVE_MARKDOWN_BYTES: usize = 32 * 1024;
const MAX_VISIBLE_MESSAGES: usize = 64;
/// Rows of a streaming message's open tail laid out per frame. Bounded by
/// the tallest body a frame can show plus headroom, not by the terminal.
const MAX_LIVE_MARKDOWN_ROWS: usize = 160;
/// Completed messages at or below these bounds retain full markdown styling.
/// Larger messages use a sparse plain-text row index so scrolling stays
/// complete without caching every rendered row.
const MAX_FULL_MARKDOWN_BYTES: usize = 64 * 1024;
const MAX_FULL_MARKDOWN_ROWS: usize = 4 * 1024;
const PLAIN_TEXT_CHECKPOINT_ROWS: usize = 1024;
const MAX_PLAIN_TEXT_CHECKPOINTS: usize = 4 * 1024;
const MAX_PLAIN_TEXT_ROW_BYTES: usize = 4 * 1024;

/// The cells a frame lays out for a terminal of `actual_size`: clamped to the
/// render bounds so a huge or degenerate size costs a bounded frame.
pub(crate) fn render_size(actual_size: (u16, u16)) -> (usize, usize) {
    (
        usize::from(actual_size.0.clamp(1, MAX_RENDER_WIDTH)),
        usize::from(actual_size.1.clamp(1, MAX_RENDER_HEIGHT)),
    )
}

/// Whether the sessions rail is on screen at `width` under `prefs`. Shared
/// with the app so event visibility and the toggle command agree with the
/// frame.
pub(crate) fn rail_visible(width: usize, prefs: LayoutPrefs, sessions: usize) -> bool {
    compute_layout(width, layout::MIN_HEIGHT, 0, prefs, sessions).rail_visible()
}

/// Frame assembly and the row diff against the previous frame. Retained
/// transcript state lives in one [`TranscriptCache`]; the highlighter is
/// separate because its results are keyed by message and width.
#[derive(Default)]
pub(crate) struct FrameRenderer {
    previous: Vec<Line>,
    size: Option<(u16, u16)>,
    /// Off-tick syntax highlighting for cached completed messages.
    pub(crate) highlighter: Highlighter,
    cache: TranscriptCache,
    /// `App::theme_generation` the caches were built under. Cached rows
    /// bake in colors, so a theme change discards every layout and forces
    /// a full repaint.
    theme_generation: u64,
    /// The viewport reconciled while building the last frame. `draw` hands it
    /// back to the app after the frame is composed; `frame` itself never
    /// mutates the model.
    viewport_update: Option<Viewport>,
    /// Where the terminal cursor belongs after the last frame, or hidden.
    cursor: Option<CursorPosition>,
}

impl FrameRenderer {
    /// Hand the viewport reconciled while building the last frame back to
    /// the model.
    pub(crate) fn commit(&mut self, app: &mut App) {
        if let Some(viewport) = self.viewport_update.take() {
            app.viewport = viewport;
        }
    }

    /// Forget the previous frame so the next draw repaints every row, after
    /// something else (an external editor) wrote to the terminal.
    pub(crate) fn invalidate(&mut self) {
        self.previous.clear();
        self.size = None;
    }

    /// Render one frame for a terminal of `actual_size` columns and rows and
    /// return the bytes that bring the terminal from the previous frame to
    /// this one. Only changed rows are emitted unless the size changed.
    pub fn draw(&mut self, app: &mut App, actual_size: (u16, u16)) -> io::Result<Vec<u8>> {
        let (width, height) = render_size(actual_size);
        let frame = self.frame(app, width, height);
        self.commit(app);
        let resized = self.size != Some(actual_size);
        let mut output = Vec::with_capacity(4096);
        queue!(&mut output, BeginSynchronizedUpdate)?;
        if resized {
            queue!(&mut output, Clear(ClearType::All))?;
        }
        for (row, line) in frame.iter().enumerate() {
            if resized || self.previous.get(row) != Some(line) {
                queue!(
                    &mut output,
                    MoveTo(
                        0,
                        u16::try_from(row).expect("bounded terminal row fits u16")
                    ),
                    SetAttribute(Attribute::Reset),
                    ResetColor,
                    Clear(ClearType::CurrentLine)
                )?;
                write_line(&mut output, line)?;
            }
        }
        queue!(&mut output, SetAttribute(Attribute::Reset), ResetColor)?;
        // The real cursor marks the composer caret; overlays and approvals own
        // input without a caret, so it hides then. Placing it after the rows
        // keeps IME candidate windows and screen readers anchored correctly.
        match self.cursor {
            Some(position) => {
                queue!(&mut output, MoveTo(position.column, position.row), Show)?;
            }
            None => queue!(&mut output, Hide)?,
        }
        queue!(&mut output, EndSynchronizedUpdate)?;
        self.previous = frame;
        self.size = Some(actual_size);
        Ok(output)
    }

    /// Build one frame from the model without changing it. The viewport clamp
    /// computed along the way is kept in `viewport_update` for `draw`.
    fn frame(&mut self, app: &App, width: usize, height: usize) -> Vec<Line> {
        theme::activate(app.theme().palette);
        self.viewport_update = None;
        if self.theme_generation != app.theme_generation {
            self.theme_generation = app.theme_generation;
            self.cache = TranscriptCache::default();
            self.invalidate();
        }
        if width < layout::MIN_WIDTH || height < layout::MIN_HEIGHT {
            return fit_height(
                vec![
                    Line::styled(" qq", brand().bold()),
                    Line::default(),
                    Line::styled("Terminal is too small.", warning()),
                    Line::styled("Resize to at least 32 x 9. Ctrl-C exits.", muted()),
                ],
                height,
            );
        }

        self.cursor = None;
        let mut lines = vec![top_row(app, width)];
        // The top row and the composer rule are fixed; the rule doubles as
        // the status and hint line so no row is spent on either. The composer
        // can grow with wrapped multi-line input, so it is laid out first and
        // the body takes what remains.
        let max_composer_rows = height
            .saturating_sub(FIXED_CHROME_ROWS)
            .saturating_sub(1)
            .clamp(1, layout::max_composer_rows(width, height));
        let draft_lines = queued_drafts(app, width);
        let (composer_lines, caret) = composer(app, width, max_composer_rows);
        let chrome_rows = FIXED_CHROME_ROWS - 1 + draft_lines.len() + composer_lines.len();
        let layout = compute_layout(width, height, chrome_rows, app.layout, app.sessions.len());
        let body_height = layout.body.height;
        let mode = app.mode();
        let pane = layout.transcripts[0];
        // Overlays take the transcript pane's cells; the rail stays so the
        // picker is read in context. The transcript caches stay warm behind
        // an overlay so closing one costs no relayout or highlight storm.
        let pane_width = pane.area.width;
        let mut body = match mode {
            Mode::Models => model_picker(app, pane_width, body_height),
            Mode::Profiles => profile_picker(app, pane_width, body_height),
            Mode::ApprovalModes => approval_mode_picker(app, pane_width, body_height),
            Mode::Skills => skill_picker(app, pane_width, body_height),
            Mode::Themes => theme_picker(app, pane_width, body_height),
            Mode::Sessions => session_picker(app, pane_width, body_height),
            Mode::Commands => command_picker(app, pane_width, body_height),
            Mode::History => history_picker(app, pane_width, body_height),
            // An approval keeps the transcript on screen and adds its block
            // under the awaiting call, so the decision is made in context.
            Mode::Approval => self.body(app, pane, body_height),
            Mode::Compose => {
                let mut body = self.body(app, pane, body_height);
                let menu = mention_autocomplete(app, pane_width, body_height);
                let menu = if menu.is_empty() {
                    slash_autocomplete(app, pane_width, body_height)
                } else {
                    menu
                };
                overlay_slash_autocomplete(&mut body, menu);
                body
            }
        };
        body = fit_height(body, body_height);
        // Side panes are blitted column-wise onto the body rows: each row is
        // padded to the pane's `x`, then the pane's cells appended. Columns
        // are built at pane width so a blit never re-measures the row.
        let mut columns: Vec<(usize, Vec<Line>)> = Vec::with_capacity(2);
        if let Some(inspector) = layout.inspector {
            columns.push((
                inspector.x,
                inspector_pane(app, inspector.width, body_height),
            ));
        }
        if let Some(rail) = layout.rail {
            columns.push((rail.x, sidebar(app, rail.width, body_height)));
        }
        for (x, column) in columns {
            for (row, cells) in body.iter_mut().zip(column) {
                pad_line(row, x);
                for span in cells.spans {
                    row.push(span.text, span.style);
                }
            }
        }
        // With side panes glued on, every body row is exactly the terminal
        // width so the border columns line up and nothing overflows.
        if layout.rail.is_some() || layout.inspector.is_some() {
            for row in &mut body {
                pad_line(row, width);
            }
        }
        lines.extend(body);
        if layout.strip
            && let Some(strip) = agent_strip(app, width)
        {
            lines.push(strip);
        }
        lines.extend(draft_lines);
        lines.push(composer_rule(app, width));
        let composer_top = lines.len();
        lines.extend(composer_lines);
        if (mode == Mode::Compose || app.approval_amendment.is_some())
            && let Some((column, row)) = caret
            && let (Ok(column), Ok(row)) = (
                u16::try_from(column.min(width.saturating_sub(1))),
                u16::try_from(composer_top + row),
            )
        {
            self.cursor = Some(CursorPosition { column, row });
        }
        fit_height(lines, height)
    }

    /// Render the main area through the transcript cache into `pane` and
    /// remember the reconciled viewport for `commit`. Rows come back at the
    /// pane's full width with the content column centered.
    fn body(&mut self, app: &App, pane: TranscriptPane, height: usize) -> Vec<Line> {
        let (mut lines, viewport) =
            self.cache
                .body(&mut self.highlighter, app, pane.content_width, height);
        self.viewport_update = Some(viewport);
        if pane.inset > 0 {
            // Cached rows are shared with later frames; the margin lives on
            // the frame's copy as a count, so centering costs no allocation.
            for line in &mut lines {
                line.indent = pane.inset;
            }
        }
        lines
    }

    /// Install a finished highlight layout if the cache still holds that
    /// message at that width. Returns whether any frame content changed;
    /// stale results for a message that was re-laid-out or evicted are
    /// dropped.
    pub(crate) fn apply_highlight(&mut self, result: Highlighted) -> bool {
        self.cache.apply_highlight(&result)
    }

    #[cfg(test)]
    fn markdown(&self) -> &HashMap<MessageId, CachedMarkdown> {
        &self.cache.markdown
    }

    #[cfg(test)]
    fn render_message(&mut self, message: &MessageSnapshot, width: usize) -> Vec<Line> {
        self.cache
            .render_message(&mut self.highlighter, message, width)
    }

    #[cfg(test)]
    fn transcript<'a>(&'a mut self, app: &App, width: usize) -> VirtualBody<'a> {
        self.cache.transcript(
            &mut self.highlighter,
            app,
            app.focused(),
            &app.viewport,
            width,
        )
    }
}

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "bench-support"))]
impl FrameRenderer {
    /// Build a frame and hand its geometry back to the app, as `draw` does.
    pub(crate) fn frame_and_commit(
        &mut self,
        app: &mut App,
        width: usize,
        height: usize,
    ) -> Vec<Line> {
        let frame = self.frame(app, width, height);
        self.commit(app);
        frame
    }
}
