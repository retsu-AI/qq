use super::*;
use crate::render::Span;

#[derive(Default)]
pub(crate) struct TranscriptCache {
    pub(super) markdown: HashMap<MessageId, CachedMarkdown>,
    tool_rows: HashMap<ToolCallId, (ToolRowKey, ToolRow)>,
    cached_bytes: usize,
    clock: u64,
    live: HashMap<MessageId, LiveMarkdown>,
}

pub(crate) struct PaneUpdate {
    pub viewport: Viewport,
    pub live_message_ranges: HashMap<MessageId, Range<usize>>,
}

pub(super) struct LiveMarkdown {
    width: usize,
    settled_bytes: usize,
    rows: Vec<Line>,
    tail: String,
    tail_source_len: usize,
}

pub(super) struct CachedMarkdown {
    pub(super) width: usize,
    output_bytes: usize,
    refusal_bytes: usize,
    loaded_through: u64,
    body: CachedMessageBody,
    highlight_requested: bool,
    last_used: u64,
    bytes: usize,
}

const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_CACHED_MESSAGES: usize = MAX_VISIBLE_MESSAGES * MAX_PANES;

impl CachedMessageBody {
    fn bytes(&self) -> usize {
        match self {
            Self::Markdown(lines) => lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.text.len() + std::mem::size_of::<Span>())
                        .sum::<usize>()
                        + std::mem::size_of::<Line>()
                })
                .sum(),
            Self::Plain(index) => {
                index.checkpoints.len() * std::mem::size_of::<PlainTextCheckpoint>()
            }
        }
    }
}

impl CachedMarkdown {
    fn key(&self, message_id: MessageId) -> HighlightKey {
        HighlightKey {
            message_id,
            width: self.width,
            output_bytes: self.output_bytes,
            refusal_bytes: self.refusal_bytes,
            loaded_through: self.loaded_through,
        }
    }
}

pub(super) enum CachedMessageBody {
    Markdown(Vec<Line>),
    Plain(PlainTextIndex),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PlainTextCheckpoint {
    row: usize,
    byte: usize,
}

#[derive(Clone, Copy)]
pub(super) struct MessageText<'a> {
    output: &'a str,
    refusal: &'a str,
}

impl<'a> MessageText<'a> {
    const SEPARATOR: &'static str = "\n\n";

    pub(super) fn new(message: &'a MessageSnapshot) -> Self {
        Self {
            output: &message.output,
            refusal: &message.refusal,
        }
    }

    const fn has_separator(self) -> bool {
        !self.output.is_empty() && !self.refusal.is_empty()
    }

    fn len(self) -> usize {
        self.output.len()
            + self.refusal.len()
            + usize::from(self.has_separator()) * Self::SEPARATOR.len()
    }

    fn as_cow(self) -> Cow<'a, str> {
        if self.refusal.is_empty() {
            return Cow::Borrowed(self.output);
        }
        if self.output.is_empty() {
            return Cow::Borrowed(self.refusal);
        }
        Cow::Owned(format!(
            "{}{}{}",
            self.output,
            Self::SEPARATOR,
            self.refusal
        ))
    }

    fn next_char(self, byte: usize) -> Option<(char, usize)> {
        if byte < self.output.len() {
            let character = self.output[byte..].chars().next()?;
            return Some((character, byte + character.len_utf8()));
        }
        let refusal_start =
            self.output.len() + usize::from(self.has_separator()) * Self::SEPARATOR.len();
        if byte < refusal_start {
            return Some(('\n', byte + 1));
        }
        if byte < self.len() {
            let local = byte - refusal_start;
            let character = self.refusal[local..].chars().next()?;
            return Some((character, byte + character.len_utf8()));
        }
        None
    }

    fn is_char_boundary(self, byte: usize) -> bool {
        if byte <= self.output.len() {
            return self.output.is_char_boundary(byte);
        }
        let refusal_start =
            self.output.len() + usize::from(self.has_separator()) * Self::SEPARATOR.len();
        if byte <= refusal_start {
            return true;
        }
        let local = byte - refusal_start;
        local <= self.refusal.len() && self.refusal.is_char_boundary(local)
    }

    fn collect_range(self, range: Range<usize>, sanitize: bool) -> String {
        let mut collected = String::with_capacity(range.len());
        let mut byte = range.start.min(self.len());
        let end = range.end.min(self.len());
        while byte < end {
            let Some((character, next)) = self.next_char(byte) else {
                break;
            };
            if !sanitize {
                collected.push(character);
            } else if let Some(character) = terminal_safe_character(character) {
                collected.push(character);
            }
            byte = next;
        }
        collected
    }

    fn bounded_tail(self, max_bytes: usize) -> Cow<'a, str> {
        if self.len() <= max_bytes {
            return self.as_cow();
        }
        let mut start = self.len() - max_bytes;
        while !self.is_char_boundary(start) {
            start += 1;
        }
        Cow::Owned(self.collect_range(start..self.len(), false))
    }
}

pub(super) struct PlainTextIndex {
    content_width: usize,
    rows: usize,
    pub(super) checkpoints: Vec<PlainTextCheckpoint>,
}

impl PlainTextIndex {
    pub(super) fn new(source: MessageText<'_>, content_width: usize) -> Self {
        let content_width = content_width.max(1);
        let mut checkpoints = vec![PlainTextCheckpoint { row: 0, byte: 0 }];
        let mut checkpoint_rows = PLAIN_TEXT_CHECKPOINT_ROWS;
        let mut rows = 0;
        let mut byte = 0;
        while let Some((_, next)) = next_plain_text_row(source, byte, content_width) {
            rows += 1;
            byte = next;
            if checkpoints.len() >= MAX_PLAIN_TEXT_CHECKPOINTS {
                checkpoint_rows = checkpoint_rows.saturating_mul(2);
                checkpoints.retain(|checkpoint| checkpoint.row % checkpoint_rows == 0);
            }
            if rows % checkpoint_rows == 0 && byte < source.len() {
                checkpoints.push(PlainTextCheckpoint { row: rows, byte });
            }
        }
        Self {
            content_width,
            rows,
            checkpoints,
        }
    }

