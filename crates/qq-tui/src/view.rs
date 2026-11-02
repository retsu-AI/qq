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
    app::{App, SessionView, terminal_safe_character},
    input::{Mode, SessionConfirm, approval_mode_label, effort_label},
    render::{
        Line, Style, accent, border, brand, failure, faint, info, muted, normal, selection,
        success, surface, text_width, warning, write_line,
    },
    theme,
    viewport::{Block, MAX_PANES, TranscriptPane, View, Viewport},
};
use chrome::*;
pub(crate) use chrome::{ComposerMode, CursorPosition};
use highlight::HighlightKey;
pub(crate) use highlight::{Highlighted, Highlighter};
pub(crate) use layout::LayoutPrefs;
pub(crate) use layout::PanePref;
use layout::{Tier, TranscriptSlot, compute_layout};
use markdown::{has_fenced_code, markdown_lines, settled_prefix_end};
use overlay::*;
use sidebar::*;
use tools::*;
pub(crate) use tools::{
    EXPANDED_WINDOW_ROWS, expanded_panel_match, expanded_panel_rows, expanded_window_start,
    tool_copy_text,
};
use transcript::*;
use workspace::*;
pub(crate) use wrap::preview;
#[cfg(test)]
use wrap::transcript_viewport;
use wrap::{
    bounded_head, bounded_tail, fit_height, indent_lines, selection_viewport, truncate_line,
    wrap_line, wrap_line_chars,
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

pub(crate) fn code_blocks(source: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for event in pulldown_cmark::Parser::new_ext(source, pulldown_cmark::Options::all()) {
        match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(_)) => {
                current = Some(String::new());
            }
            pulldown_cmark::Event::Text(text) => {
                if let Some(code) = current.as_mut() {
                    code.push_str(&text);
                }
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
                if let Some(mut code) = current.take() {
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    blocks.push(code);
                }
            }
            _ => {}
        }
    }
    blocks
}

pub(crate) fn rail_visible(width: usize, prefs: LayoutPrefs, sessions: usize) -> bool {
    layout::rail_width(width, prefs, sessions) > 0
}

fn inspector_content(app: &App) -> bool {
    match app.pane().view {
        View::Transcript(Some(session_id)) => app
            .sessions
            .get(&session_id)
            .and_then(|session| session.tool_calls.as_deref())
            .is_some_and(|calls| {
                calls
                    .iter()
                    .any(|call| app.expanded_tool_calls.contains(&call.id))
            }),
        View::Transcript(None)
        | View::Attention
        | View::Changes
        | View::Goal(_)
        | View::SideQuestions(_) => false,
    }
}

#[derive(Default)]
pub(crate) struct FrameRenderer {
    previous: Vec<Line>,
    size: Option<(u16, u16)>,
    pub(crate) highlighter: Highlighter,
    cache: TranscriptCache,
    theme_generation: u64,
    pane_updates: Vec<(usize, PaneUpdate)>,
    goal_scroll: Option<usize>,
    cursor: Option<CursorPosition>,
    transcript_area: Option<crate::selection::Area>,
    rail_area: Option<crate::selection::Area>,
    rail_rows: Vec<(usize, SessionId)>,
    inspector_shown: bool,
    tool_window: usize,
    shapes: Vec<crate::selection::RowShape>,
    shapes_ready: bool,
}

