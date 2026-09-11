//! Client-side session state reduced from server events, shared by every
//! surface: the per-session view with its warm body, live status, run
//! statistics, and reasoning; the store that indexes the tree they form; and
//! the reducer that applies `SessionEvent`s and snapshots to it.
//!
//! The reducer is pure with respect to the surface: it takes the store, the
//! event, and a [`ReduceContext`] describing what the surface is showing, and
//! returns [`StateEffect`]s for everything that is the surface's business
//! (notices, attention, refocus, snapshot requests, draft submission). It
//! never touches the network, the clock, or rendering state.

use std::{
    cell::OnceCell,
    collections::{BTreeSet, HashMap, VecDeque, hash_map},
};

use qq_protocol::{
    AgentProfileId, EditPreview, MessageId, MessageRole, MessageSnapshot, MessageState,
    ModelDescriptor, RunActivity, RunId, RunOutcome, RunPlanIdentity, SessionId, SessionSnapshot,
    SessionStatus, SessionSummary, ShellCommandPreview, SnapshotRequest, TokenUsage, ToolCallId,
    ToolCallSnapshot, ToolCallState, WorkspaceId,
};

mod reduce;
#[cfg(test)]
mod tests;

pub use reduce::{Attention, NoticeLevel, ReduceContext, StateEffect, StateEffects};

/// Warm transcript bodies kept loaded at once. The focused session is always
/// warm; the rest are the most recently focused sessions so switching back
/// costs no round trip.
pub const WARM_BODY_LIMIT: usize = 8;
/// Bytes of assistant text retained per session for the live status tail.
pub const LIVE_TAIL_BYTES: usize = 256;
/// Bytes of reasoning text retained per run.
pub const MAX_REASONING_BYTES: usize = 16 * 1024;
/// Bytes of live tool output retained per running call.
pub const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
/// Prompts remembered per session for history browsing.
pub const MAX_PROMPT_HISTORY: usize = 100;
/// Drafts that may wait per session while it runs.
pub const MAX_QUEUED_DRAFTS: usize = 8;
/// Sessions a snapshot request asks for.
pub const SNAPSHOT_SESSION_LIMIT: u16 = 512;
/// Messages retained per warm body and requested per snapshot.
pub const SNAPSHOT_MESSAGE_LIMIT: u16 = 256;
/// Tool calls retained per warm body.
pub const MAX_RECENT_TOOL_CALLS: usize = 64;

/// Maps one character of streamed text to what the surface can show, or
/// `None` to drop it. Surfaces supply this because what is safe differs: a
/// terminal must drop control and bidi override characters, a DOM text node
/// needs only the controls removed.
pub type TextSanitizer = fn(char) -> Option<char>;

/// Drops control characters (whitespace controls become a space) and the
/// Unicode bidi override and isolate controls. Safe for terminals and text
/// nodes alike; the default sanitizer.
#[must_use]
pub fn plain_text_character(character: char) -> Option<char> {
    if character.is_control() {
        return character.is_whitespace().then_some(' ');
    }
    if matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    ) {
        return None;
    }
    Some(character)
}

/// A model the surface knows about: what the catalog said plus the selection
/// that picks it. The reducer uses the catalog to derive each session's
/// context window from its selected model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider: String,
    pub model: String,
    pub name: Option<String>,
    pub context_window: Option<u32>,
    pub selection: qq_protocol::ModelSelection,
}

impl From<ModelDescriptor> for ModelOption {
    fn from(descriptor: ModelDescriptor) -> Self {
        Self {
            provider: descriptor.provider,
            model: descriptor.model,
            name: descriptor.name,
            context_window: descriptor.context_window,
            selection: descriptor.selection,
        }
    }
}

/// The context window of the catalog entry whose selection names `model`.
#[must_use]
pub fn model_context_window(models: &[ModelOption], model: Option<&str>) -> Option<u32> {
    models
        .iter()
        .find(|option| option.selection.model.as_deref() == model)?
        .context_window
}