    pub(super) fn render(
        &self,
        source: MessageText<'_>,
        rows: Range<usize>,
        prefix: &'static str,
        prefix_style: Style,
        width: usize,
    ) -> Vec<Line> {
        let rows = rows.start.min(self.rows)..rows.end.min(self.rows);
        if rows.is_empty() {
            return Vec::new();
        }
        let checkpoint = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.row <= rows.start)
            .saturating_sub(1);
        let checkpoint = self.checkpoints[checkpoint];
        let mut current_row = checkpoint.row;
        let mut byte = checkpoint.byte;
        let mut rendered = Vec::with_capacity(rows.len());
        while let Some((range, next)) = next_plain_text_row(source, byte, self.content_width) {
            if current_row >= rows.end {
                break;
            }
            if current_row >= rows.start {
                let safe = source.collect_range(range, true);
                let mut line = Line::styled(prefix, prefix_style);
                line.push(safe, normal());
                rendered.push(truncate_line(line, width));
            }
            current_row += 1;
            byte = next;
        }
        rendered
    }
}

pub(super) enum BodySegment<'a> {
    Owned(Vec<Line>),
    Cached(&'a [Line]),
    Plain {
        index: &'a PlainTextIndex,
        message_id: MessageId,
        prefix: &'static str,
        prefix_style: Style,
        width: usize,
    },
}

impl BodySegment<'_> {
    fn rows(&self) -> usize {
        match self {
            Self::Owned(lines) => lines.len(),
            Self::Cached(lines) => lines.len(),
            Self::Plain { index, .. } => index.rows,
        }
    }
}

#[derive(Default)]
pub(super) struct VirtualBody<'a> {
    pub(super) segments: Vec<BodySegment<'a>>,
    pub(super) rows: usize,
    preserve_tail_anchor: bool,
    live_message_ranges: Vec<(MessageId, Range<usize>)>,
}

impl<'a> VirtualBody<'a> {
    fn is_empty(&self) -> bool {
        self.rows == 0
    }

    fn push_line(&mut self, line: Line) {
        self.extend_owned(vec![line]);
    }

    fn extend_owned(&mut self, mut lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        self.rows += lines.len();
        if let Some(BodySegment::Owned(current)) = self.segments.last_mut() {
            current.append(&mut lines);
        } else {
            self.segments.push(BodySegment::Owned(lines));
        }
    }

    fn extend_cached(&mut self, lines: &'a [Line]) {
        if lines.is_empty() {
            return;
        }
        self.rows += lines.len();
        self.segments.push(BodySegment::Cached(lines));
    }

    fn extend_plain(
        &mut self,
        index: &'a PlainTextIndex,
        message_id: MessageId,
        prefix: &'static str,
        prefix_style: Style,
        width: usize,
    ) {
        if index.rows == 0 {
            return;
        }
        self.rows += index.rows;
        self.segments.push(BodySegment::Plain {
            index,
            message_id,
            prefix,
            prefix_style,
            width,
        });
    }

    fn extend_virtual(&mut self, mut other: VirtualBody<'a>) {
        let row_offset = self.rows;
        for (_, range) in &mut other.live_message_ranges {
            range.start += row_offset;
            range.end += row_offset;
        }
        self.rows += other.rows;
        self.preserve_tail_anchor |= other.preserve_tail_anchor;
        self.live_message_ranges
            .append(&mut other.live_message_ranges);
        self.segments.append(&mut other.segments);
    }

    #[inline(never)]
    fn pad_top(&mut self) {
        for (_, range) in &mut self.live_message_ranges {
            range.start += 1;
            range.end += 1;
        }
        self.rows += 1;
        self.segments
            .insert(0, BodySegment::Owned(vec![Line::default()]));
    }

    pub(super) fn viewport(&self, app: &App, height: usize, offset: usize) -> Vec<Line> {
        let offset = offset.min(self.rows.saturating_sub(height));
        let end = self.rows.saturating_sub(offset);
        let start = end.saturating_sub(height);
        let mut rendered = Vec::with_capacity(height.min(self.rows));
        let mut segment_start = 0;
        for segment in &self.segments {
            let segment_end = segment_start + segment.rows();
            let local_start = start.saturating_sub(segment_start).min(segment.rows());
            let local_end = end.saturating_sub(segment_start).min(segment.rows());
            if local_start < local_end {
                match segment {
                    BodySegment::Owned(lines) => {
                        rendered.extend_from_slice(&lines[local_start..local_end]);
                    }
                    BodySegment::Cached(lines) => {
                        rendered.extend_from_slice(&lines[local_start..local_end]);
                    }
                    BodySegment::Plain {
                        index,
                        message_id,
                        prefix,
                        prefix_style,
                        width,
                    } => match find_message(app, *message_id) {
                        Some(message) => rendered.extend(index.render(
                            MessageText::new(message),
                            local_start..local_end,
                            prefix,
                            *prefix_style,
                            *width,
                        )),
                        None => rendered.extend(std::iter::repeat_n(
                            Line::default(),
                            local_end - local_start,
                        )),
                    },
                }
            }
            if segment_end >= end {
                break;
            }
            segment_start = segment_end;
        }
        fit_height(rendered, height)
    }
}
struct TurnIndex<'a> {
    first_assistant: HashMap<RunId, usize>,
    assistant_indices: HashMap<RunId, Vec<usize>>,
    calls: HashMap<RunId, Vec<&'a ToolCallSnapshot>>,
}

