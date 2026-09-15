//! Frame assembly: composes chrome, transcript, and overlays into the lines
//! the renderer diffs against the previous frame.

mod chrome;
mod highlight;
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

/// Rows the composer may grow to before it scrolls around the caret.
const MAX_COMPOSER_ROWS: usize = 8;
const MAX_RENDER_WIDTH: u16 = 320;
const MAX_RENDER_HEIGHT: u16 = 160;
const MAX_LIVE_MARKDOWN_BYTES: usize = 32 * 1024;
const MAX_VISIBLE_MESSAGES: usize = 64;
/// Widest the transcript body lays out; the rest of a wider terminal is
/// left blank rather than stretching prose past a readable line length.
const MAX_TRANSCRIPT_WIDTH: usize = 120;
const MAX_LIVE_MARKDOWN_ROWS: usize = MAX_RENDER_HEIGHT as usize;
/// Completed messages at or below these bounds retain full markdown styling.
/// Larger messages use a sparse plain-text row index so scrolling stays
/// complete without caching every rendered row.
const MAX_FULL_MARKDOWN_BYTES: usize = 64 * 1024;
const MAX_FULL_MARKDOWN_ROWS: usize = 4 * 1024;
const PLAIN_TEXT_CHECKPOINT_ROWS: usize = 1024;
const MAX_PLAIN_TEXT_CHECKPOINTS: usize = 4 * 1024;
const MAX_PLAIN_TEXT_ROW_BYTES: usize = 4 * 1024;

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
        let width = actual_size.0.clamp(1, MAX_RENDER_WIDTH);
        let height = actual_size.1.clamp(1, MAX_RENDER_HEIGHT);
        let frame = self.frame(app, usize::from(width), usize::from(height));
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
        if width < 32 || height < 9 {
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
        // can grow with wrapped multi-line input, so body height is computed
        // after the composer is laid out against the remaining space.
        let fixed_chrome_rows = 2;
        let max_composer_rows = height
            .saturating_sub(fixed_chrome_rows)
            .saturating_sub(1)
            .clamp(1, MAX_COMPOSER_ROWS);
        let mut draft_lines = queued_drafts(app, width);
        let sidebar_width = app.sidebar.width(width, app.sessions.len());
        if sidebar_width == 0
            && let Some(strip) = agent_strip(app, width)
        {
            draft_lines.insert(0, strip);
        }
        let (composer_lines, caret) = composer(app, width, max_composer_rows);
        let body_height = height
            .saturating_sub(fixed_chrome_rows)
            .saturating_sub(draft_lines.len())
            .saturating_sub(composer_lines.len());
        // The sidebar takes a column on the right; the body renders in what
        // remains so its cache keys see one stable width per terminal size.
        let body_width = width.saturating_sub(sidebar_width);
        let mode = app.mode();
        let mut body = match mode {
            // Overlays hide the transcript; its caches stay warm so closing
            // one costs no relayout or highlight storm. Memory stays bounded
            // by the per-pane byte budget, not by pruning here.
            Mode::Models => model_picker(app, body_width, body_height),
            Mode::Profiles => profile_picker(app, body_width, body_height),
            Mode::ApprovalModes => approval_mode_picker(app, body_width, body_height),
            Mode::Skills => skill_picker(app, body_width, body_height),
            Mode::Themes => theme_picker(app, body_width, body_height),
            Mode::Sessions => session_picker(app, body_width, body_height),
            Mode::Commands => command_picker(app, body_width, body_height),
            Mode::History => history_picker(app, body_width, body_height),
            // An approval keeps the transcript on screen and adds its block
            // under the awaiting call, so the decision is made in context.
            Mode::Approval => self.body(app, body_width, body_height),
            Mode::Compose => {
                let mut body = self.body(app, body_width, body_height);
                let menu = mention_autocomplete(app, body_width, body_height);
                let menu = if menu.is_empty() {
                    slash_autocomplete(app, body_width, body_height)
                } else {
                    menu
                };
                overlay_slash_autocomplete(&mut body, menu);
                body
            }
        };
        if sidebar_width > 0 {
            let sidebar = sidebar(app, sidebar_width, body_height);
            body = fit_height(body, body_height);
            for (row, column) in body.iter_mut().zip(sidebar) {
                pad_line(row, body_width);
                for span in column.spans {
                    row.push(span.text, span.style);
                }
                pad_line(row, width);
            }
        }
        lines.extend(body);
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

    /// Render the main area through the transcript cache and remember the
    /// reconciled viewport for `commit`.
    fn body(&mut self, app: &App, width: usize, height: usize) -> Vec<Line> {
        let (lines, viewport) = self.cache.body(&mut self.highlighter, app, width, height);
        self.viewport_update = Some(viewport);
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

#[cfg(test)]
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