impl FrameRenderer {
    pub(crate) fn commit(&mut self, app: &mut App) {
        if let Some(scroll) = self.goal_scroll.take() {
            app.goal_scroll = scroll;
        }
        app.geometry.transcript = self.transcript_area;
        app.geometry.rail = self.rail_area;
        app.geometry.rail_rows.clone_from(&self.rail_rows);
        app.inspector_shown = self.inspector_shown;
        app.tool_window = self.tool_window.max(1);
        for (index, update) in self.pane_updates.drain(..) {
            if let Some(pane) = app.panes.get_mut(index) {
                pane.viewport = update.viewport;
                pane.reveal = None;
                pane.relayout = false;
                pane.live_message_ranges = update.live_message_ranges;
                let previous = std::mem::replace(&mut pane.blocks, update.blocks);
                self.cache.recycle_blocks(previous);
            }
        }
        if std::mem::take(&mut app.find_stale) {
            app.refresh_find();
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.previous.clear();
        self.size = None;
    }

    pub fn draw(&mut self, app: &mut App, actual_size: (u16, u16)) -> io::Result<Vec<u8>> {
        let (width, height) = render_size(actual_size);
        let mut frame = self.frame(app, width, height);
        self.commit(app);
        self.paint_selection(app, &mut frame);
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

    fn paint_selection(&mut self, app: &mut App, frame: &mut [Line]) {
        let Some(selection) = app.selection.as_mut() else {
            return;
        };
        self.shapes = Self::shapes_of(frame, self.transcript_area);
        self.shapes_ready = true;
        if selection.still_shows(frame, &self.shapes, self.transcript_area) {
            selection.paint(frame, &self.shapes);
        } else {
            app.selection = None;
        }
    }

    pub(crate) fn selection_text(
        &self,
        selection: &crate::selection::Selection,
    ) -> crate::selection::Copied {
        if self.shapes_ready {
            return selection.text(&self.previous, &self.shapes);
        }
        selection.text(
            &self.previous,
            &Self::shapes_of(&self.previous, self.transcript_area),
        )
    }

    fn shapes_of(
        frame: &[Line],
        area: Option<crate::selection::Area>,
    ) -> Vec<crate::selection::RowShape> {
        let rows = area.map_or(0, |area| area.y + area.height);
        frame
            .iter()
            .take(rows)
            .map(crate::selection::RowShape::of)
            .collect()
    }

    fn frame(&mut self, app: &App, width: usize, height: usize) -> Vec<Line> {
        theme::activate(app.theme().palette);
        self.pane_updates.clear();
        self.transcript_area = None;
        self.rail_area = None;
        self.rail_rows.clear();
        self.inspector_shown = false;
        self.shapes_ready = false;
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
                    Line::styled("Resize to at least 32 x 9. Ctrl-C twice exits.", muted()),
                ],
                height,
            );
        }

