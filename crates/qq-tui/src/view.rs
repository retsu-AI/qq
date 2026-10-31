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
    input::{Mode, SessionConfirm, approval_mode_label, effort_label},
    render::{
        Line, Style, accent, border, brand, failure, faint, info, muted, normal, selection,
        success, surface, text_width, warning, write_line,
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
use layout::{Tier, TranscriptSlot, compute_layout};
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

const MAX_RENDER_WIDTH: u16 = 1024;
const MAX_RENDER_HEIGHT: u16 = 512;
const MAX_LIVE_MARKDOWN_BYTES: usize = 32 * 1024;
const MAX_VISIBLE_MESSAGES: usize = 64;
const MAX_LIVE_MARKDOWN_ROWS: usize = 160;
const MAX_FULL_MARKDOWN_BYTES: usize = 64 * 1024;
const MAX_FULL_MARKDOWN_ROWS: usize = 4 * 1024;
const PLAIN_TEXT_CHECKPOINT_ROWS: usize = 1024;
const MAX_PLAIN_TEXT_CHECKPOINTS: usize = 4 * 1024;
const MAX_PLAIN_TEXT_ROW_BYTES: usize = 4 * 1024;

pub(crate) fn render_size(actual_size: (u16, u16)) -> (usize, usize) {
    (
        usize::from(actual_size.0.clamp(1, MAX_RENDER_WIDTH)),
        usize::from(actual_size.1.clamp(1, MAX_RENDER_HEIGHT)),
    )
}

pub(crate) fn rail_visible(width: usize, prefs: LayoutPrefs, sessions: usize) -> bool {
    layout::rail_width(width, prefs, sessions) > 0
}

#[derive(Default)]
pub(crate) struct FrameRenderer {
    previous: Vec<Line>,
    size: Option<(u16, u16)>,
    pub(crate) highlighter: Highlighter,
    cache: TranscriptCache,
    theme_generation: u64,
    pane_updates: Vec<(usize, PaneUpdate)>,
    cursor: Option<CursorPosition>,
}

impl FrameRenderer {
    pub(crate) fn commit(&mut self, app: &mut App) {
        for (index, update) in self.pane_updates.drain(..) {
            if let Some(pane) = app.panes.get_mut(index) {
                pane.viewport = update.viewport;
                pane.live_message_ranges = update.live_message_ranges;
            }
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.previous.clear();
        self.size = None;
    }

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
        let rail_width = layout::rail_width(width, app.layout, app.sessions.len());
        let pane_width = width - rail_width;
        let dock_width = pane_width.saturating_sub(2 * layout::MARGIN).max(1);
        let max_composer_rows = layout::max_composer_rows(width, height)
            .min(height.saturating_sub(4))
            .max(1);
        let (composer_lines, caret) = composer(app, dock_width, max_composer_rows);
        let fixed_rows = 2 + composer_lines.len();
        let draft_budget = (height / 4).min(height.saturating_sub(fixed_rows + 1));
        let draft_lines = queued_drafts(app, dock_width, draft_budget);
        let mode = app.mode();
        let panel_limit = height.saturating_sub(fixed_rows + draft_lines.len()) / 2;
        let panel = dock_panel(app, dock_width, panel_limit.max(1));
        let mut dock: Vec<Line> = Vec::new();
        dock.push(dock_rule(app, dock_width, rail_width > 0));
        dock.extend(panel);
        dock.extend(draft_lines);
        let composer_top = dock.len();
        dock.extend(composer_lines);
        dock.push(dock_footer(
            app,
            dock_width,
            rail_width == 0 && Tier::of(width) == Tier::Compact,
        ));
        let layout = compute_layout(width, height, dock.len(), app.layout, app.sessions.len());
        let body_height = layout.body.height;
        let inline_detail = layout.inspector.is_none();
        let slots = layout.transcripts.len();
        let first_shown = app
            .focused_pane
            .saturating_sub(slots.saturating_sub(1))
            .min(app.panes.len().saturating_sub(slots));
        let shown_panes = || app.panes.iter().enumerate().skip(first_shown).take(slots);
        let shown: Vec<Option<SessionId>> = shown_panes()
            .map(|(_, pane)| shown_view(app, pane, inline_detail).session())
            .collect();
        self.cache.retain_visible(app, &shown);
        let mut body: Vec<Line> = Vec::new();
        let mut columns: Vec<(usize, Vec<Line>)> = Vec::with_capacity(slots + 1);
        for ((index, pane), slot) in shown_panes().zip(&layout.transcripts) {
            let slot_width = slot.content_width;
            let mut rows = self.body(app, index, pane, *slot, body_height, inline_detail);
            if index == app.focused_pane && mode == Mode::Compose {
                let menu = mention_autocomplete(app, slot_width, body_height);
                let menu = if menu.is_empty() {
                    slash_autocomplete(app, slot_width, body_height)
                } else {
                    menu
                };
                overlay_slash_autocomplete(&mut rows, menu);
            }
            if slot.area.x == 0 {
                body = fit_height(rows, body_height);
            } else {
                columns.push((slot.area.x, fit_height(rows, body_height)));
            }
        }
        if let Some(inspector) = layout.inspector {
            columns.push((
                inspector.x,
                inspector_pane(app, &self.cache, app.pane(), inspector.width, body_height),
            ));
        }
        for line in &mut dock {
            line.indent = layout::MARGIN;
        }
        body.extend(dock);
        if let Some(rail) = layout.rail {
            columns.push((rail.x, sidebar(app, rail.width, height, layout.tier)));
        }
        if !columns.is_empty() {
            for (index, slot) in body.iter_mut().enumerate() {
                let mut used = slot.width();
                let first = std::mem::take(slot);
                let mut row = Line {
                    indent: first.indent,
                    spans: first.spans,
                };
                for (x, column) in &mut columns {
                    let Some(cells) = column.get_mut(index).map(std::mem::take) else {
                        continue;
                    };
                    if used > *x {
                        let indent = row.indent;
                        row = truncate_line(std::mem::take(&mut row), x.saturating_sub(indent));
                        row.indent = indent;
                        used = row.width();
                    }
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
                if used < width {
                    row.push(" ".repeat(width - used), normal());
                }
                *slot = row;
            }
        }
        let composer_top = body_height + composer_top;
        if (mode == Mode::Compose || app.approval_amendment.is_some() || answering_question(app))
            && let Some((column, row)) = caret
            && let (Ok(column), Ok(row)) = (
                u16::try_from((layout::MARGIN + column).min(width.saturating_sub(1))),
                u16::try_from(composer_top + row),
            )
        {
            self.cursor = Some(CursorPosition { column, row });
        }
        fit_height(body, height)
    }

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
            for line in &mut lines {
                line.indent = slot.inset;
            }
        }
        lines
    }

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