/// The profile and the first eight hex digits of the plan digest: enough to
/// tell two plans apart in a transcript without filling the line.
#[must_use]
pub fn plan_label(plan: &RunPlanIdentity) -> (AgentProfileId, String) {
    let digest = plan.digest.to_string();
    (
        plan.profile.clone(),
        digest[..digest.len().min(8)].to_owned(),
    )
}

/// Every session the client knows about, with the tree indexes a sidebar,
/// picker, or navigation reads every frame.
///
/// Reads go through `get`; writes go through `get_mut`, `insert`, `remove`,
/// `entry`, and `values_mut`, each of which drops the cached tree index so
/// the next read rebuilds it once. A frame therefore pays for the index at
/// most once per batch of mutations instead of once per call site.
#[derive(Debug)]
pub struct SessionStore {
    sessions: HashMap<SessionId, SessionView>,
    index: OnceCell<TreeIndex>,
    sanitizer: TextSanitizer,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::with_sanitizer(plain_text_character)
    }
}

/// Derived tree shape, valid until the next mutation.
#[derive(Debug, Default)]
struct TreeIndex {
    /// Depth-first order: roots newest-first, children oldest-first.
    order: Vec<SessionId>,
    depth: HashMap<SessionId, usize>,
    /// Children oldest-first, keyed by parent; `None` holds the roots.
    children: HashMap<Option<SessionId>, Vec<SessionId>>,
    spawned_by_call: HashMap<ToolCallId, SessionId>,
}

impl SessionStore {
    /// A store whose live-status tails pass every character through
    /// `sanitizer` before it is retained.
    #[must_use]
    pub fn with_sanitizer(sanitizer: TextSanitizer) -> Self {
        Self {
            sessions: HashMap::new(),
            index: OnceCell::new(),
            sanitizer,
        }
    }

    #[must_use]
    pub fn sanitizer(&self) -> TextSanitizer {
        self.sanitizer
    }

    #[must_use]
    pub fn get(&self, id: &SessionId) -> Option<&SessionView> {
        self.sessions.get(id)
    }

    pub fn get_mut(&mut self, id: &SessionId) -> Option<&mut SessionView> {
        self.index.take();
        self.sessions.get_mut(id)
    }

    #[must_use]
    pub fn contains_key(&self, id: &SessionId) -> bool {
        self.sessions.contains_key(id)
    }

    pub fn insert(&mut self, id: SessionId, view: SessionView) -> Option<SessionView> {
        self.index.take();
        self.sessions.insert(id, view)
    }

    pub fn remove(&mut self, id: &SessionId) -> Option<SessionView> {
        self.index.take();
        self.sessions.remove(id)
    }