impl<'a> TurnIndex<'a> {
    fn build(messages: &[MessageSnapshot], tool_calls: &'a [ToolCallSnapshot]) -> Self {
        let mut first_assistant = HashMap::new();
        let mut assistant_indices: HashMap<RunId, Vec<usize>> = HashMap::new();
        for (index, message) in messages.iter().enumerate() {
            if message.role == MessageRole::Assistant {
                first_assistant.entry(message.run_id).or_insert(index);
                assistant_indices
                    .entry(message.run_id)
                    .or_default()
                    .push(index);
            }
        }
        let mut calls: HashMap<RunId, Vec<&ToolCallSnapshot>> = HashMap::new();
        for call in tool_calls {
            calls.entry(call.run_id).or_default().push(call);
        }
        for run_calls in calls.values_mut() {
            run_calls.sort_by_key(|call| (call.turn_ordinal, call.call_ordinal));
        }
        Self {
            first_assistant,
            assistant_indices,
            calls,
        }
    }

    fn first_assistant(&self, run_id: RunId) -> Option<usize> {
        self.first_assistant.get(&run_id).copied()
    }

    fn next_assistant_turn(&self, messages: &[MessageSnapshot], index: usize) -> u32 {
        let run_id = messages[index].run_id;
        self.assistant_indices
            .get(&run_id)
            .and_then(|indices| {
                let position = indices.iter().position(|candidate| *candidate == index)?;
                indices.get(position + 1)
            })
            .map_or(u32::MAX, |next| messages[*next].turn_ordinal)
    }

    fn calls_of(&self, run_id: RunId) -> &[&'a ToolCallSnapshot] {
        self.calls.get(&run_id).map_or(&[], Vec::as_slice)
    }
}

impl TranscriptCache {
    pub(super) fn body(
        &mut self,
        highlighter: &mut Highlighter,
        app: &App,
        pane: &TranscriptPane,
        width: usize,
        height: usize,
        inline_detail: bool,
    ) -> (Vec<Line>, PaneUpdate) {
        let view = shown_view(app, pane, inline_detail);
        let session_id = view.session();
        let mut viewport = pane.viewport.clone();
        let workspace_rows = match view {
            View::Attention => Some(attention_body(app, width)),
            View::Changes => Some(changes_body(app, width)),
            View::SideQuestions(session_id) => Some(side_questions_body(app, session_id, width)),
            View::Goal(_) | View::Transcript(_) => None,
        };
        if let Some(rows) = workspace_rows {
            let mut body = VirtualBody::default();
            body.extend_owned(rows);
            viewport.update(view, body.rows, height, false);
            let offset = viewport.offset();
            return (
                fit_height(body.viewport(app, height, offset), height),
                PaneUpdate {
                    viewport,
                    live_message_ranges: HashMap::new(),
                },
            );
        }
        let content_width = width;
        let mut body = self.threadline(
            highlighter,
            app,
            session_id,
            pane,
            content_width,
            inline_detail,
        );
        body.pad_top();
        viewport.update(view, body.rows, height, body.preserve_tail_anchor);
        let offset = viewport.offset();
        let live_message_ranges = body.live_message_ranges.iter().cloned().collect();
        let mut rows = body.viewport(app, height, offset);
        drop(body);
        let running = session_id
            .and_then(|id| app.sessions.get(&id))
            .is_some_and(|session| session.summary.active_run_id.is_some());
        if offset > 0
            && running
            && let Some(last) = rows.last_mut()
        {
            let pill = format!(" ↓ {} more ", count_noun(offset, "row", "rows"));
            let pill_width = pill.chars().count();
            let mut row = std::mem::take(last);
            pad_line(&mut row, width);
            let mut clipped = truncate_line(row, width.saturating_sub(pill_width + 1));
            pad_line(&mut clipped, width.saturating_sub(pill_width));
            clipped.push(pill, accent().bold());
            *last = clipped;
        }
        (
            fit_height(rows, height),
            PaneUpdate {
                viewport,
                live_message_ranges,
            },
        )
    }

    pub(super) fn tool_row(&self, call: &ToolCallSnapshot) -> Option<&ToolRow> {
        self.tool_rows
            .get(&call.id)
            .filter(|(key, _)| *key == ToolRowKey::of(call))
            .map(|(_, row)| row)
    }

    pub(super) fn prune_all(&mut self) {
        self.markdown.clear();
        self.cached_bytes = 0;
        self.live.clear();
        self.tool_rows.clear();
    }

    pub(super) fn retain_visible(&mut self, app: &App, shown: &[Option<SessionId>]) {
        let mut visible = std::collections::HashSet::new();
        let mut any_warm = false;
        let warm = || {
            shown
                .iter()
                .filter_map(|session_id| app.sessions.get(session_id.as_ref()?))
        };
        for session in warm() {
            let Some(messages) = session.messages.as_ref() else {
                continue;
            };
            any_warm = true;
            visible.extend(
                messages
                    .iter()
                    .rev()
                    .take(MAX_VISIBLE_MESSAGES)
                    .map(|message| message.id),
            );
        }
        if !any_warm {
            self.prune_all();
            return;
        }
        self.markdown.retain(|id, cached| {
            let keep = visible.contains(id);
            if !keep {
                self.cached_bytes = self.cached_bytes.saturating_sub(cached.bytes);
            }
            keep
        });
        self.live.retain(|id, _| visible.contains(id));
        self.tool_rows.retain(|id, _| {
            warm()
                .filter_map(|session| session.tool_calls.as_deref())
                .any(|calls| calls.iter().any(|call| call.id == *id))
        });
    }

