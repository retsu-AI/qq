use std::{
    cell::OnceCell,
    collections::{BTreeSet, HashMap, VecDeque, hash_map},
};

use qq_protocol::{
    AgentProfileId, ApprovalResolution, MessageId, MessageRole, MessageSnapshot, MessageState,
    ModelDescriptor, PendingHold, RunActivity, RunId, RunOutcome, RunPlanIdentity, SessionId,
    SessionSnapshot, SessionStatus, SessionSummary, SnapshotRequest, TokenUsage, ToolCallId,
    ToolCallSnapshot, ToolCallState, WorkspaceId,
};

mod reduce;
#[cfg(test)]
mod tests;

pub use reduce::{Attention, NoticeLevel, ReduceContext, StateEffect, StateEffects};

pub const WARM_BODY_LIMIT: usize = 8;
pub const LIVE_TAIL_BYTES: usize = 256;
pub const MAX_REASONING_BYTES: usize = 16 * 1024;
pub const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
pub const MAX_PROMPT_HISTORY: usize = 100;
pub const MAX_QUEUED_DRAFTS: usize = 8;
pub const SNAPSHOT_SESSION_LIMIT: u16 = 512;
pub const SNAPSHOT_MESSAGE_LIMIT: u16 = 256;
pub const MAX_RECENT_TOOL_CALLS: usize = 64;

pub type TextSanitizer = fn(char) -> Option<char>;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider: String,
    pub model: String,
    pub name: Option<String>,
    pub context_window: Option<u32>,
    pub reasoning_efforts: Vec<qq_protocol::ReasoningEffort>,
    pub selection: qq_protocol::ModelSelection,
}

impl From<ModelDescriptor> for ModelOption {
    fn from(descriptor: ModelDescriptor) -> Self {
        Self {
            provider: descriptor.provider,
            model: descriptor.model,
            name: descriptor.name,
            context_window: descriptor.context_window,
            reasoning_efforts: descriptor.reasoning_efforts,
            selection: descriptor.selection,
        }
    }
}

#[must_use]
pub fn model_reasoning_efforts<'a>(
    models: &'a [ModelOption],
    model: Option<&str>,
) -> &'a [qq_protocol::ReasoningEffort] {
    models
        .iter()
        .find(|option| option.selection.model.as_deref() == model)
        .map_or(&[], |option| option.reasoning_efforts.as_slice())
}

#[must_use]
pub fn model_context_window(models: &[ModelOption], model: Option<&str>) -> Option<u32> {
    models
        .iter()
        .find(|option| option.selection.model.as_deref() == model)?
        .context_window
}

#[must_use]
pub fn plan_label(plan: &RunPlanIdentity) -> (AgentProfileId, String) {
    let digest = plan.digest.to_string();
    (
        plan.profile.clone(),
        digest[..digest.len().min(8)].to_owned(),
    )
}

#[derive(Debug)]
pub struct SessionStore {
    sessions: HashMap<SessionId, SessionView>,
    index: OnceCell<TreeIndex>,
    #[cfg(test)]
    index_rebuilds: std::cell::Cell<usize>,
    sanitizer: TextSanitizer,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::with_sanitizer(plain_text_character)
    }
}

#[derive(Debug, Default)]
struct TreeIndex {
    order: Vec<SessionId>,
    depth: HashMap<SessionId, usize>,
    children: HashMap<Option<SessionId>, Vec<SessionId>>,
    spawned_by_call: HashMap<ToolCallId, SessionId>,
}