    pub fn entry(&mut self, id: SessionId) -> hash_map::Entry<'_, SessionId, SessionView> {
        self.index.take();
        self.sessions.entry(id)
    }

    pub fn clear(&mut self) {
        self.index.take();
        self.sessions.clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn values(&self) -> impl Iterator<Item = &SessionView> {
        self.sessions.values()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut SessionView> {
        self.index.take();
        self.sessions.values_mut()
    }

    /// Every session in tree order: roots newest-first, each followed by
    /// its descendants oldest-first.
    #[must_use]
    pub fn thread_order(&self) -> &[SessionId] {
        &self.index().order
    }

    /// Distance from the root; zero for roots and unknown sessions.
    #[must_use]
    pub fn depth(&self, id: SessionId) -> usize {
        self.index().depth.get(&id).copied().unwrap_or(0)
    }

    /// Direct children of `parent`, oldest-first.
    #[must_use]
    pub fn children_of(&self, parent: SessionId) -> &[SessionId] {
        self.index()
            .children
            .get(&Some(parent))
            .map_or(&[], Vec::as_slice)
    }

    /// Root sessions, oldest-first.
    #[must_use]
    pub fn roots(&self) -> &[SessionId] {
        self.index().children.get(&None).map_or(&[], Vec::as_slice)
    }

    /// The child session a `spawn_agent` call created, if any.
    #[must_use]
    pub fn child_spawned_by(&self, tool_call_id: ToolCallId) -> Option<SessionId> {
        self.index().spawned_by_call.get(&tool_call_id).copied()
    }

    /// Sessions with a tool call awaiting approval, in tree order.
    #[must_use]
    pub fn awaiting_approval(&self) -> Vec<SessionId> {
        self.thread_order()
            .iter()
            .copied()
            .filter(|id| !self.sessions[id].live.awaiting_approval.is_empty())
            .collect()
    }

    /// Sessions that need the user, most urgent first and then in tree
    /// order: approvals, then unread failures, then unread finishes.
    #[must_use]
    pub fn needing_attention(&self) -> Vec<SessionId> {
        let mut needing: Vec<(Need, usize, SessionId)> = self
            .thread_order()
            .iter()
            .enumerate()
            .filter_map(|(position, id)| self.sessions[id].need().map(|need| (need, position, *id)))
            .collect();
        needing.sort();
        needing.into_iter().map(|(_, _, id)| id).collect()
    }

    /// Insert or refresh a summary. A new session starts summary-only with
    /// `loaded_through` as its baseline cursor.
    pub fn upsert_summary(
        &mut self,
        summary: SessionSummary,
        models: &[ModelOption],
        loaded_through: u64,
    ) {
        let context_window = model_context_window(models, summary.model.as_deref());
        match self.entry(summary.id) {
            hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().set_summary(summary, context_window);
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(SessionView::summary_only(
                    summary,
                    context_window,
                    loaded_through,
                ));
            }
        }
    }

    /// Recompute every session's context window against a new catalog.
    pub fn apply_models(&mut self, models: &[ModelOption]) {
        for session in self.values_mut() {
            session.context_window = model_context_window(models, session.summary.model.as_deref());
        }
    }

    /// Load one session's transcript body from a snapshot. Other warm bodies
    /// are untouched; [`Self::evict_cold_bodies`] enforces the warm limit
    /// afterwards. Focus-independent per-session state (drafts, focus stamp,
    /// live output for still-running calls, approval previews) survives.
    pub fn install_session_snapshot(
        &mut self,
        snapshot: SessionSnapshot,
        models: &[ModelOption],
        loaded_through: u64,
    ) {
        let session_id = snapshot.summary.id;
        let mut messages = snapshot.messages;
        retain_recent_messages(&mut messages);
        let history = messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.output.clone())
            .filter(|prompt| !prompt.trim().is_empty())
            .collect::<VecDeque<_>>();
        let mut tool_calls = snapshot.tool_calls;
        retain_recent_tool_calls(&mut tool_calls);
        let context_window = model_context_window(models, snapshot.summary.model.as_deref());
        let previous = self.remove(&session_id);
        let mut view = SessionView::summary_only(snapshot.summary, context_window, loaded_through);
        view.live = LiveStatus::from_body(&messages, &tool_calls, self.sanitizer);
        for run in &snapshot.runs {
            let stats = view.runs.entry(run.id).or_default();
            stats.outcome = run.outcome.clone();
            stats.usage = run.usage;
            stats.cost_usd_nanos = run.estimated_cost_usd_nanos;
            stats.plan = run.plan.as_deref().map(plan_label);
            stats.resolved_route = run
                .resolved_model
                .as_deref()
                .map(|model| model.route.clone());
        }
        for call in &tool_calls {
            if matches!(
                call.state,
                ToolCallState::Completed | ToolCallState::Failed | ToolCallState::Denied
            ) {
                view.runs.entry(call.run_id).or_default().tool_calls += 1;
            }
        }
        view.prompt_history = history
            .into_iter()
            .rev()
            .take(MAX_PROMPT_HISTORY)
            .rev()
            .collect();
        if let Some(previous) = previous {
            view.last_focused = previous.last_focused;
            view.drafts = previous.drafts;
            // Live tool output for calls this body no longer reports as
            // running would render forever; keep only the running ones.
            view.live_tool_output = previous.live_tool_output;
            view.live_tool_output.retain(|id, _| {
                tool_calls
                    .iter()
                    .any(|call| call.id == *id && call.state == ToolCallState::Running)
            });
            view.approval_previews = previous.approval_previews;
        }
        view.messages = Some(messages);
        view.tool_calls = Some(tool_calls);
        self.insert(session_id, view);
    }

    /// A session this client just created has an empty transcript by
    /// construction, so it is warm immediately and needs no round trip.
    pub fn warm_empty(&mut self, session_id: SessionId) {
        if let Some(session) = self.get_mut(&session_id)
            && !session.is_warm()
        {
            session.messages = Some(Vec::new());
            session.tool_calls = Some(Vec::new());
        }
    }

    /// Stamp `session_id` with `focus_clock` and clear what the user has now
    /// seen. The caller owns the clock so one counter can span stores.
    pub fn mark_focused(&mut self, session_id: SessionId, focus_clock: u64) {
        if let Some(session) = self.get_mut(&session_id) {
            session.last_focused = focus_clock;
            session.unread = 0;
            session.finished_unread = false;
        }
    }

    /// Drop transcript bodies beyond the warm limit, least recently focused
    /// first. `pinned` is never evicted. Summaries and live status stay, so
    /// lists and pickers keep working for cold sessions.
    pub fn evict_cold_bodies(&mut self, pinned: Option<SessionId>) {
        let mut warm: Vec<(u64, SessionId)> = self
            .values()
            .filter(|session| session.is_warm() && Some(session.summary.id) != pinned)
            .map(|session| (session.last_focused, session.summary.id))
            .collect();
        let keep = WARM_BODY_LIMIT.saturating_sub(usize::from(pinned.is_some()));
        if warm.len() <= keep {
            return;
        }
        warm.sort_unstable();
        let evict = warm.len() - keep;
        for (_, session_id) in warm.into_iter().take(evict) {
            if let Some(session) = self.get_mut(&session_id) {
                session.evict_body();
            }
        }
    }

    /// Insert a message into its session's warm body, deduplicated by id and
    /// placed after its run's last message so live order matches snapshots.
    pub fn push_message(&mut self, message: MessageSnapshot) {
        let Some(messages) = self
            .get_mut(&message.session_id)
            .and_then(|session| session.messages.as_mut())
        else {
            return;
        };
        if messages
            .iter()
            .rev()
            .any(|candidate| candidate.id == message.id)
        {
            return;
        }
        // Server snapshots order messages by run first, then by ordinal
        // within the run, so a prompt queued mid-run sorts after that run's
        // later per-turn messages. Mirror that live: a message whose run is
        // already present slots in right after the run's last message, and a
        // new run appends (runs are created in queue order).
        let position = messages
            .iter()
            .rposition(|candidate| candidate.run_id == message.run_id)
            .map_or(messages.len(), |index| index + 1);
        messages.insert(position, message);
        retain_recent_messages(messages);
    }

    /// Replace or append a tool call in its session's warm body.
    pub fn upsert_tool_call(&mut self, tool_call: ToolCallSnapshot) {
        let Some(tool_calls) = self
            .get_mut(&tool_call.session_id)
            .and_then(|session| session.tool_calls.as_mut())
        else {
            return;
        };
        // Updates target recent calls; scan from the tail.
        if let Some(existing) = tool_calls
            .iter_mut()
            .rev()
            .find(|existing| existing.id == tool_call.id)
        {
            *existing = tool_call;
        } else {
            tool_calls.push(tool_call);
            retain_recent_tool_calls(tool_calls);
        }
    }

    /// The streaming message is nearly always the newest, so scan from the
    /// tail: a text delta then costs one comparison rather than a walk over
    /// the retained history.
    fn message_mut(
        &mut self,
        session_id: SessionId,
        message_id: MessageId,
    ) -> Option<&mut MessageSnapshot> {
        self.get_mut(&session_id)?
            .messages
            .as_mut()?
            .iter_mut()
            .rev()
            .find(|message| message.id == message_id)
    }

    /// Marks a run's still-streaming assistant messages complete through the
    /// given turn: the server finalizes a turn's message in the same
    /// transaction as the turn's tool calls, without a dedicated event.
    fn complete_streamed_turns(&mut self, session_id: SessionId, run_id: RunId, through_turn: u16) {
        let Some(messages) = self
            .get_mut(&session_id)
            .and_then(|session| session.messages.as_mut())
        else {
            return;
        };
        for message in messages.iter_mut().filter(|message| {
            message.run_id == run_id
                && message.role == MessageRole::Assistant
                && message.state == MessageState::Streaming
                && message.turn_ordinal <= through_turn
        }) {
            message.state = MessageState::Complete;
        }
    }

    fn index(&self) -> &TreeIndex {
        self.index.get_or_init(|| {
            let mut index = TreeIndex::default();
            for session in self.sessions.values() {
                index
                    .children
                    .entry(session.summary.parent_id)
                    .or_default()
                    .push(session.summary.id);
                if let Some(origin) = session.summary.spawned_by
                    && let Some(call) = origin.tool_call_id
                {
                    index.spawned_by_call.insert(call, session.summary.id);
                }
            }
            // Ties on the timestamp fall back to the title so the list is stable
            // across frames whatever the map's iteration order.
            for siblings in index.children.values_mut() {
                siblings.sort_by(|a, b| {
                    let (a, b) = (&self.sessions[a].summary, &self.sessions[b].summary);
                    a.updated_at_ms
                        .cmp(&b.updated_at_ms)
                        .then_with(|| b.title.cmp(&a.title))
                });
            }
            // Roots are newest-first; popping from the back yields the newest.
            let mut stack: Vec<(SessionId, usize)> = index
                .children
                .get(&None)
                .into_iter()
                .flatten()
                .map(|id| (*id, 0))
                .collect();
            index.order.reserve(self.sessions.len());
            while let Some((session_id, depth)) = stack.pop() {
                index.order.push(session_id);
                index.depth.insert(session_id, depth);
                if let Some(kids) = index.children.get(&Some(session_id)) {
                    // Children render oldest-first, so push newest first to
                    // be popped last.
                    stack.extend(kids.iter().rev().map(|id| (*id, depth + 1)));
                }
            }
            index
        })
    }
}