    fn prepare_markdown(
        &mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        pane: &TranscriptPane,
        width: usize,
    ) -> bool {
        self.clock += 1;
        let Some(session) = session_id.and_then(|session_id| app.sessions.get(&session_id)) else {
            return false;
        };
        let Some(messages) = session.messages.as_ref() else {
            return false;
        };
        if let Some(calls) = session.tool_calls.as_ref() {
            for call in calls {
                let key = ToolRowKey::of(call);
                let stale = self
                    .tool_rows
                    .get(&call.id)
                    .is_none_or(|(cached, _)| *cached != key);
                if stale {
                    self.tool_rows.insert(call.id, (key, ToolRow::derive(call)));
                }
            }
        }
        let mut preserve_tail_anchor = false;
        for message in messages.iter().rev().take(MAX_VISIBLE_MESSAGES) {
            if message_is_terminal(message) {
                self.live.remove(&message.id);
                if pane
                    .live_message_ranges
                    .get(&message.id)
                    .is_some_and(|range| pane.viewport.intersects_or_follows(range))
                {
                    preserve_tail_anchor = true;
                }
                self.cache_message(highlighter, message, width, session.loaded_through);
            } else {
                self.markdown.remove(&message.id);
                self.refresh_live(message, width);
            }
        }
        preserve_tail_anchor
    }

    fn refresh_live(&mut self, message: &MessageSnapshot, width: usize) {
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(LANE).max(1);
        let entry = self.live.entry(message.id).or_insert(LiveMarkdown {
            width,
            settled_bytes: 0,
            rows: Vec::new(),
            tail: String::new(),
            tail_source_len: 0,
        });
        if entry.width != width || entry.settled_bytes > source.len() {
            entry.width = width;
            entry.settled_bytes = 0;
            entry.rows.clear();
        }
        let visible_start = source.len().saturating_sub(MAX_LIVE_MARKDOWN_BYTES);
        if entry.settled_bytes < visible_start {
            entry.settled_bytes = 0;
            entry.rows.clear();
        }
        let scan_from = entry.settled_bytes;
        let mut text = source.collect_range(scan_from..source.len(), false);
        let settled = settled_prefix_end(&text);
        if settled > 0 {
            let rows = markdown_lines(&text[..settled], content_width, false);
            entry.rows.extend(message_rows(rows, message.role, width));
            entry.settled_bytes = scan_from + settled;
            text.drain(..settled);
        }
        entry.tail = text;
        entry.tail_source_len = source.len();
        if settled == 0 {
            return;
        }
        let excess = entry.rows.len().saturating_sub(MAX_LIVE_MARKDOWN_ROWS);
        if excess > 0 {
            entry.rows.drain(..excess);
        }
    }

