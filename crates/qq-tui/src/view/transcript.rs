use super::*;
use crate::render::Span;

/// Retained rendering for the transcript: completed messages cached
/// by width, the settled prefix of streaming messages, and the row ranges
/// the live messages occupied on the last frame.
#[derive(Default)]
pub(crate) struct TranscriptCache {
    pub(super) markdown: HashMap<MessageId, CachedMarkdown>,
    /// Derived tool rows keyed by call, refreshed when a call's state or
    /// result changes so JSON parsing stays off the per-frame path.
    tool_rows: HashMap<ToolCallId, (ToolRowKey, ToolRow)>,
    /// Sum of `CachedMarkdown::bytes` over `markdown`.
    cached_bytes: usize,
    /// Monotonic counter bumped per `prepare_markdown`; stamps cache use.
    clock: u64,
    /// Settled rows of messages still streaming, keyed by message. Each entry
    /// holds the layout of the message's block-boundary-settled prefix so a
    /// frame only lays out the trailing open block.
    live: HashMap<MessageId, LiveMarkdown>,
    live_message_ranges: HashMap<MessageId, Range<usize>>,
    preserve_tail_anchor: bool,
}

pub(super) struct LiveMarkdown {
    width: usize,
    /// Bytes of the combined output+refusal text covered by `rows`.
    settled_bytes: usize,
    /// Rendered, indented rows for the settled prefix.
    rows: Vec<Line>,
}

pub(super) struct CachedMarkdown {
    pub(super) width: usize,
    output_bytes: usize,
    refusal_bytes: usize,
    loaded_through: u64,
    body: CachedMessageBody,
    /// A highlighted layout has been requested or applied; `false` means a
    /// later frame should try again once the highlighter has capacity.
    highlight_requested: bool,
    /// Frame counter at last use, for least-recently-used eviction.
    last_used: u64,
    /// Approximate heap bytes held by `body`, for the byte budget.
    bytes: usize,
}

/// Heap bytes the cache may hold in completed-message layouts. Larger than
/// any single message the cache admits so the newest message always fits.
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

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