impl std::ops::Index<&SessionId> for SessionStore {
    type Output = SessionView;

    fn index(&self, id: &SessionId) -> &SessionView {
        &self.sessions[id]
    }
}

/// Trim a warm body's messages to the snapshot window, oldest first.
pub fn retain_recent_messages(messages: &mut Vec<MessageSnapshot>) {
    let excess = messages
        .len()
        .saturating_sub(usize::from(SNAPSHOT_MESSAGE_LIMIT));
    if excess > 0 {
        messages.drain(..excess);
    }
}

/// Trim a warm body's tool calls to the retained window, oldest first.
pub fn retain_recent_tool_calls(tool_calls: &mut Vec<ToolCallSnapshot>) {
    let excess = tool_calls.len().saturating_sub(MAX_RECENT_TOOL_CALLS);
    if excess > 0 {
        tool_calls.drain(..excess);
    }
}

/// A snapshot request for one session's body, at the limits the store
/// retains, so a surface never asks for more than it can keep.
#[must_use]
pub fn body_request(workspace_id: WorkspaceId, session_id: SessionId) -> SnapshotRequest {
    SnapshotRequest {
        workspace_id,
        focused_session_id: Some(session_id),
        include_sessions: Vec::new(),
        session_limit: SNAPSHOT_SESSION_LIMIT,
        message_limit: SNAPSHOT_MESSAGE_LIMIT,
    }
}