    fn cache_message(
        &mut self,
        highlighter: &mut Highlighter,
        message: &MessageSnapshot,
        width: usize,
        loaded_through: u64,
    ) {
        if let Some(cached) = self.markdown.get_mut(&message.id)
            && cached.width == width
            && cached.output_bytes == message.output.len()
            && cached.refusal_bytes == message.refusal.len()
            && cached.loaded_through == loaded_through
        {
            cached.last_used = self.clock;
            if !cached.highlight_requested {
                let key = cached.key(message.id);
                cached.highlight_requested = Self::request_highlight(
                    highlighter,
                    key,
                    MessageText::new(message),
                    message.role,
                );
            }
            return;
        }
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(LANE).max(1);
        let mut needs_highlight = false;
        let body = if source.len() <= MAX_FULL_MARKDOWN_BYTES {
            let content = source.as_cow();
            let lines = markdown_lines(&content, content_width, false);
            if lines.len() <= MAX_FULL_MARKDOWN_ROWS {
                needs_highlight = has_fenced_code(&content);
                CachedMessageBody::Markdown(message_rows(lines, message.role, width))
            } else {
                CachedMessageBody::Plain(PlainTextIndex::new(source, content_width))
            }
        } else {
            CachedMessageBody::Plain(PlainTextIndex::new(source, content_width))
        };
        let bytes = body.bytes();
        if let Some(previous) = self.markdown.remove(&message.id) {
            self.cached_bytes = self.cached_bytes.saturating_sub(previous.bytes);
        }
        while !self.markdown.is_empty()
            && (self.markdown.len() >= MAX_CACHED_MESSAGES
                || self.cached_bytes + bytes > MAX_CACHE_BYTES)
        {
            let Some(stale) = self
                .markdown
                .iter()
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(id, _)| *id)
            else {
                break;
            };
            if let Some(removed) = self.markdown.remove(&stale) {
                self.cached_bytes = self.cached_bytes.saturating_sub(removed.bytes);
            }
        }
        self.cached_bytes += bytes;
        let cached = CachedMarkdown {
            width,
            output_bytes: message.output.len(),
            refusal_bytes: message.refusal.len(),
            loaded_through,
            body,
            highlight_requested: !needs_highlight,
            last_used: self.clock,
            bytes,
        };
        let key = cached.key(message.id);
        let mut cached = cached;
        if needs_highlight {
            cached.highlight_requested =
                Self::request_highlight(highlighter, key, source, message.role);
        }
        self.markdown.insert(message.id, cached);
    }

    fn request_highlight(
        highlighter: &mut Highlighter,
        key: HighlightKey,
        source: MessageText<'_>,
        role: MessageRole,
    ) -> bool {
        let content = source.as_cow().into_owned();
        let content_width = key.width.saturating_sub(LANE).max(1);
        let width = key.width;
        highlighter.request(key, move || {
            message_rows(markdown_lines(&content, content_width, true), role, width)
        })
    }

    pub(super) fn apply_highlight(&mut self, result: &Highlighted) -> bool {
        let Some(cached) = self.markdown.get_mut(&result.key.message_id) else {
            return false;
        };
        if cached.key(result.key.message_id) != result.key {
            return false;
        }
        match &mut cached.body {
            CachedMessageBody::Markdown(lines) => {
                lines.clone_from(&result.lines);
                true
            }
            CachedMessageBody::Plain(_) => false,
        }
    }

    fn threadline<'a>(
        &'a mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        pane: &TranscriptPane,
        width: usize,
        inline_detail: bool,
    ) -> VirtualBody<'a> {
        let transcript = self.transcript(highlighter, app, session_id, pane, width, inline_detail);
        let mut body = VirtualBody::default();
        body.extend_virtual(transcript);
        if let Some(focused) = session_id {
            let children: Vec<SessionId> = app
                .sessions
                .children_of(focused)
                .iter()
                .copied()
                .filter(|child| {
                    app.sessions[child]
                        .summary
                        .spawned_by
                        .and_then(|origin| origin.tool_call_id)
                        .is_none_or(|call| app.sessions.child_spawned_by(call) != Some(*child))
                })
                .collect();
            if !children.is_empty() {
                body.push_line(Line::default());
                body.extend_owned(agent_lanes(app, &children, width));
            }
        }
        body
    }

    pub(super) fn transcript<'a>(
        &'a mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        pane: &TranscriptPane,
        width: usize,
        inline_detail: bool,
    ) -> VirtualBody<'a> {
        let preserve_tail_anchor = self.prepare_markdown(highlighter, app, session_id, pane, width);
        let mut body = VirtualBody {
            preserve_tail_anchor,
            ..VirtualBody::default()
        };
        let Some(session_id) = session_id else {
            if let Some(remedy) = app.configured_provider_remedy() {
                body.push_line(Line::styled(format!("  {}", remedy.message()), warning()));
            }
            body.push_line(Line::styled(
                format!(
                    "  {} creates the first session.",
                    app.chord_label(crate::commands::Command::NewRootSession)
                        .unwrap_or_else(|| "/new".to_owned())
                ),
                muted(),
            ));
            push_environment(&mut body, app, width);
            return body;
        };
        let Some(session) = app.sessions.get(&session_id) else {
            body.push_line(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return body;
        };
        let Some(messages) = session.messages.as_ref() else {
            body.push_line(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return body;
        };
        let hidden = messages.len().saturating_sub(MAX_VISIBLE_MESSAGES);
        if hidden > 0 {
            body.push_line(Line::styled(
                format!("  {hidden} earlier messages outside the viewport"),
                muted(),
            ));
        }
        self.append_messages(&mut body, app, session, messages, width, inline_detail);
        for prompt in app.pending_prompts(session_id) {
            if !body.is_empty() {
                body.push_line(Line::default());
                body.push_line(Line::default());
            }
            let mut line = Line::styled(" ▌ ", warning());
            line.push("YOU  pending", warning().bold());
            body.push_line(line);
            body.extend_owned(indent_lines(
                pending_markdown_lines(prompt, width.saturating_sub(LANE)),
                " ▌ ",
                warning(),
                width,
            ));
        }
        if body.is_empty() {
            let first_session = app.sessions.len() == 1 && session.prompt_history.is_empty();
            if first_session {
                if let Some(remedy) = app.configured_provider_remedy() {
                    body.push_line(Line::styled(format!("  {}", remedy.message()), warning()));
                }
                body.extend_owned(banner(app, width));
                push_environment(&mut body, app, width);
            } else {
                body.push_line(Line::styled("  Ask QQ to begin this session.", faint()));
            }
        }
        body
    }

    fn append_messages<'a>(
        &'a self,
        body: &mut VirtualBody<'a>,
        app: &App,
        session: &SessionView,
        messages: &[MessageSnapshot],
        width: usize,
        inline_detail: bool,
    ) {
        let indices = messages.len().saturating_sub(MAX_VISIBLE_MESSAGES)..messages.len();
        let tool_calls = session.tool_calls.as_deref().unwrap_or_default();
        let fallback_rows: HashMap<ToolCallId, ToolRow> = tool_calls
            .iter()
            .filter(|call| !self.tool_rows.contains_key(&call.id))
            .map(|call| (call.id, ToolRow::derive(call)))
            .collect();
        let lookup = |call: &ToolCallSnapshot| -> ToolRowContext<'_> {
            let row = self
                .tool_rows
                .get(&call.id)
                .map(|(_, row)| row)
                .or_else(|| fallback_rows.get(&call.id))
                .expect("every call has a derived row");
            ToolRowContext {
                row,
                clock: RowClock {
                    timing: session
                        .tool_timing
                        .get(&call.id)
                        .copied()
                        .unwrap_or_default(),
                    now_ms: app.now_ms,
                },
                expanded: app.expanded_tool_calls.contains(&call.id),
                inline_detail,
                fold: app.tool_detail == ToolDetail::Folded,
                selected: app.transcript_cursor == Some(call.id),
            }
        };
        let turns = TurnIndex::build(messages, tool_calls);
        for index in indices {
            let message = &messages[index];
            if !body.is_empty() && !(message.steering && message.role == MessageRole::User) {
                body.push_line(Line::default());
            }
            if message.role == MessageRole::Assistant {
                let first_of_run = turns.first_assistant(message.run_id) == Some(index);
                let next_turn = turns.next_assistant_turn(messages, index);
                let run_calls: Vec<&ToolCallSnapshot> = turns
                    .calls_of(message.run_id)
                    .iter()
                    .filter(|tool_call| {
                        tool_call.turn_ordinal < next_turn
                            && (first_of_run || tool_call.turn_ordinal >= message.turn_ordinal)
                    })
                    .copied()
                    .collect();
                let head = if first_of_run {
                    run_calls
                        .iter()
                        .take_while(|tool_call| tool_call.turn_ordinal < message.turn_ordinal)
                        .count()
                } else {
                    0
                };
                if head > 0 {
                    body.extend_owned(render_tool_calls(
                        &run_calls[..head],
                        &session.live_tool_output,
                        &lookup,
                        app.animation_tick,
                        width,
                        &|call_id, width| child_rows(app, call_id, width),
                    ));
                    body.push_line(Line::default());
                }
                if first_of_run {
                    let rows = reasoning_rows(app, message.session_id, message.run_id, width);
                    if !rows.is_empty() {
                        body.extend_owned(rows);
                        body.push_line(Line::default());
                    }
                }
                self.append_message(body, message, width);
                if run_calls.len() > head {
                    body.push_line(Line::default());
                    body.extend_owned(render_tool_calls(
                        &run_calls[head..],
                        &session.live_tool_output,
                        &lookup,
                        app.animation_tick,
                        width,
                        &|call_id, width| child_rows(app, call_id, width),
                    ));
                }
                if next_turn == u32::MAX
                    && let Some(line) = run_completion_line(session, message.run_id, width)
                {
                    body.push_line(line);
                }
            } else {
                self.append_message(body, message, width);
                if turns.first_assistant(message.run_id).is_none() {
                    let orphan_calls = turns.calls_of(message.run_id);
                    if !orphan_calls.is_empty() {
                        body.push_line(Line::default());
                        body.extend_owned(render_tool_calls(
                            orphan_calls,
                            &session.live_tool_output,
                            &lookup,
                            app.animation_tick,
                            width,
                            &|call_id, width| child_rows(app, call_id, width),
                        ));
                    }
                }
            }
        }
    }

    fn append_message<'a>(
        &'a self,
        body: &mut VirtualBody<'a>,
        message: &MessageSnapshot,
        width: usize,
    ) {
        let (prefix, prefix_style) = message_presentation(message.role);
        let content_start = body.rows;
        if message_is_terminal(message) {
            let Some(cached) = self.markdown.get(&message.id) else {
                body.push_line(message_ellipsis(prefix, prefix_style));
                return;
            };
            match &cached.body {
                CachedMessageBody::Markdown(lines) => {
                    if lines.is_empty() {
                        body.push_line(message_ellipsis(prefix, prefix_style));
                    } else {
                        body.extend_cached(lines);
                    }
                }
                CachedMessageBody::Plain(index) => {
                    if index.rows == 0 {
                        body.push_line(message_ellipsis(prefix, prefix_style));
                    } else {
                        body.extend_plain(index, message.id, prefix, prefix_style, width);
                    }
                }
            }
        } else {
            let mut lines = self.live_lines(message, width);
            if lines.is_empty() {
                lines.push(message_ellipsis(prefix, prefix_style));
            } else if message.state == MessageState::Streaming
                && let Some(last) = lines.last_mut()
            {
                last.push("▍", accent());
            }
            body.extend_owned(lines);
            body.live_message_ranges
                .push((message.id, content_start..body.rows));
        }
        if let Some(note) = message_note(message) {
            body.push_line(truncate_line(note, width));
        }
    }

    fn live_lines(&self, message: &MessageSnapshot, width: usize) -> Vec<Line> {
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(LANE).max(1);
        let (settled_bytes, settled_rows, cached_tail) = match self.live.get(&message.id) {
            Some(live) if live.width == width && live.settled_bytes <= source.len() => (
                live.settled_bytes,
                live.rows.as_slice(),
                (live.tail_source_len == source.len()).then_some(live.tail.as_str()),
            ),
            Some(_) | None => (0, &[][..], None),
        };
        let visible_start = source.len().saturating_sub(MAX_LIVE_MARKDOWN_BYTES);
        let tail_start = settled_bytes.max(visible_start);
        let tail = if tail_start == settled_bytes {
            match cached_tail {
                Some(tail) => std::borrow::Cow::Borrowed(tail),
                None => {
                    std::borrow::Cow::Owned(source.collect_range(tail_start..source.len(), false))
                }
            }
        } else {
            source.bounded_tail(MAX_LIVE_MARKDOWN_BYTES)
        };
        let tail_rows = message_rows(
            markdown_lines(&tail, content_width, false),
            message.role,
            width,
        );
        let total = settled_rows.len() + tail_rows.len();
        let truncated = tail_start > settled_bytes || total > MAX_LIVE_MARKDOWN_ROWS;
        let budget = MAX_LIVE_MARKDOWN_ROWS.saturating_sub(usize::from(truncated));
        let mut lines = Vec::with_capacity(total.min(budget) + 1);
        if truncated {
            lines.push(truncate_line(
                Line::styled(
                    "... earlier output remains available when this message completes",
                    muted().italic(),
                ),
                width,
            ));
        }
        let drop = total.saturating_sub(budget);
        let drop_settled = drop.min(settled_rows.len());
        lines.extend_from_slice(&settled_rows[drop_settled..]);
        lines.extend(tail_rows.into_iter().skip(drop - drop_settled));
        lines
    }

    #[cfg(test)]
    #[cfg(test)]
    pub(super) fn render_message(
        &mut self,
        highlighter: &mut Highlighter,
        message: &MessageSnapshot,
        width: usize,
    ) -> Vec<Line> {
        if message_is_terminal(message) {
            self.live.remove(&message.id);
            self.cache_message(highlighter, message, width, 0);
        } else {
            self.refresh_live(message, width);
        }
        let (prefix, prefix_style) = message_presentation(message.role);
        let mut lines = Vec::new();
        if message_is_terminal(message) {
            match &self.markdown.get(&message.id).expect("message cached").body {
                CachedMessageBody::Markdown(body) => lines.extend_from_slice(body),
                CachedMessageBody::Plain(index) => lines.extend(index.render(
                    MessageText::new(message),
                    0..index.rows,
                    prefix,
                    prefix_style,
                    width,
                )),
            }
        } else {
            lines.extend(self.live_lines(message, width));
        }
        if lines.is_empty() {
            lines.push(message_ellipsis(prefix, prefix_style));
        }
        lines.extend(message_note(message));
        lines
    }
}

