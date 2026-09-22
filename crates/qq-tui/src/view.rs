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
    app::{App, SessionView, ToolDetail, approval_grant_recordable, terminal_safe_character},
    input::{Mode, SessionConfirm, approval_mode_label},
    render::{
        Line, Style, accent, border, brand, diff_line_style, failure, info, muted, normal,
        selection, success, text_width, warning, write_line,
    },
    theme,
    viewport::{MAX_PANES, TranscriptPane, View, Viewport},
};
use chrome::*;
pub(crate) use chrome::{ComposerMode, CursorPosition};
use highlight::HighlightKey;
pub(crate) use highlight::{Highlighted, Highlighter};
pub(crate) use layout::LayoutPrefs;
#[cfg(any(test, feature = "bench-support"))]
pub(crate) use layout::PanePref;
use layout::{TranscriptSlot, compute_layout};
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
/// transcript layouts live in one [`TranscriptCache`] shared by every pane;
/// the highlighter is separate because its results are keyed by message and
/// width.
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
    /// Per-pane state reconciled while building the last frame, by pane
    /// index. `draw` hands it back to the app after the frame is composed;
    /// `frame` itself never mutates the model. Bounded by `MAX_PANES`.
    pane_updates: Vec<(usize, PaneUpdate)>,
    /// Where the terminal cursor belongs after the last frame, or hidden.
    cursor: Option<CursorPosition>,
}