/// One run's displayable reasoning: text accumulated from `ReasoningDelta`
/// events plus whether the block is still streaming.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reasoning {
    pub text: String,
    pub streaming: bool,
    /// Surface animation ticks observed while streaming, for an elapsed
    /// label. The reducer never touches this; the surface's tick does.
    pub ticks: usize,
}

impl Reasoning {
    pub fn append(&mut self, text: &str) {
        self.text.push_str(text);
        if self.text.len() > MAX_REASONING_BYTES {
            let mut start = self.text.len() - MAX_REASONING_BYTES;
            while !self.text.is_char_boundary(start) {
                start += 1;
            }
            self.text.drain(..start);
        }
    }
}

/// What is known about one run's timing and outcome, for the completion
/// line under its last message. Timestamps are the server's `occurred_at_ms`
/// of the run events; historical runs loaded from a snapshot carry only the
/// outcome and usage until the protocol records run timing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunStats {
    pub started_at_ms: Option<u64>,
    /// When the first assistant text of the run arrived: time to first token.
    pub first_token_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<RunOutcome>,
    pub usage: Option<TokenUsage>,
    /// Tool calls the run made, counted as their finished events arrive or
    /// from the loaded body.
    pub tool_calls: u32,
    /// Estimated cost of the run, when the accounting delta was observable.
    pub cost_usd_nanos: Option<u64>,
    /// Highest model turn committed so far, from `ModelTurnCompleted`; zero
    /// for historical runs loaded from a snapshot.
    pub turns: u16,
    /// The profile the run was claimed under and the first eight hex digits
    /// of its plan digest, from `RunStarted.plan` or the loaded run.
    pub plan: Option<(AgentProfileId, String)>,
    /// The `provider/model` route actually executed, when the run recorded
    /// it. Shown only when it differs from the session's selected model.
    pub resolved_route: Option<String>,
    /// Cost accumulated from committed turns while the run is active. Shown
    /// until `RunFinished` settles `cost_usd_nanos` from the session totals.
    pub live_cost_usd_nanos: Option<u64>,
}