fn push_environment(body: &mut VirtualBody<'_>, app: &App, width: usize) {
    if app.environment.is_empty() {
        return;
    }
    body.push_line(Line::default());
    for line in &app.environment {
        let rows = super::wrap::wrap_line(
            Line::styled(line.clone(), muted()),
            width.saturating_sub(2).max(1),
        );
        for row in rows {
            let mut indented = Line::styled("  ", muted());
            for span in row.spans {
                indented.push(span.text, span.style);
            }
            body.push_line(indented);
        }
    }
}

pub(super) fn shown_view(app: &App, pane: &TranscriptPane, inline_detail: bool) -> View {
    match pane.view {
        View::Transcript(Some(session_id)) if !app.sessions.contains_key(&session_id) => {
            View::Transcript(None)
        }
        View::Goal(session_id) if !app.sessions.contains_key(&session_id) => View::Transcript(None),
        View::Goal(session_id) => View::Transcript(Some(session_id)),
        View::Attention | View::Changes if !inline_detail => View::Transcript(
            app.view_return()
                .filter(|session_id| app.sessions.contains_key(session_id)),
        ),
        view => view,
    }
}

pub(super) fn banner(app: &App, width: usize) -> Vec<Line> {
    let mut title = Line::styled(
        format!("  qq {}", env!("CARGO_PKG_VERSION")),
        normal().bold(),
    );
    if !app.workspace_path.is_empty() {
        title.push(format!(" · {}", app.workspace_path), muted());
    }
    if let Some(model) = app.model.model.as_deref() {
        let short = model.split_once('/').map_or(model, |(_, name)| name);
        title.push(format!(" · {short}"), muted());
    }
    let mut hints = Line::styled("  ", normal());
    let switch = app
        .chord_label(crate::commands::Command::OpenSessions)
        .map_or_else(|| "/sessions".to_owned(), |chord| compact_chord(&chord));
    for (index, (key, label)) in [
        ("/models".to_owned(), "choose a model"),
        ("@path".to_owned(), "mention a file"),
        (switch, "switch session"),
        ("?".to_owned(), "help"),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            hints.push("    ", normal());
        }
        hints.push(key, normal());
        hints.push(format!("  {label}"), faint());
    }
    vec![
        truncate_line(title, width),
        Line::default(),
        truncate_line(hints, width),
    ]
}