/// Sparse row index for oversized completed messages. It stores one byte
/// checkpoint per bounded group of visual rows and reconstructs only the
/// requested viewport. Checkpoints are compacted as the message grows, so the
/// source is scanned once while retained memory stays predictable.
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
                    } => {
                        // The message can only vanish between prepare and
                        // viewport if a snapshot replaced the session inside
                        // one frame; blank rows are the safe degradation.
                        match find_message(app, *message_id) {
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
                        }
                    }
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
/// Per-frame grouping of a session's messages and tool calls by run, built
/// once per transcript layout so per-message questions are O(1).
struct TurnIndex<'a> {
    /// Index of the first assistant message of each run.
    first_assistant: HashMap<RunId, usize>,
    /// Indices of every assistant message, in transcript order, per run.
    assistant_indices: HashMap<RunId, Vec<usize>>,
    /// Tool calls per run sorted by (turn, call) ordinal.
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

    /// The turn ordinal of the next assistant message of the same run after
    /// `index`, or `u16::MAX` when `index` holds the run's last one.
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
    /// Render the main area: the session transcript or a workspace view,
    /// scrolled to the app's viewport. Every row is exactly `width` cells
    /// wide. Returns the rows and the reconciled viewport, which the caller
    /// hands back to the app after composition so rendering never writes
    /// into the model mid-frame.
    pub(super) fn body(
        &mut self,
        highlighter: &mut Highlighter,
        app: &App,
        width: usize,
        height: usize,
    ) -> (Vec<Line>, Viewport) {
        let session_id = app.view.session();
        let mut viewport = app.viewport.clone();
        // Workspace-wide views are cheap lists; they draw without the
        // transcript caches and scroll like any other body.
        let workspace_rows = match app.view {
            View::Attention => Some(attention_body(app, width)),
            View::Changes => Some(changes_body(app, width)),
            View::Transcript(_) => None,
        };
        if let Some(rows) = workspace_rows {
            let mut body = VirtualBody::default();
            body.extend_owned(rows);
            viewport.update(app.view, body.rows, height, false);
            let offset = viewport.offset();
            return (
                fit_height(body.viewport(app, height, offset), height),
                viewport,
            );
        }
        // Prose past a readable measure gets no wider: lines stay scannable
        // on a wide screen, and the width-keyed caches see one width across
        // every terminal wider than the cap.
        let content_width = width.min(MAX_TRANSCRIPT_WIDTH);
        let body = self.threadline(highlighter, app, session_id, &viewport, content_width);
        viewport.update(app.view, body.rows, height, body.preserve_tail_anchor);
        let offset = viewport.offset();
        let live_message_ranges = body.live_message_ranges.clone();
        let mut rows = body.viewport(app, height, offset);
        drop(body);
        self.live_message_ranges = live_message_ranges.into_iter().collect();
        // Scrolled up while the session runs: a pill on the last row says
        // how much has arrived below, so the user knows to jump back.
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
        (fit_height(rows, height), viewport)
    }

    /// Drop every cached layout, keeping live-row anchors: an overlay hides
    /// the transcript but a completion behind it must still preserve the
    /// user's viewport when the transcript returns.
    pub(super) fn prune_all(&mut self) {
        self.markdown.clear();
        self.cached_bytes = 0;
        self.live.clear();
        self.tool_rows.clear();
    }

    /// Keep only the layouts for messages the pane can show this frame.
    fn prune_markdown(&mut self, app: &App, session_id: Option<SessionId>) {
        let visible = session_id
            .and_then(|session_id| app.sessions.get(&session_id))
            .and_then(|session| {
                session
                    .messages
                    .as_ref()
                    .map(|messages| (session, messages))
            })
            .map(|(_, messages)| {
                messages
                    .iter()
                    .rev()
                    .take(MAX_VISIBLE_MESSAGES)
                    .map(|message| message.id)
                    .collect::<std::collections::HashSet<_>>()
            });
        match visible {
            Some(visible) => {
                self.markdown.retain(|id, cached| {
                    let keep = visible.contains(id);
                    if !keep {
                        self.cached_bytes = self.cached_bytes.saturating_sub(cached.bytes);
                    }
                    keep
                });
                self.live.retain(|id, _| visible.contains(id));
                self.live_message_ranges
                    .retain(|id, _| visible.contains(id));
            }
            None => self.prune_all(),
        }
    }

    fn prepare_markdown(
        &mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        viewport: &Viewport,
        width: usize,
    ) {
        self.clock += 1;
        self.prune_markdown(app, session_id);
        let Some(session) = session_id.and_then(|session_id| app.sessions.get(&session_id)) else {
            return;
        };
        let Some(messages) = session.messages.as_ref() else {
            return;
        };
        if let Some(calls) = session.tool_calls.as_ref() {
            self.tool_rows
                .retain(|id, _| calls.iter().any(|call| call.id == *id));
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
        } else {
            self.tool_rows.clear();
        }
        for message in messages.iter().rev().take(MAX_VISIBLE_MESSAGES) {
            if message_is_terminal(message) {
                self.live.remove(&message.id);
                if self
                    .live_message_ranges
                    .remove(&message.id)
                    .is_some_and(|range| viewport.intersects_or_follows(&range))
                {
                    self.preserve_tail_anchor = true;
                }
                self.cache_message(highlighter, message, width, session.loaded_through);
            } else {
                self.markdown.remove(&message.id);
                self.refresh_live(message, width);
            }
        }
    }

    /// Extend the settled-prefix layout of a streaming message. Only the bytes
    /// past the previous settled boundary are examined, and only blocks that
    /// became settled since the last frame are laid out.
    fn refresh_live(&mut self, message: &MessageSnapshot, width: usize) {
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(3).max(1);
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        let entry = self.live.entry(message.id).or_insert(LiveMarkdown {
            width,
            settled_bytes: 0,
            rows: Vec::new(),
        });
        if entry.width != width || entry.settled_bytes > source.len() {
            entry.width = width;
            entry.settled_bytes = 0;
            entry.rows.clear();
        }
        // The live view shows at most the last MAX_LIVE_MARKDOWN_BYTES; a
        // settled prefix beyond that is never displayed, so skip ahead rather
        // than lay out rows that would be dropped.
        let visible_start = source.len().saturating_sub(MAX_LIVE_MARKDOWN_BYTES);
        if entry.settled_bytes < visible_start {
            entry.settled_bytes = 0;
            entry.rows.clear();
        }
        let scan_from = entry.settled_bytes;
        let text = source.collect_range(scan_from..source.len(), false);
        let settled = settled_prefix_end(&text);
        if settled == 0 {
            return;
        }
        let rows = markdown_lines(&text[..settled], content_width, false);
        entry
            .rows
            .extend(indent_lines(rows, prefix, prefix_style, width));
        entry.settled_bytes = scan_from + settled;
        // Rows past the display bound are never shown again while streaming.
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
            // Layout is current; retry a highlight request that was skipped
            // because the highlighter was saturated.
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
        let content_width = width.saturating_sub(3).max(1);
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        // Plain layout first so the frame never waits on tree-sitter; the
        // highlighted layout replaces it when the blocking job finishes.
        let mut needs_highlight = false;
        let body = if source.len() <= MAX_FULL_MARKDOWN_BYTES {
            let content = source.as_cow();
            let lines = markdown_lines(&content, content_width, false);
            if lines.len() <= MAX_FULL_MARKDOWN_ROWS {
                needs_highlight = has_fenced_code(&content);
                CachedMessageBody::Markdown(indent_lines(lines, prefix, prefix_style, width))
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
        // Evict least-recently-used layouts until both the entry count and
        // the byte budget admit the new one.
        while !self.markdown.is_empty()
            && (self.markdown.len() >= MAX_VISIBLE_MESSAGES
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
        let content_width = key.width.saturating_sub(3).max(1);
        let width = key.width;
        let (prefix, prefix_style, _, _) = message_presentation(role);
        highlighter.request(key, move || {
            indent_lines(
                markdown_lines(&content, content_width, true),
                prefix,
                prefix_style,
                width,
            )
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
        viewport: &Viewport,
        width: usize,
    ) -> VirtualBody<'a> {
        let transcript = self.transcript(highlighter, app, session_id, viewport, width);
        let mut body = VirtualBody::default();
        body.extend_virtual(transcript);
        if let Some(focused) = session_id {
            // Children already shown under their spawn call are not repeated.
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
                body.push_line(Line::styled("  related sessions", muted().bold()));
                for child in children {
                    body.push_line(session_line(app, child, width, "  ↳ "));
                }
            }
        }
        body
    }

    pub(super) fn transcript<'a>(
        &'a mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        viewport: &Viewport,
        width: usize,
    ) -> VirtualBody<'a> {
        self.prepare_markdown(highlighter, app, session_id, viewport, width);
        let mut body = VirtualBody {
            preserve_tail_anchor: std::mem::take(&mut self.preserve_tail_anchor),
            ..VirtualBody::default()
        };
        let Some(session_id) = session_id else {
            body.push_line(Line::styled(
                format!(
                    "  {} creates the first session.",
                    app.chord_label(crate::commands::Command::NewRootSession)
                        .unwrap_or_else(|| "/new".to_owned())
                ),
                muted(),
            ));
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
        self.append_message_indices(
            &mut body,
            app,
            session,
            messages,
            hidden..messages.len(),
            width,
        );
        for prompt in app.pending_prompts(session_id) {
            // A pending prompt is a YOU boundary: the same two blank lines
            // that precede any user turn.
            if !body.is_empty() {
                body.push_line(Line::default());
                body.push_line(Line::default());
            }
            let mut line = Line::styled(" ▌ ", warning());
            line.push("YOU  pending", warning().bold());
            body.push_line(line);
            body.extend_owned(indent_lines(
                pending_markdown_lines(prompt, width.saturating_sub(3)),
                " ▌ ",
                warning(),
                width,
            ));
        }
        if body.is_empty() {
            body.push_line(Line::styled(
                "  Ask QQ to begin this session.",
                muted().italic(),
            ));
        }
        body
    }

    fn append_message_indices<'a>(
        &'a self,
        body: &mut VirtualBody<'a>,
        app: &App,
        session: &SessionView,
        messages: &[MessageSnapshot],
        indices: impl IntoIterator<Item = usize>,
        width: usize,
    ) {
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
                fold: app.tool_detail == ToolDetail::Folded,
                selected: app.transcript_cursor == Some(call.id),
            }
        };
        // One pass over messages and calls answers every per-message question
        // below (first assistant message of a run, next assistant turn, calls
        // by run) so the loop is linear in messages plus calls.
        let turns = TurnIndex::build(messages, tool_calls);
        for index in indices {
            let message = &messages[index];
            if !body.is_empty() {
                body.push_line(Line::default());
                // A user prompt starts a new turn; extra spacing keeps
                // prompt/response boundaries scannable.
                if message.role == MessageRole::User {
                    body.push_line(Line::default());
                }
            }
            if message.role == MessageRole::Assistant {
                // Group calls under the assistant message of their turn.
                // Calls from turns without a message of their own (call-only
                // turns, legacy turn 0 messages) attach after the nearest
                // preceding assistant message of the run; the run's first
                // rendered message also collects any earlier orphan turns.
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
                // Call-only turns before the run's first message executed
                // before its text streamed: render that head group ahead of
                // the message so execution order holds from the first block.
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
                        &|call_id, width| approval_rows(app, call_id, width),
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
                        &|call_id, width| approval_rows(app, call_id, width),
                    ));
                }
                // The run's last assistant message carries its completion line.
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
                            &|call_id, width| approval_rows(app, call_id, width),
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
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        body.push_line(truncate_line(message_header(message), width));
        let content_start = body.rows;
        if message_is_terminal(message) {
            let Some(cached) = self.markdown.get(&message.id) else {
                // `prepare_markdown` caches every visible terminal message;
                // a miss means the cache was evicted under memory pressure
                // this frame. Show the header and recover next frame.
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
            // Still streaming: the settled prefix comes from the live cache and
            // only the open trailing block is laid out this frame. Tree-sitter
            // stays off so per-frame work is bounded by one block, not the
            // message. Any hidden live prefix becomes reachable through the
            // completed-message cache once the message settles.
            let lines = self.live_lines(message, width);
            if lines.is_empty() {
                body.push_line(message_ellipsis(prefix, prefix_style));
            } else {
                body.extend_owned(lines);
            }
            body.live_message_ranges
                .push((message.id, content_start..body.rows));
        }
    }

    /// Rows for a streaming message: cached settled rows followed by the
    /// freshly laid-out open tail, bounded to the live display budget with a
    /// marker when earlier rows were dropped.
    fn live_lines(&self, message: &MessageSnapshot, width: usize) -> Vec<Line> {
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(3).max(1);
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        let (settled_bytes, settled_rows) = match self.live.get(&message.id) {
            Some(live) if live.width == width && live.settled_bytes <= source.len() => {
                (live.settled_bytes, live.rows.as_slice())
            }
            Some(_) | None => (0, &[][..]),
        };
        let visible_start = source.len().saturating_sub(MAX_LIVE_MARKDOWN_BYTES);
        let tail_start = settled_bytes.max(visible_start);
        let tail = if tail_start == settled_bytes {
            source.collect_range(tail_start..source.len(), false)
        } else {
            source.bounded_tail(MAX_LIVE_MARKDOWN_BYTES).into_owned()
        };
        let tail_rows = indent_lines(
            markdown_lines(&tail, content_width, false),
            prefix,
            prefix_style,
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
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        let mut lines = vec![truncate_line(message_header(message), width)];
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
        if lines.len() == 1 {
            lines.push(message_ellipsis(prefix, prefix_style));
        }
        lines
    }
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

pub(super) fn message_presentation(
    role: MessageRole,
) -> (&'static str, Style, &'static str, Style) {
    match role {
        MessageRole::User => (" ▌ ", accent(), "YOU", accent().bold()),
        MessageRole::Assistant => ("   ", muted(), "QQ", normal().bold()),
    }
}

pub(super) fn message_ellipsis(prefix: &'static str, prefix_style: Style) -> Line {
    let mut ellipsis = Line::styled(prefix, prefix_style);
    ellipsis.push("...", muted());
    ellipsis
}
/// Rows for a run's provider-exposed reasoning. Collapsed: one line with the
/// state and length, or the first sentence when there is room. Expanded: the
/// bounded text laid out as plain prose under a dimmed rail. Empty when the
/// run produced no reasoning.
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
    let seconds = reasoning.ticks / 8;
    let mut header = Line::styled(" ∴ ", muted());
    if reasoning.streaming {
        header.push(
            format!(
                "{} thinking… {seconds}s",
                TOOL_SPINNER[app.animation_tick % TOOL_SPINNER.len()]
            ),
            muted().italic(),
        );
    } else {
        header.push(format!("thought for {seconds}s"), muted().italic());
    }
    match app.reasoning_detail {
        crate::app::ReasoningDetail::Collapsed => {
            // First paragraph only: the collapsed row is a glance, not the text.
            let first = reasoning.text.split("\n\n").next().unwrap_or_default();
            let summary = preview(first, width.saturating_sub(header.width() + 12));
            if !summary.is_empty() {
                header.push(format!("  {summary}"), muted());
            }
            if let Some(chord) = app.chord_label(crate::commands::Command::ToggleReasoning) {
                header.push(format!("  {chord}"), muted());
            }
            vec![truncate_line(header, width)]
        }
        crate::app::ReasoningDetail::Expanded => {
            let mut rows = vec![truncate_line(header, width)];
            let content_width = width.saturating_sub(3).max(1);
            for paragraph in reasoning.text.split("\n\n") {
                for line in paragraph.lines() {
                    let safe = line
                        .chars()
                        .filter_map(terminal_safe_character)
                        .collect::<String>();
                    for wrapped in wrap_line(Line::styled(safe, muted().italic()), content_width) {
                        let mut row = Line::styled(" ┆ ", muted());
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
/// `✓ 42s · 8 tools · 12.3k tok · $0.04` under the last message of a finished
/// run. Each field appears only when known; a run still executing, or one
/// with nothing measurable, produces no row.
pub(super) fn run_completion_line(
    session: &SessionView,
    run_id: RunId,
    width: usize,
) -> Option<Line> {
    let stats = session.runs.get(&run_id)?;
    let outcome = stats.outcome.as_ref()?;
    let (glyph, style) = match outcome {
        qq_protocol::RunOutcome::Completed => ("✓", success()),
        qq_protocol::RunOutcome::Cancelled | qq_protocol::RunOutcome::Interrupted => {
            ("◌", warning())
        }
        qq_protocol::RunOutcome::BudgetExhausted { .. } => ("◌", warning()),
        qq_protocol::RunOutcome::Failed { .. } => ("✕", failure()),
    };
    let mut parts: Vec<String> = Vec::with_capacity(4);
    if let (Some(started), Some(finished)) = (stats.started_at_ms, stats.finished_at_ms) {
        parts.push(format_duration_ms(finished.saturating_sub(started)));
    }
    if stats.turns > 1 {
        parts.push(count_noun(
            usize::try_from(stats.turns).unwrap_or(usize::MAX),
            "turn",
            "turns",
        ));
    }
    if stats.tool_calls > 0 {
        parts.push(count_noun(stats.tool_calls as usize, "tool", "tools"));
    }
    if let Some(usage) = stats.usage {
        let tokens = usage.input_tokens
            + usage.cache_read_input_tokens
            + usage.cache_write_input_tokens
            + usage.output_tokens;
        if tokens > 0 {
            parts.push(format!("{} tok", format_count(tokens)));
        }
    }
    if let Some(cost) = stats.cost_usd_nanos
        && cost > 0
    {
        parts.push(format_cost(cost));
    }
    // Speed telemetry: time to first token, and output tokens per second of
    // generation (first token to finish).
    if let (Some(started), Some(first)) = (stats.started_at_ms, stats.first_token_at_ms) {
        parts.push(format!(
            "ttft {}",
            format_duration_ms(first.saturating_sub(started))
        ));
    }
    if let (Some(first), Some(finished), Some(usage)) =
        (stats.first_token_at_ms, stats.finished_at_ms, stats.usage)
        && finished > first
        && usage.output_tokens > 0
    {
        let per_second = usage.output_tokens * 1000 / (finished - first);
        if per_second > 0 {
            parts.push(format!("{per_second} tok/s"));
        }
    }
    // Provenance: which profile and plan ran, and the model route when it is
    // not the one the session selected (a profile override, or a change made
    // since). Both come from the run's own records, never inferred.
    if let Some(route) = &stats.resolved_route
        && session.summary.model.as_deref() != Some(route.as_str())
    {
        parts.push(format!("on {route}"));
    }
    if let Some((profile, digest)) = &stats.plan {
        parts.push(if profile.is_default() {
            format!("plan {digest}")
        } else {
            format!("as {} · plan {digest}", profile.as_str())
        });
    }
    if parts.is_empty() && matches!(outcome, qq_protocol::RunOutcome::Completed) {
        return None;
    }
    let mut line = Line::styled(format!("   {glyph} "), style);
    if parts.is_empty() {
        line.push(
            match outcome {
                qq_protocol::RunOutcome::Cancelled => "cancelled",
                qq_protocol::RunOutcome::Interrupted => "interrupted",
                qq_protocol::RunOutcome::BudgetExhausted { .. } => "budget exhausted",
                qq_protocol::RunOutcome::Failed { .. } => "failed",
                qq_protocol::RunOutcome::Completed => "",
            },
            muted(),
        );
    } else {
        line.push(parts.join(" · "), muted());
    }
    Some(truncate_line(line, width))
}

/// `0.4s`, `42s`, `4m12s`, `1h03m`.
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

/// `999`, `12.3k`, `1.2M`.
pub(super) fn format_count(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{}.{}k", count / 1_000, (count % 1_000) / 100)
    } else {
        format!("{}.{}M", count / 1_000_000, (count % 1_000_000) / 100_000)
    }
}

/// The inline approval block under the call the user must answer, when
/// `call_id` is that call; otherwise nothing.
fn approval_rows(app: &App, call_id: ToolCallId, width: usize) -> Vec<Line> {
    match app.pending_approval() {
        Some(call) if call.id == call_id => approval_block(app, width),
        _ => Vec::new(),
    }
}

/// The `▌ YOU  streaming` style row that opens a message. Steering rows keep
/// the user prefix but say what they are: injected mid-run, not a new prompt,
/// with a lifecycle (waiting for a boundary, applied, superseded) in words the
/// run's own messages never use.
pub(super) fn message_header(message: &MessageSnapshot) -> Line {
    let (prefix, prefix_style, role, role_style) = message_presentation(message.role);
    let mut header = Line::styled(prefix, prefix_style);
    header.push(role, role_style);
    if message.steering {
        let (label, style) = match message.state {
            MessageState::Queued => ("steering  waiting for the next turn", warning()),
            MessageState::Complete => ("steered", muted()),
            MessageState::Cancelled => ("steering  run finished first", warning()),
            MessageState::Streaming | MessageState::Failed | MessageState::Interrupted => (
                message_state_label(message.state),
                status_style(message.state),
            ),
        };
        header.push(format!("  {label}"), style);
    } else if !matches!(message.state, MessageState::Complete) {
        header.push(
            format!("  {}", message_state_label(message.state)),
            status_style(message.state),
        );
    }
    header
}

pub(super) fn message_state_label(state: MessageState) -> &'static str {
    match state {
        MessageState::Queued => "queued",
        MessageState::Streaming => "streaming",
        MessageState::Complete => "complete",
        MessageState::Cancelled => "cancelled",
        MessageState::Failed => "failed",
        MessageState::Interrupted => "interrupted",
    }
}

pub(super) fn status_style(state: MessageState) -> Style {
    match state {
        MessageState::Queued => warning(),
        MessageState::Streaming => accent(),
        MessageState::Complete => muted(),
        MessageState::Cancelled | MessageState::Interrupted => warning(),
        MessageState::Failed => failure(),
    }
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