/// When a tool call started, last produced output, and finished, from the
/// `occurred_at_ms` of the events that carried those transitions. Historical
/// calls loaded from a snapshot have none of this until the protocol records
/// call timing; the rows then show no wall-clock time rather than a guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallTiming {
    pub started_at_ms: Option<u64>,
    pub last_output_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

/// Cheap per-session liveness reduced from every event, whether or not the
/// session's transcript body is loaded. This is what a sidebar or session
/// list shows for the sessions the user is not looking at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveStatus {
    /// Last bytes of the newest assistant message, whitespace-collapsed.
    pub tail: String,
    /// The previous append ended in whitespace that has not yet been emitted
    /// as a separator.
    tail_space_pending: bool,
    /// Name of the tool call currently running or awaiting approval.
    pub active_tool: Option<String>,
    /// Tool calls awaiting an approval answer. A set rather than a count so
    /// replayed or repeated events cannot drift it.
    pub awaiting_approval: BTreeSet<ToolCallId>,
}

/// What one `ToolApprovalRequested` event said the call would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalPreview {
    pub shell: Option<ShellCommandPreview>,
    pub edit: Option<EditPreview>,
}

/// One session as the client sees it: the summary every surface lists, and
/// the warm body, live status, and per-run detail a transcript shows.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub summary: SessionSummary,
    /// `Some` only while the body is warm; `None` means summary-only.
    pub messages: Option<Vec<MessageSnapshot>>,
    pub tool_calls: Option<Vec<ToolCallSnapshot>>,
    pub context_window: Option<u32>,
    /// Latest replaceable liveness state for the active run, seeded from the
    /// summary on load and replaced by `RunActivityChanged` events.
    pub activity: Option<(RunId, RunActivity)>,
    pub live: LiveStatus,
    /// Provider-exposed reasoning per run, bounded, kept only for runs whose
    /// messages are loaded. Display-only: never fed back to the model.
    pub reasoning: HashMap<RunId, Reasoning>,
    /// Timing and outcome per run, for completion lines. Bounded with
    /// `reasoning`: runs whose messages were trimmed are dropped together.
    pub runs: HashMap<RunId, RunStats>,
    /// Focus clock at the last time this session was focused; orders warm
    /// body eviction. Zero for never-focused sessions.
    pub last_focused: u64,
    /// Sequence of the snapshot this view was loaded from; events at or
    /// below it are already reflected.
    pub loaded_through: u64,
    /// Prompts this session has submitted, oldest first, for history browsing.
    pub prompt_history: VecDeque<String>,
    /// Drafts held locally while the session runs; they submit in order when
    /// it goes idle. Bounded by [`MAX_QUEUED_DRAFTS`].
    pub drafts: VecDeque<String>,
    /// Bounded tails of live streamed output per running tool call, dropped
    /// when the call reaches a terminal state or the body reloads.
    pub live_tool_output: HashMap<ToolCallId, String>,
    /// Shell and edit previews carried by approval requests, kept only while
    /// the call awaits an answer so a modal can show what it would do. The
    /// server computes these; the client never re-derives them from arguments.
    pub approval_previews: HashMap<ToolCallId, ApprovalPreview>,
    /// Observed timing per tool call, bounded with the warm body.
    pub tool_timing: HashMap<ToolCallId, ToolCallTiming>,
    /// Assistant messages and run finishes that arrived while this session
    /// was not shown, cleared when it is focused.
    pub unread: u32,
    /// The last run finished while unfocused and has not been looked at.
    pub finished_unread: bool,
}