const FOLDED_LANES: usize = 6;

pub(super) fn agent_lanes(app: &App, children: &[SessionId], width: usize) -> Vec<Line> {
    let urgency = |id: &SessionId| match app.sessions[id].group() {
        qq_client::state::Group::NeedsYou => 0,
        qq_client::state::Group::Working => 1,
        qq_client::state::Group::Done => 2,
        qq_client::state::Group::Idle => 3,
    };
    let mut tally = [0_usize; 4];
    for child in children {
        let index = match rail_glyph(app, *child).0 {
            "◐" => 0,
            "◇" => 1,
            "●" => 2,
            "✕" => 3,
            _ => continue,
        };
        tally[index] += 1;
    }
    let mut header = Line::styled("● ", faint());
    header.push(format!("{:<9}", "Spawned"), normal());
    header.push(count_noun(children.len(), "agent", "agents"), muted());
    let mut first = true;
    for ((glyph, style), count) in [
        ("◐", info()),
        ("◇", warning()),
        ("●", faint()),
        ("✕", failure()),
    ]
    .into_iter()
    .zip(tally)
    {
        if count > 0 {
            header.push(if first { "   " } else { "  " }, normal());
            header.push(format!("{glyph} {count}"), style);
            first = false;
        }
    }
    let mut rows = vec![truncate_line(header, width)];
    let mut shown: Vec<SessionId> = children.to_vec();
    let folded = shown.len() > FOLDED_LANES;
    if folded {
        shown.sort_by_key(urgency);
        shown.truncate(FOLDED_LANES);
    }
    let title_width = shown
        .iter()
        .map(|id| text_width(&app.sessions[id].summary.title))
        .max()
        .unwrap_or(0);
    for (index, child) in shown.iter().enumerate() {
        let view = &app.sessions[child];
        let connector = if index + 1 == shown.len() && !folded {
            "└ "
        } else {
            "├ "
        };
        let (glyph, style) = rail_glyph(app, *child);
        let mut line = Line::styled(connector, faint());
        line.push(format!("{glyph} "), style);
        line.push(format!("{:<title_width$}   ", view.summary.title), normal());
        if let Some((activity, _)) = live_status_line(app, *child) {
            let room = width.saturating_sub(line.width());
            line.push(fit_text(&activity, room), muted());
        }
        rows.push(truncate_line(line, width));
    }
    if folded {
        let mut line = Line::styled("└ ", faint());
        line.push(format!("+{} more", children.len() - FOLDED_LANES), muted());
        if let Some(chord) = app.chord_label(crate::commands::Command::OpenSessions) {
            line.push(format!("   {}", compact_chord(&chord)), faint());
        }
        rows.push(truncate_line(line, width));
    }
    rows
}

pub(super) fn find_message(app: &App, message_id: MessageId) -> Option<&MessageSnapshot> {
    app.sessions
        .values()
        .filter_map(|session| session.messages.as_ref())
        .flatten()
        .find(|message| message.id == message_id)
}

pub(super) const fn message_is_terminal(message: &MessageSnapshot) -> bool {
    matches!(
        message.state,
        MessageState::Complete
            | MessageState::Cancelled
            | MessageState::Failed
            | MessageState::Interrupted
    )
}

pub(super) const LANE: usize = 2;

pub(super) fn message_rows(lines: Vec<Line>, role: MessageRole, width: usize) -> Vec<Line> {
    let (prefix, prefix_style) = message_presentation(role);
    let mut rows = indent_lines(lines, prefix, prefix_style, width);
    if role == MessageRole::User {
        for row in &mut rows {
            for span in row.spans.iter_mut().skip(1) {
                span.style = span.style.bold();
            }
        }
    }
    rows
}

pub(super) fn message_presentation(role: MessageRole) -> (&'static str, Style) {
    match role {
        MessageRole::User => ("┃ ", accent()),
        MessageRole::Assistant => ("  ", normal()),
    }
}