impl FrameRenderer {
    /// Hand the pane state reconciled while building the last frame back to
    /// the model.
    pub(crate) fn commit(&mut self, app: &mut App) {
        for (index, update) in self.pane_updates.drain(..) {
            if let Some(pane) = app.panes.get_mut(index) {
                pane.viewport = update.viewport;
                pane.live_message_ranges = update.live_message_ranges;
            }
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

    /// Build one frame from the model without changing it. The pane state
    /// reconciled along the way is kept in `pane_updates` for `draw`.
    fn frame(&mut self, app: &App, width: usize, height: usize) -> Vec<Line> {
        theme::activate(app.theme().palette);
        self.pane_updates.clear();
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
        // the status and hint line so no row is spent on either. A terminal
        // tall enough spares one blank row under the composer. The composer
        // can grow with wrapped multi-line input, so it is laid out first and
        // the body takes what remains.
        let padding_rows = layout::composer_padding_rows(height);
        let fixed_rows = layout::fixed_chrome_rows(height);
        let max_composer_rows = height
            .saturating_sub(fixed_rows)
            .saturating_sub(1)
            .clamp(1, layout::max_composer_rows(width, height));
        let draft_lines = queued_drafts(app, width);
        let (composer_lines, caret) = composer(app, width, max_composer_rows);
        let chrome_rows = fixed_rows - 1 + draft_lines.len() + composer_lines.len();
        let layout = compute_layout(width, height, chrome_rows, app.layout, app.sessions.len());
        let body_height = layout.body.height;
        let mode = app.mode();
        // With the inspector on screen, expanded tool bodies and the
        // workspace views render there and the transcript keeps its rows.
        let inline_detail = layout.inspector.is_none();
        // The slots show a window of the panes that always contains the
        // focused one: with one slot that is the focused pane itself; with
        // more, focus past the right edge slides the window.
        let slots = layout.transcripts.len();
        let first_shown = app
            .focused_pane
            .saturating_sub(slots.saturating_sub(1))
            .min(app.panes.len().saturating_sub(slots));
        let shown_panes = || app.panes.iter().enumerate().skip(first_shown).take(slots);
        // The cache keeps layouts for every shown session at once, so panes
        // on different sessions never evict each other mid-frame.
        let shown: Vec<Option<SessionId>> = shown_panes()
            .map(|(_, pane)| shown_view(app, pane, inline_detail).session())
            .collect();
        self.cache.retain_visible(app, &shown);
        // Transcript panes fill their slots left to right. Overlays take the
        // focused pane's cells; the rail stays so the picker is read in
        // context. The transcript caches stay warm behind an overlay so
        // closing one costs no relayout or highlight storm.
        let mut body: Vec<Line> = Vec::new();
        let mut columns: Vec<(usize, Vec<Line>)> = Vec::with_capacity(slots + 1);
        for ((index, pane), slot) in shown_panes().zip(&layout.transcripts) {
            let slot_width = slot.area.width;
            let rows = if index == app.focused_pane {
                match mode {
                    Mode::Models => model_picker(app, slot_width, body_height),
                    Mode::Profiles => profile_picker(app, slot_width, body_height),
                    Mode::ApprovalModes => approval_mode_picker(app, slot_width, body_height),
                    Mode::Skills => skill_picker(app, slot_width, body_height),
                    Mode::Themes => theme_picker(app, slot_width, body_height),
                    Mode::Sessions => session_picker(app, slot_width, body_height),
                    Mode::Commands => command_picker(app, slot_width, body_height),
                    Mode::History => history_picker(app, slot_width, body_height),
                    // An approval keeps the transcript on screen and adds its
                    // block under the awaiting call, so the decision is made
                    // in context.
                    Mode::Approval => {
                        self.body(app, index, pane, *slot, body_height, inline_detail)
                    }
                    Mode::Compose => {
                        let mut rows =
                            self.body(app, index, pane, *slot, body_height, inline_detail);
                        let menu = mention_autocomplete(app, slot_width, body_height);
                        let menu = if menu.is_empty() {
                            slash_autocomplete(app, slot_width, body_height)
                        } else {
                            menu
                        };
                        overlay_slash_autocomplete(&mut rows, menu);
                        rows
                    }
                }
            } else {
                self.body(app, index, pane, *slot, body_height, inline_detail)
            };
            if slot.area.x == 0 {
                body = fit_height(rows, body_height);
            } else {
                columns.push((slot.area.x, fit_height(rows, body_height)));
            }
        }
        // Side panes are blitted column-wise onto the body rows: each row is
        // padded to the pane's `x`, then the pane's cells appended. Columns
        // are built at pane width so a blit never re-measures the row.
        if let Some(inspector) = layout.inspector {
            columns.push((
                inspector.x,
                inspector_pane(app, &self.cache, app.pane(), inspector.width, body_height),
            ));
        }
        if let Some(rail) = layout.rail {
            let density = RailDensity::of(layout.tier);
            columns.push((rail.x, sidebar(app, rail.width, body_height, density)));
        }
        // Each row is measured once and the running width carried through
        // the columns: measuring the row again per column would scan the
        // transcript text once per pane on every frame.
        if !columns.is_empty() {
            for (index, slot) in body.iter_mut().enumerate() {
                let extra: usize = columns
                    .iter()
                    .map(|(_, column)| column.get(index).map_or(0, |cells| cells.spans.len() + 1))
                    .sum();
                let mut used = slot.width();
                let first = std::mem::take(slot);
                let mut row = Line {
                    indent: first.indent,
                    spans: Vec::with_capacity(first.spans.len() + extra + 1),
                };
                row.spans.extend(first.spans);
                for (x, column) in &mut columns {
                    let Some(cells) = column.get_mut(index).map(std::mem::take) else {
                        continue;
                    };
                    // A blitted transcript row carries its measure inset as
                    // a count; it becomes padding here since only the first
                    // pane's rows keep the count for the renderer's cursor
                    // move.
                    let start = *x + cells.indent;
                    if used < start {
                        row.push(" ".repeat(start - used), normal());
                        used = start;
                    }
                    for span in cells.spans {
                        used += text_width(&span.text);
                        row.push(span.text, span.style);
                    }
                }
                // With side panes glued on, every body row is exactly the
                // terminal width so the border columns line up and nothing
                // overflows.
                if used < width {
                    row.push(" ".repeat(width - used), normal());
                }
                *slot = row;
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
        lines.extend(std::iter::repeat_n(Line::default(), padding_rows));
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

    /// Render one pane's main area through the transcript cache into `slot`
    /// and remember the pane's reconciled state for `commit`. Rows come back
    /// at the slot's full width with the content column placed.
    fn body(
        &mut self,
        app: &App,
        index: usize,
        pane: &TranscriptPane,
        slot: TranscriptSlot,
        height: usize,
        inline_detail: bool,
    ) -> Vec<Line> {
        let (mut lines, update) = self.cache.body(
            &mut self.highlighter,
            app,
            pane,
            slot.content_width,
            height,
            inline_detail,
        );
        self.pane_updates.push((index, update));
        if slot.inset > 0 {
            // Cached rows are shared with later frames; the margin lives on
            // the frame's copy as a count, so centering costs no allocation.
            for line in &mut lines {
                line.indent = slot.inset;
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
        let pane = app.pane();
        self.cache.retain_visible(app, &[pane.view.session()]);
        self.cache.transcript(
            &mut self.highlighter,
            app,
            pane.view.session(),
            pane,
            width,
            true,
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