/// Why a session needs the user, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    Approval,
    Failed,
    FinishedUnread,
}

/// Which list group a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    NeedsYou,
    Working,
    Idle,
    Done,
}

impl Group {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "NEEDS YOU",
            Self::Working => "WORKING",
            Self::Idle => "IDLE",
            Self::Done => "DONE",
        }
    }
}

impl SessionView {
    #[must_use]
    pub fn summary_only(
        summary: SessionSummary,
        context_window: Option<u32>,
        loaded_through: u64,
    ) -> Self {
        let activity = summary.active_run_id.zip(summary.activity);
        Self {
            summary,
            messages: None,
            tool_calls: None,
            context_window,
            activity,
            live: LiveStatus::default(),
            reasoning: HashMap::new(),
            runs: HashMap::new(),
            last_focused: 0,
            loaded_through,
            prompt_history: VecDeque::new(),
            drafts: VecDeque::new(),
            live_tool_output: HashMap::new(),
            approval_previews: HashMap::new(),
            tool_timing: HashMap::new(),
            unread: 0,
            finished_unread: false,
        }
    }

    /// The most urgent reason this session needs the user, if any.
    #[must_use]
    pub fn need(&self) -> Option<Need> {
        if !self.live.awaiting_approval.is_empty() {
            return Some(Need::Approval);
        }
        if matches!(self.summary.last_outcome, Some(RunOutcome::Failed { .. }))
            && self.finished_unread
        {
            return Some(Need::Failed);
        }
        if self.finished_unread {
            return Some(Need::FinishedUnread);
        }
        None
    }

    /// List group: needs attention, running, idle with no history, or
    /// finished and already seen.
    #[must_use]
    pub fn group(&self) -> Group {
        if self.need().is_some() {
            return Group::NeedsYou;
        }
        match self.summary.status {
            SessionStatus::Running | SessionStatus::Queued => Group::Working,
            SessionStatus::Idle => {
                if self.summary.last_outcome.is_some() {
                    Group::Done
                } else {
                    Group::Idle
                }
            }
        }
    }

    /// Refresh the summary in place. Activity follows the summary when the
    /// summary carries it or the run changed; a live event already applied
    /// for the same run is kept when the summary is silent.
    pub fn set_summary(&mut self, summary: SessionSummary, context_window: Option<u32>) {
        match (summary.active_run_id, summary.activity) {
            (Some(run_id), Some(activity)) => self.activity = Some((run_id, activity)),
            (Some(run_id), None) => {
                if self.activity.is_some_and(|(active, _)| active != run_id) {
                    self.activity = None;
                }
            }
            (None, _) => self.activity = None,
        }
        self.summary = summary;
        self.context_window = context_window;
    }

    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.messages.is_some()
    }

    /// Append a live output chunk for `tool_call_id`, keeping the tail bounded.
    pub fn append_live_tool_output(&mut self, tool_call_id: ToolCallId, chunk: &str) {
        let buffer = self.live_tool_output.entry(tool_call_id).or_default();
        buffer.push_str(chunk);
        if buffer.len() > MAX_LIVE_TOOL_OUTPUT_BYTES {
            let mut start = buffer.len() - MAX_LIVE_TOOL_OUTPUT_BYTES;
            while !buffer.is_char_boundary(start) {
                start += 1;
            }
            buffer.drain(..start);
        }
    }

    /// Remember a submitted prompt, skipping consecutive duplicates.
    pub fn record_prompt(&mut self, prompt: &str) {
        if self
            .prompt_history
            .back()
            .is_some_and(|previous| previous == prompt)
        {
            return;
        }
        self.prompt_history.push_back(prompt.to_owned());
        while self.prompt_history.len() > MAX_PROMPT_HISTORY {
            self.prompt_history.pop_front();
        }
    }

    /// Drop the warm body and everything keyed by its tool calls.
    pub fn evict_body(&mut self) {
        self.messages = None;
        self.tool_calls = None;
        self.live_tool_output.clear();
        self.approval_previews.clear();
        self.tool_timing.clear();
    }
}