        self.cursor = None;
        let rail_width = layout::rail_width(width, app.layout, app.sessions.listed_count());
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
        let (panel, goal_scroll) = dock_panel(app, dock_width, panel_limit.max(1));
        self.goal_scroll = goal_scroll;
        let mut dock: Vec<Line> = Vec::new();
        dock.push(Line::default());
        dock.extend(panel);
        dock.extend(draft_lines);
        let composer_top = dock.len();
        dock.extend(composer_lines);
        dock.push(dock_footer(
            app,
            dock_width,
            rail_width == 0 && Tier::of(width) == Tier::Compact,
        ));
        let demand = layout::LayoutDemand {
            sessions: app.sessions.listed_count(),
            inspector_content: inspector_content(app),
        };
        let mut layout = compute_layout(width, height, dock.len(), app.layout, demand);
        let tool_window = if layout.inspector.is_some() {
            inspector_window(layout.body.height)
        } else {
            EXPANDED_WINDOW_ROWS
        };
        let mut inspector_rows = layout.inspector.map(|inspector| {
            inspector_rows(app, &self.cache, app.pane(), inspector.width, tool_window)
        });
        if app.layout.inspector == layout::PanePref::Auto
            && inspector_rows
                .as_ref()
                .is_some_and(|rows| rows.len() + 1 > layout.body.height)
        {
            layout = compute_layout(
                width,
                height,
                dock.len(),
                app.layout,
                layout::LayoutDemand {
                    inspector_content: false,
                    ..demand
                },
            );
            inspector_rows = None;
        }
        let body_height = layout.body.height;
        self.inspector_shown = layout.inspector.is_some();
        self.tool_window = if self.inspector_shown {
            tool_window
        } else {
            EXPANDED_WINDOW_ROWS
        };
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
            if index == app.focused_pane
                && shown_view(app, pane, inline_detail).transcript().is_some()
            {
                self.transcript_area = Some(crate::selection::Area {
                    x: slot.area.x,
                    y: 0,
                    width: slot.area.width,
                    height: body_height,
                });
                self.shapes_ready = false;
            }
            if slot.area.x == 0 {
                body = fit_height(rows, body_height);
            } else {
                columns.push((slot.area.x, fit_height(rows, body_height)));
            }
        }
        if let (Some(inspector), Some(rows)) = (layout.inspector, inspector_rows) {
            columns.push((
                inspector.x,
                inspector_pane(rows, inspector.width, body_height),
            ));
        }
        let unseen = self
            .pane_updates
            .iter()
            .find(|(index, _)| *index == app.focused_pane)
            .map_or(0, |(_, update)| update.viewport.unseen());
        dock[0] = dock_rule(app, dock_width, rail_width > 0, unseen);
        for line in &mut dock {
            line.indent = layout::MARGIN as u16;
        }
        body.extend(dock);
        if let Some(rail) = layout.rail {
            let (rows, hits) = sidebar(app, rail.width, height, layout.tier);
            self.rail_area = Some(crate::selection::Area {
                x: rail.x,
                y: 0,
                width: rail.width,
                height,
            });
            self.rail_rows = hits;
            columns.push((rail.x, rows));
        }
        if !columns.is_empty() {
            for (index, slot) in body.iter_mut().enumerate() {
                let mut used = slot.width();
                let first_width = used;
                let first_pad = slot.pad;
                let first = std::mem::take(slot);
                let mut row = Line {
                    indent: first.indent,
                    chrome: first.chrome,
                    pad: first.pad,
                    kind: first.kind,
                    join: first.join,
                    spans: first.spans,
                };
                for (x, column) in &mut columns {
                    let Some(cells) = column.get_mut(index).map(std::mem::take) else {
                        continue;
                    };
                    if used > *x {
                        let indent = row.indent;
                        row = truncate_line(
                            std::mem::take(&mut row),
                            x.saturating_sub(usize::from(indent)),
                        );
                        row.indent = indent;
                        used = row.width();
                    }
                    let start = *x + usize::from(cells.indent);
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
                    used = width;
                }
                let added = used.saturating_sub(first_width);
                row.pad = first_pad.saturating_add(u16::try_from(added).unwrap_or(u16::MAX));
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
        if index == app.focused_pane
            && let Some(cursor @ (Block::Message(_) | Block::Code(..))) = app.block_cursor
            && let Some((_, range)) = update.blocks.iter().find(|(block, _)| *block == cursor)
        {
            let window = update.viewport.window();
            let first = range.start.max(window.start) - window.start;
            let last = range.end.min(window.end).saturating_sub(window.start);
            for line in lines.iter_mut().take(last).skip(first) {
                for span in &mut line.spans {
                    span.style = selection(span.style);
                }
            }
        }
        self.pane_updates.push((index, update));
        if slot.inset > 0 {
            for line in &mut lines {
                line.indent = u16::try_from(slot.inset).unwrap_or(u16::MAX);
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

#[cfg(test)]
pub(crate) fn message_rows_for_test(source: &str, role: MessageRole, width: usize) -> Vec<Line> {
    transcript::message_rows(
        markdown_lines(source, width.saturating_sub(2), false),
        role,
        width,
    )
}

#[cfg(test)]
pub(crate) fn tool_row_for_test(output: &str, width: usize) -> Vec<Line> {
    let mut rows = vec![Line::styled("● ", faint()).with_chrome(2)];
    rows[0].push("Ran  cargo test", normal());
    rows[0].kind = crate::render::RowKind::Tool;
    rows.extend(
        output
            .lines()
            .map(|line| tools::tail_row(line, muted(), width)),
    );
    rows
}

#[cfg(test)]
pub(crate) fn wrap_for_test(line: Line, width: usize) -> Vec<Line> {
    wrap_line(line, width)
}

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
