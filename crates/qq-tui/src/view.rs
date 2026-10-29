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
    compute_layout(width, layout::MIN_HEIGHT, 0, prefs, sessions).rail_visible()
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
        let mut lines = vec![top_row(app, width)];
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
            let slot_width = slot.area.width;
            let rows = if index == app.focused_pane {
                match mode {
                    Mode::GoalForm => goal_form(app, slot_width, body_height),
                    Mode::GoalCheck => goal_check_picker(app, slot_width, body_height),
                    Mode::Models => model_picker(app, slot_width, body_height),
                    Mode::Profiles => profile_picker(app, slot_width, body_height),
                    Mode::ApprovalModes => approval_mode_picker(app, slot_width, body_height),
                    Mode::Effort => effort_picker(app, slot_width, body_height),
                    Mode::Skills => skill_picker(app, slot_width, body_height),
                    Mode::Themes => theme_picker(app, slot_width, body_height),
                    Mode::Sessions => session_picker(app, slot_width, body_height),
                    Mode::Commands => command_picker(app, slot_width, body_height),
                    Mode::History => history_picker(app, slot_width, body_height),
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