pub(super) fn message_ellipsis(prefix: &'static str, prefix_style: Style) -> Line {
    let mut ellipsis = Line::styled(prefix, prefix_style);
    ellipsis.push("▍", accent());
    ellipsis
}
pub(super) fn reasoning_rows(
    app: &App,
    session_id: SessionId,
    run_id: RunId,
    width: usize,
) -> Vec<Line> {
    let Some(reasoning) = app
        .sessions
        .get(&session_id)
        .and_then(|session| session.reasoning.get(&run_id))
    else {
        return Vec::new();
    };
    if reasoning.text.is_empty() && !reasoning.streaming {
        return Vec::new();
    }
    if reasoning.streaming {
        return Vec::new();
    }
    let seconds = reasoning.ticks / 8;
    let mut header = Line::styled("∴ ", faint());
    header.push(format!("thought for {seconds}s"), faint());
    match app.reasoning_detail {
        crate::app::ReasoningDetail::Collapsed => vec![truncate_line(header, width)],
        crate::app::ReasoningDetail::Expanded => {
            let mut rows = vec![truncate_line(header, width)];
            let content_width = width.saturating_sub(LANE).max(1);
            for paragraph in reasoning.text.split("\n\n") {
                for line in paragraph.lines() {
                    let safe = line
                        .chars()
                        .filter_map(terminal_safe_character)
                        .collect::<String>();
                    for wrapped in wrap_line(Line::styled(safe, muted()), content_width) {
                        let mut row = Line::styled("│ ", faint());
                        for span in wrapped.spans {
                            row.push(span.text, span.style);
                        }
                        rows.push(row);
                    }
                }
            }
            rows
        }
    }
}
pub(super) fn run_completion_line(
    session: &SessionView,
    run_id: RunId,
    width: usize,
) -> Option<Line> {
    let stats = session.runs.get(&run_id)?;
    let (glyph, style, label) = match stats.outcome.as_ref()? {
        qq_protocol::RunOutcome::Completed => return None,
        qq_protocol::RunOutcome::Cancelled => ("◌", warning(), "cancelled"),
        qq_protocol::RunOutcome::Interrupted => ("◌", warning(), "interrupted"),
        qq_protocol::RunOutcome::BudgetExhausted { .. } => ("◌", warning(), "budget exhausted"),
        qq_protocol::RunOutcome::Paused { .. } => ("◌", warning(), "paused"),
        qq_protocol::RunOutcome::Failed { .. } => ("✕", failure(), "failed"),
    };
    let mut line = Line::styled(format!("{glyph} "), style);
    line.push(label, normal());
    if let (Some(started), Some(finished)) = (stats.started_at_ms, stats.finished_at_ms)
        && finished.saturating_sub(started) >= 1000
    {
        line.push(
            format!("  {}", format_duration_ms(finished - started)),
            faint(),
        );
    }
    Some(truncate_line(line, width))
}

pub(super) fn format_duration_ms(ms: u64) -> String {
    if ms < 10_000 {
        return format!("{}.{}s", ms / 1000, (ms % 1000) / 100);
    }
    let seconds = ms / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m{:02}s", seconds % 60);
    }
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

pub(super) fn message_note(message: &MessageSnapshot) -> Option<Line> {
    let (glyph, label, style) = if message.steering {
        match message.state {
            MessageState::Queued => ("", "waiting for the next turn", faint()),
            MessageState::Complete => ("", "steered", faint()),
            MessageState::Cancelled => ("", "not sent · the run finished first", faint()),
            MessageState::Streaming => return None,
            MessageState::Failed => ("✕ ", "failed", failure()),
            MessageState::Interrupted => ("", "interrupted", warning()),
        }
    } else {
        match message.state {
            MessageState::Complete | MessageState::Streaming => return None,
            MessageState::Queued => ("", "queued", faint()),
            MessageState::Cancelled => ("", "cancelled", warning()),
            MessageState::Interrupted => ("", "interrupted", warning()),
            MessageState::Failed => ("✕ ", "failed", failure()),
        }
    };
    let mut line = Line::styled("  ", normal());
    line.push(glyph, style);
    line.push(label, style);
    Some(line)
}

pub(super) fn pending_markdown_lines(source: &str, width: usize) -> Vec<Line> {
    let source_was_truncated = source.len() > MAX_LIVE_MARKDOWN_BYTES;
    let mut lines = markdown_lines(bounded_tail(source, MAX_LIVE_MARKDOWN_BYTES), width, false);
    let reserved_marker = usize::from(source_was_truncated || lines.len() > MAX_LIVE_MARKDOWN_ROWS);
    let excess = lines
        .len()
        .saturating_sub(MAX_LIVE_MARKDOWN_ROWS.saturating_sub(reserved_marker));
    if excess > 0 {
        lines.drain(..excess);
    }
    if reserved_marker > 0 {
        lines.insert(
            0,
            truncate_line(
                Line::styled("... earlier pending prompt omitted", muted().italic()),
                width,
            ),
        );
    }
    lines
}

pub(super) fn next_plain_text_row(
    source: MessageText<'_>,
    start: usize,
    width: usize,
) -> Option<(Range<usize>, usize)> {
    if start >= source.len() {
        return None;
    }
    let width = width.max(1);
    let mut used = 0_usize;
    let mut byte = start;
    while let Some((character, next)) = source.next_char(byte) {
        if byte > start && next.saturating_sub(start) > MAX_PLAIN_TEXT_ROW_BYTES {
            return Some((start..byte, byte));
        }
        if character == '\n' {
            return Some((start..byte, next));
        }
        let character_width = terminal_safe_character(character)
            .and_then(UnicodeWidthChar::width)
            .unwrap_or_default();
        if used > 0 && used.saturating_add(character_width) > width {
            return Some((start..byte, byte));
        }
        used = used.saturating_add(character_width);
        byte = next;
    }
    Some((start..source.len(), source.len()))
}