impl LiveStatus {
    /// Derive status from a loaded body, as after a snapshot.
    #[must_use]
    pub fn from_body(
        messages: &[MessageSnapshot],
        tool_calls: &[ToolCallSnapshot],
        sanitizer: TextSanitizer,
    ) -> Self {
        let mut live = Self::default();
        if let Some(message) = messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
        {
            live.set_tail(&message.output, sanitizer);
        }
        for call in tool_calls {
            live.note_tool_call(call);
        }
        live
    }

    /// Replace the tail with the last [`LIVE_TAIL_BYTES`] of `text`, with
    /// whitespace collapsed to single spaces so it fits one row.
    pub fn set_tail(&mut self, text: &str, sanitizer: TextSanitizer) {
        let mut start = text.len().saturating_sub(LIVE_TAIL_BYTES);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        self.tail.clear();
        self.tail_space_pending = false;
        self.push_collapsed(&text[start..], sanitizer);
    }

    fn push_collapsed(&mut self, text: &str, sanitizer: TextSanitizer) {
        for character in text.chars() {
            if character.is_whitespace() {
                self.tail_space_pending = !self.tail.is_empty();
            } else if let Some(character) = sanitizer(character) {
                if self.tail_space_pending {
                    self.tail.push(' ');
                    self.tail_space_pending = false;
                }
                self.tail.push(character);
            }
        }
    }

    /// Append streamed text and trim the front back to the byte bound.
    pub fn append_tail(&mut self, text: &str, sanitizer: TextSanitizer) {
        if text.len() >= LIVE_TAIL_BYTES {
            self.set_tail(text, sanitizer);
            return;
        }
        self.push_collapsed(text, sanitizer);
        if self.tail.len() > LIVE_TAIL_BYTES {
            let mut start = self.tail.len() - LIVE_TAIL_BYTES;
            while !self.tail.is_char_boundary(start) {
                start += 1;
            }
            self.tail.drain(..start);
        }
    }

    pub fn note_tool_call(&mut self, call: &ToolCallSnapshot) {
        match call.state {
            ToolCallState::AwaitingApproval => {
                self.awaiting_approval.insert(call.id);
                self.active_tool = Some(call.name.clone());
            }
            ToolCallState::Running | ToolCallState::Requested => {
                self.awaiting_approval.remove(&call.id);
                self.active_tool = Some(call.name.clone());
            }
            ToolCallState::Completed
            | ToolCallState::Failed
            | ToolCallState::Denied
            | ToolCallState::Interrupted => {
                self.awaiting_approval.remove(&call.id);
                if self.active_tool.as_deref() == Some(call.name.as_str()) {
                    self.active_tool = None;
                }
            }
        }
    }
}

const fn tool_call_state_is_terminal(state: ToolCallState) -> bool {
    match state {
        ToolCallState::Completed
        | ToolCallState::Failed
        | ToolCallState::Denied
        | ToolCallState::Interrupted => true,
        ToolCallState::Requested | ToolCallState::AwaitingApproval | ToolCallState::Running => {
            false
        }
    }
}