impl SessionStore {
    #[must_use]
    pub fn with_sanitizer(sanitizer: TextSanitizer) -> Self {
        Self {
            sessions: HashMap::new(),
            index: OnceCell::new(),
            #[cfg(test)]
            index_rebuilds: std::cell::Cell::new(0),
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

    pub fn body_mut(&mut self, id: &SessionId) -> Option<&mut SessionView> {
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

    #[must_use]
    pub fn thread_order(&self) -> &[SessionId] {
        &self.index().order
    }

    #[must_use]
    pub fn depth(&self, id: SessionId) -> usize {
        self.index().depth.get(&id).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn root_of(&self, mut id: SessionId) -> Option<SessionId> {
        for _ in 0..=self.depth(id) {
            match self.get(&id)?.summary.parent_id {
                Some(parent) => id = parent,
                None => return Some(id),
            }
        }
        None
    }

    #[must_use]
    pub fn children_of(&self, parent: SessionId) -> &[SessionId] {
        self.index()
            .children
            .get(&Some(parent))
            .map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn roots(&self) -> &[SessionId] {
        self.index().children.get(&None).map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn child_spawned_by(&self, tool_call_id: ToolCallId) -> Option<SessionId> {
        self.index().spawned_by_call.get(&tool_call_id).copied()
    }

    #[must_use]
    pub fn awaiting_approval(&self) -> Vec<SessionId> {
        self.thread_order()
            .iter()
            .copied()
            .filter(|id| !self.sessions[id].live.awaiting_approval.is_empty())
            .collect()
    }

    #[must_use]
    pub fn activity(&self) -> StoreActivity {
        let mut working = false;
        let mut blocked: Option<&SessionView> = None;
        for view in self.sessions.values() {
            let live = &view.live;
            if !live.awaiting_approval.is_empty() {
                if blocked.is_none_or(|current| {
                    (view.summary.updated_at_ms, view.summary.id)
                        > (current.summary.updated_at_ms, current.summary.id)
                }) {
                    blocked = Some(view);
                }
                continue;
            }
            working |= view.summary.running_side_question.is_some()
                || matches!(
                    view.summary.status,
                    SessionStatus::Running | SessionStatus::Queued
                );
        }
        if let Some(view) = blocked {
            return StoreActivity::Blocked {
                session_title: view.summary.title.clone(),
            };
        }
        if working {
            StoreActivity::Working
        } else {
            StoreActivity::Idle
        }
    }

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

    pub fn apply_models(&mut self, models: &[ModelOption]) {
        for session in self.values_mut() {
            session.context_window = model_context_window(models, session.summary.model.as_deref());
        }
    }

    pub fn install_session_snapshot(
        &mut self,
        snapshot: SessionSnapshot,
        models: &[ModelOption],
        loaded_through: u64,
    ) {
        self.install_body(snapshot, models, loaded_through, false);
    }

    pub fn install_isolated_session_snapshot(
        &mut self,
        snapshot: SessionSnapshot,
        models: &[ModelOption],
        loaded_through: u64,
    ) {
        self.install_body(snapshot, models, loaded_through, true);
    }

    fn install_body(
        &mut self,
        snapshot: SessionSnapshot,
        models: &[ModelOption],
        loaded_through: u64,
        isolated: bool,
    ) {
        let session_id = snapshot.summary.id;
        if !isolated && (snapshot.has_older_messages || snapshot.has_older_tool_calls) {
            self.upsert_summary(snapshot.summary, models, loaded_through);
            if let Some(view) = self.body_mut(&session_id) {
                view.evict_body();
                view.loaded_through = loaded_through;
            }
            return;
        }
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
            stats.origin = run.origin;
            stats.owned_step = run
                .origin
                .is_some_and(qq_protocol::RunOrigin::is_in_run_compaction)
                || (run.status == qq_protocol::RunStatus::Running
                    && view.summary.active_run_id != Some(run.id));
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
            if view.foreground_run_id.is_none() {
                view.foreground_run_id = previous.foreground_run_id;
            }
            for (run_id, observed) in &previous.runs {
                if let Some(stats) = view.runs.get_mut(run_id) {
                    stats.started_at_ms = observed.started_at_ms;
                    stats.first_token_at_ms = observed.first_token_at_ms;
                    stats.finished_at_ms = observed.finished_at_ms;
                    stats.turns = observed.turns;
                    stats.live_cost_usd_nanos = observed.live_cost_usd_nanos;
                    stats.owned_step |= observed.owned_step;
                    stats.origin = stats.origin.or(observed.origin);
                }
            }
            view.unread = previous.unread;
            view.finished_unread = previous.finished_unread;
            view.last_focused = previous.last_focused;
            view.drafts = previous.drafts;
            view.live_tool_output = previous.live_tool_output;
            view.live_tool_output.retain(|id, _| {
                tool_calls
                    .iter()
                    .any(|call| call.id == *id && call.state == ToolCallState::Running)
            });
        }
        view.approval_previews = snapshot
            .approval_previews
            .into_iter()
            .map(|pending| (pending.tool_call_id, pending.preview))
            .collect();
        view.live.set_holds(&view.summary.pending_holds);
        view.side_questions = snapshot.side_questions;
        view.goal = snapshot.goal;
        view.messages = Some(messages);
        view.tool_calls = Some(tool_calls);
        self.insert(session_id, view);
    }

    pub fn warm_empty(&mut self, session_id: SessionId) {
        if let Some(session) = self.body_mut(&session_id)
            && !session.is_warm()
        {
            session.messages = Some(Vec::new());
            session.tool_calls = Some(Vec::new());
        }
    }

    pub fn mark_focused(&mut self, session_id: SessionId, focus_clock: u64) {
        if let Some(session) = self.body_mut(&session_id) {
            session.last_focused = focus_clock;
            session.unread = 0;
            session.finished_unread = false;
        }
    }

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
            if let Some(session) = self.body_mut(&session_id) {
                session.evict_body();
            }
        }
    }

    pub fn push_message(&mut self, message: MessageSnapshot) {
        let Some(messages) = self
            .body_mut(&message.session_id)
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
        let position = messages
            .iter()
            .rposition(|candidate| candidate.run_id == message.run_id)
            .map_or(messages.len(), |index| index + 1);
        messages.insert(position, message);
        retain_recent_messages(messages);
    }

    pub fn upsert_tool_call(&mut self, tool_call: ToolCallSnapshot) {
        let Some(session) = self.body_mut(&tool_call.session_id) else {
            return;
        };
        let Some(tool_calls) = session.tool_calls.as_mut() else {
            return;
        };
        if let Some(existing) = tool_calls
            .iter_mut()
            .rev()
            .find(|existing| existing.id == tool_call.id)
        {
            *existing = tool_call;
        } else {
            tool_calls.push(tool_call);
            for evicted in retain_recent_tool_calls(tool_calls) {
                session.tool_timing.remove(&evicted);
            }
        }
    }

    fn message_mut(
        &mut self,
        session_id: SessionId,
        message_id: MessageId,
    ) -> Option<&mut MessageSnapshot> {
        self.body_mut(&session_id)?
            .messages
            .as_mut()?
            .iter_mut()
            .rev()
            .find(|message| message.id == message_id)
    }

    fn complete_streamed_turns(&mut self, session_id: SessionId, run_id: RunId, through_turn: u32) {
        let Some(messages) = self
            .body_mut(&session_id)
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

    #[cfg(test)]
    pub(crate) fn index_rebuilds(&self) -> usize {
        self.index_rebuilds.get()
    }

    fn index(&self) -> &TreeIndex {
        self.index.get_or_init(|| {
            #[cfg(test)]
            self.index_rebuilds.set(self.index_rebuilds.get() + 1);
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
            for siblings in index.children.values_mut() {
                siblings.sort_by(|a, b| {
                    let (a, b) = (&self.sessions[a].summary, &self.sessions[b].summary);
                    a.updated_at_ms
                        .cmp(&b.updated_at_ms)
                        .then_with(|| b.title.cmp(&a.title))
                });
            }
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

pub fn retain_recent_messages(messages: &mut Vec<MessageSnapshot>) {
    let excess = messages
        .len()
        .saturating_sub(usize::from(SNAPSHOT_MESSAGE_LIMIT));
    if excess > 0 {
        messages.drain(..excess);
    }
}

pub fn retain_recent_tool_calls(tool_calls: &mut Vec<ToolCallSnapshot>) -> Vec<ToolCallId> {
    let excess = tool_calls.len().saturating_sub(MAX_RECENT_TOOL_CALLS);
    if excess > 0 {
        tool_calls.drain(..excess).map(|call| call.id).collect()
    } else {
        Vec::new()
    }
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reasoning {
    pub text: String,
    pub streaming: bool,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunStats {
    pub origin: Option<qq_protocol::RunOrigin>,
    pub owned_step: bool,
    pub started_at_ms: Option<u64>,
    pub first_token_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<RunOutcome>,
    pub usage: Option<TokenUsage>,
    pub tool_calls: u32,
    pub cost_usd_nanos: Option<u64>,
    pub turns: u32,
    pub plan: Option<(AgentProfileId, String)>,
    pub resolved_route: Option<String>,
    pub live_cost_usd_nanos: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallTiming {
    pub started_at_ms: Option<u64>,
    pub last_output_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub settled: Option<ApprovalSettlement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalSettlement {
    pub resolution: ApprovalResolution,
}

impl ApprovalSettlement {
    #[must_use]
    pub fn label(self) -> Option<String> {
        match self.resolution {
            ApprovalResolution::ApprovedOnce | ApprovalResolution::Answered => None,
            ApprovalResolution::Denied => Some("denied by you".to_owned()),
            ApprovalResolution::DeniedTimeout => Some("denied by timeout".to_owned()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveStatus {
    pub tail: String,
    tail_space_pending: bool,
    pub active_tool: Option<String>,
    pub awaiting_approval: BTreeSet<ToolCallId>,
}

pub use qq_protocol::ApprovalPreview;

#[derive(Debug, Clone)]
pub struct SessionView {
    pub goal: Option<Box<qq_protocol::GoalSnapshot>>,
    pub side_questions: Vec<qq_protocol::SideQuestionSnapshot>,
    pub summary: SessionSummary,
    pub foreground_run_id: Option<RunId>,
    pub messages: Option<Vec<MessageSnapshot>>,
    pub tool_calls: Option<Vec<ToolCallSnapshot>>,
    pub context_window: Option<u32>,
    pub activity: Option<(RunId, RunActivity)>,
    pub live: LiveStatus,
    pub reasoning: HashMap<RunId, Reasoning>,
    pub runs: HashMap<RunId, RunStats>,
    pub last_focused: u64,
    pub loaded_through: u64,
    pub prompt_history: VecDeque<String>,
    pub drafts: VecDeque<String>,
    pub live_tool_output: HashMap<ToolCallId, String>,
    pub approval_previews: HashMap<ToolCallId, ApprovalPreview>,
    pub tool_timing: HashMap<ToolCallId, ToolCallTiming>,
    pub unread: u32,
    pub finished_unread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreActivity {
    Idle,
    Working,
    Blocked { session_title: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    Approval,
    Failed,
    FinishedUnread,
}

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
        let foreground_run_id = summary.active_run_id;
        let mut live = LiveStatus::default();
        live.set_holds(&summary.pending_holds);
        Self {
            side_questions: Vec::new(),
            goal: None,
            summary,
            foreground_run_id,
            messages: None,
            tool_calls: None,
            context_window,
            activity,
            live,
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

    pub fn set_summary(&mut self, summary: SessionSummary, context_window: Option<u32>) {
        if let Some(run_id) = summary.active_run_id {
            self.foreground_run_id = Some(run_id);
        }
        match (summary.active_run_id, summary.activity) {
            (Some(run_id), Some(activity)) => self.activity = Some((run_id, activity)),
            (Some(run_id), None) => {
                if self.activity.is_some_and(|(active, _)| active != run_id) {
                    self.activity = None;
                }
            }
            (None, _) => self.activity = None,
        }
        self.live.set_holds(&summary.pending_holds);
        self.summary = summary;
        self.context_window = context_window;
    }

    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.messages.is_some()
    }

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

    pub fn evict_body(&mut self) {
        self.messages = None;
        self.tool_calls = None;
        self.side_questions = Vec::new();
        self.live_tool_output.clear();
        self.approval_previews.clear();
        self.tool_timing.clear();
    }
}

impl LiveStatus {
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

    pub fn set_holds(&mut self, holds: &[PendingHold]) {
        self.awaiting_approval = holds.iter().map(|hold| hold.tool_call_id).collect();
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
