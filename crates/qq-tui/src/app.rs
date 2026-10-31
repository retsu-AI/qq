use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
pub use qq_client::state::ModelOption;
pub(crate) use qq_client::state::{
    ApprovalPreview, Attention, NoticeLevel, SessionStore, SessionView,
};
use qq_client::state::{
    MAX_QUEUED_DRAFTS, ReduceContext, StateEffect, body_request, model_context_window,
    model_reasoning_efforts,
};
use qq_protocol::{
    AgentProfileId, ApprovalDecision, ApprovalMode, ApprovalResolution, CommandId, CommandOutcome,
    CommandRequest, ModelSelection, QuestionPreview, ReasoningEffort, RunId, ServerCapabilities,
    SessionCommand, SessionEvent, SessionEventEnvelope, SessionId, SessionStatus,
    SideQuestionState, SteeringCapabilities, ToolCallSnapshot, ToolCallState, WorkspaceId,
    WorkspaceSnapshot,
};
use thiserror::Error;

use crate::{
    Action, ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState, Settings,
    commands::{self, Command, SlashAction, SlashEntry},
    composer::Composer,
    effect::{Effect, Effects, PendingSubmit, Redraw, SubmitTarget},
    input::{Mode, Overlay, SessionConfirm, approval_mode_label, effort_label},
    picker::Picker,
    terminal,
    theme::Theme,
    viewport::{TranscriptPane, View, Viewport},
};
mod pickers;

const MAX_EXPANDED_TOOL_CALLS: usize = 256;

const MAX_INPUT_BYTES: usize = 64 * 1024;
pub(crate) const ANIMATION_INTERVAL_MS: u64 = 125;
const MAX_RECENT_EVENTS: usize = 1024;
const MOUSE_SCROLL_ROWS: usize = 3;
const NOTICE_TICKS: u16 = 40;
const ESC_CANCEL_TICKS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalDefaults {
    pub duration_ms: u64,
    pub max_duration_ms: u64,
    pub max_runs: u32,
    pub default_runs: u32,
    pub per_run: qq_protocol::RunLimits,
    pub check_timeout_ms: u64,
}

impl Default for GoalDefaults {
    fn default() -> Self {
        Self {
            duration_ms: 86_400_000,
            max_duration_ms: 604_800_000,
            max_runs: 2000,
            default_runs: 200,
            per_run: qq_protocol::RunLimits {
                max_model_turns: Some(256),
                ..qq_protocol::RunLimits::default()
            },
            check_timeout_ms: 600_000,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    pub goals: GoalDefaults,
    pub settings: Settings,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
    pub unauthenticated_providers: Vec<ProviderRemedy>,
    pub themes: Vec<Theme>,
    pub workspace_root: Option<std::path::PathBuf>,
    pub environment: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRemedy {
    pub provider: String,
    pub remedy: String,
}

impl ProviderRemedy {
    #[must_use]
    pub fn message(&self) -> String {
        format!("{} needs a credential: {}", self.provider, self.remedy)
    }
}

pub(crate) const CHOOSE_MODEL_NOTICE: &str = "choose a model with /models";

pub async fn run<P>(client: P, options: TuiOptions) -> Result<Option<SessionId>, TuiError>
where
    P: ClientPort,
{
    terminal::run(client, App::new(options)).await
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O failed")]
    Terminal(#[from] std::io::Error),
    #[error("TUI client stopped{}", .0.as_ref().map(|reason| format!(": {reason}")).unwrap_or_default())]
    ClientStopped(Option<String>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ToolDetail {
    #[default]
    Rows,
    Folded,
}

impl ToolDetail {
    #[must_use]
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Rows => Self::Folded,
            Self::Folded => Self::Rows,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Rows => "rows",
            Self::Folded => "folded",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReasoningDetail {
    #[default]
    Collapsed,
    Expanded,
}

#[derive(Debug, Default)]
pub(crate) struct MentionCompletion {
    pub(crate) candidates: Vec<String>,
    pub(crate) query: Option<String>,
    pub(crate) cursor: Picker,
}

impl MentionCompletion {
    fn clear(&mut self) {
        self.candidates.clear();
        self.query = None;
        self.cursor.select(0);
    }

    fn invalidate(&mut self) {
        self.query = None;
    }
}

fn recently_edited_paths(session: &qq_client::state::SessionView, limit: usize) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for call in session
        .tool_calls
        .as_deref()
        .unwrap_or_default()
        .iter()
        .rev()
    {
        if !matches!(call.name.as_str(), "edit_file" | "write_file" | "read_file") {
            continue;
        }
        let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
            continue;
        };
        let mut found: Vec<&str> = Vec::new();
        if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
            found.push(path);
        }
        if let Some(edits) = arguments.get("edits").and_then(|v| v.as_array()) {
            found.extend(
                edits
                    .iter()
                    .filter_map(|e| e.get("path").and_then(|p| p.as_str())),
            );
        }
        for path in found {
            if !paths.iter().any(|p| p == path) {
                paths.push(path.to_owned());
                if paths.len() >= limit {
                    return paths;
                }
            }
        }
    }
    paths
}

fn has_mention_syntax(text: &str) -> bool {
    !qq_protocol::parse_mentions(text).mentions.is_empty()
}

#[derive(Debug, Clone)]
enum PendingIntent {
    GoalControl {
        session_id: SessionId,
    },
    GoalSet {
        session_id: SessionId,
    },
    Side {
        session_id: SessionId,
        text: String,
    },
    CancelSide {
        session_id: SessionId,
    },
    Create,
    Prompt {
        session_id: SessionId,
        text: String,
    },
    Cancel {
        session_id: SessionId,
    },
    Steer {
        session_id: SessionId,
        text: String,
    },
    Compact {
        session_id: SessionId,
    },
    Rollback {
        session_id: SessionId,
    },
    Approval {
        tool_call_id: qq_protocol::ToolCallId,
    },
    SetModel {
        session_id: SessionId,
    },
    SetProfile {
        session_id: SessionId,
    },
    SetApprovalMode {
        session_id: SessionId,
    },
    SetEffort {
        session_id: SessionId,
    },
    Delete {
        session_id: SessionId,
    },
    Prune,
    Undo,
}

#[derive(Debug)]
struct QueuedPrompt {
    text: String,
    session_id: Option<SessionId>,
    view: View,
}

pub(crate) struct App {
    pub settings: Settings,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
    pub(crate) unauthenticated_providers: Vec<ProviderRemedy>,
    pub(crate) environment: Vec<String>,
    pub profile: AgentProfileId,
    pub approval_mode: ApprovalMode,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub workspace_id: Option<WorkspaceId>,
    pub workspace_path: String,
    pub check_interruption: Option<qq_protocol::CheckInterruption>,
    pub(crate) workspace_root: Option<std::path::PathBuf>,
    pub sessions: SessionStore,
    pub(crate) panes: Vec<TranscriptPane>,
    pub(crate) focused_pane: usize,
    view_return: Option<SessionId>,
    focus_clock: u64,
    pub overlay: Option<Overlay>,
    pub composer: Composer,
    pub(crate) approval_amendment: Option<ApprovalChoice>,
    pub(crate) question_answers: Vec<String>,
    history_position: Option<usize>,
    history_draft: Option<String>,
    slash: Picker,
    pub(crate) mention: MentionCompletion,
    resolving: usize,
    next_effect_id: u64,
    pending_resolutions: std::collections::HashSet<u64>,
    pending_prefetch: Option<(SessionId, u64)>,
    esc_armed_at: Option<usize>,
    capabilities: Option<Arc<ServerCapabilities>>,
    shown_host_warning: Option<String>,
    pub connection: ConnectionState,
    pub status: Option<String>,
    status_session_id: Option<SessionId>,
    pub(crate) status_level: NoticeLevel,
    status_ticks_left: u16,
    pub animation_tick: usize,
    pub(crate) now_ms: u64,
    pub(crate) goal_now_ms: u64,
    pub(crate) goal_defaults: GoalDefaults,
    pub tool_detail: ToolDetail,
    pub(crate) expanded_tool_calls: std::collections::HashSet<qq_protocol::ToolCallId>,
    pub(crate) transcript_cursor: Option<qq_protocol::ToolCallId>,
    pub reasoning_detail: ReasoningDetail,
    pub layout: crate::view::LayoutPrefs,
    pub(crate) mouse_capture: bool,
    terminal_width: usize,
    pub(crate) themes: Vec<Theme>,
    pub(crate) theme: usize,
    pub(crate) theme_generation: u64,
    terminal_focused: bool,
    last_sequence: u64,
    recent_events: VecDeque<SessionEventEnvelope>,
    pending: HashMap<CommandId, PendingIntent>,
    answered_approvals: std::collections::HashSet<qq_protocol::ToolCallId>,
    acknowledged_side_questions: HashMap<SessionId, RunId>,
    startup_submits: VecDeque<QueuedPrompt>,
}

impl App {
    pub(crate) fn new(options: TuiOptions) -> Self {
        Self {
            settings: options.settings,
            model: options.model,
            profile: AgentProfileId::default(),
            approval_mode: ApprovalMode::default(),
            reasoning_effort: None,
            models: options.models,
            unauthenticated_providers: options.unauthenticated_providers,
            environment: options.environment,
            workspace_id: None,
            workspace_path: String::new(),
            check_interruption: None,
            workspace_root: options.workspace_root,
            sessions: SessionStore::with_sanitizer(terminal_safe_character),
            panes: vec![TranscriptPane::default()],
            focused_pane: 0,
            view_return: None,
            focus_clock: 0,
            overlay: None,
            composer: Composer::default(),
            approval_amendment: None,
            question_answers: Vec::new(),
            history_position: None,
            history_draft: None,
            slash: Picker::new(),
            mention: MentionCompletion::default(),
            resolving: 0,
            next_effect_id: 0,
            pending_resolutions: std::collections::HashSet::new(),
            pending_prefetch: None,
            esc_armed_at: None,
            capabilities: None,
            shown_host_warning: None,
            connection: ConnectionState::Connecting,
            status: None,
            status_session_id: None,
            status_level: NoticeLevel::Info,
            status_ticks_left: 0,
            animation_tick: 0,
            now_ms: 0,
            goal_now_ms: 0,
            goal_defaults: options.goals,
            tool_detail: ToolDetail::default(),
            expanded_tool_calls: std::collections::HashSet::new(),
            transcript_cursor: None,
            reasoning_detail: ReasoningDetail::default(),
            layout: crate::view::LayoutPrefs::default(),
            terminal_width: 0,
            mouse_capture: true,
            themes: if options.themes.is_empty() {
                vec![Theme::default()]
            } else {
                options.themes
            },
            theme: 0,
            theme_generation: 0,
            terminal_focused: true,
            last_sequence: 0,
            recent_events: VecDeque::new(),
            pending: HashMap::new(),
            answered_approvals: std::collections::HashSet::new(),
            acknowledged_side_questions: HashMap::new(),
            startup_submits: VecDeque::new(),
        }
    }

    fn apply_startup_options(&mut self, options: TuiOptions) -> Effects {
        self.settings = options.settings;
        self.model = options.model;
        self.models = options.models;
        self.unauthenticated_providers = options.unauthenticated_providers;
        self.environment = options.environment;
        self.workspace_root = options.workspace_root;
        self.goal_defaults = options.goals;
        self.themes = if options.themes.is_empty() {
            vec![Theme::default()]
        } else {
            options.themes
        };
        self.theme = 0;
        self.theme_generation = self.theme_generation.wrapping_add(1);
        Effects::redraw(Redraw::Immediate)
    }

    pub(crate) fn mode(&self) -> Mode {
        match &self.overlay {
            Some(overlay) => overlay.mode(),
            None if self.pending_approval().is_some() => Mode::Approval,
            None => Mode::Compose,
        }
    }

    pub fn note_editor_failure(&mut self, reason: &str) {
        self.set_warning(reason.to_owned());
    }

    pub fn apply_editor_result(&mut self, text: Option<String>) -> bool {
        let Some(text) = text else {
            self.set_info("external editor made no changes".to_owned());
            return true;
        };
        let mut sanitized = String::with_capacity(text.len().min(MAX_INPUT_BYTES));
        for character in text.chars() {
            if sanitized.len() + character.len_utf8() > MAX_INPUT_BYTES {
                break;
            }
            if let Some(character) = composer_character(character) {
                sanitized.push(character);
            }
        }
        let trimmed = sanitized.trim_end().to_owned();
        self.composer.replace(trimmed);
        self.reset_history_browse();
        self.slash.select(0);
        true
    }

    pub fn apply_client_update(&mut self, update: ClientUpdate) -> Effects {
        match update {
            ClientUpdate::Startup(update) => match update.downcast_ref::<TuiOptions>() {
                Some(options) => self.apply_startup_options(options.clone()),
                None => {
                    self.set_warning("client sent incompatible startup options".to_owned());
                    Effects::redraw(Redraw::Scheduled)
                }
            },
            ClientUpdate::Connection(connection) => {
                self.connection = connection;
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            ClientUpdate::ResetSnapshot(snapshot) => {
                self.workspace_id = None;
                self.workspace_path.clear();
                self.sessions.clear();
                for pane in &mut self.panes {
                    pane.view = View::Transcript(None);
                }
                self.overlay = None;
                self.last_sequence = 0;
                self.recent_events.clear();
                self.set_warning("session state reset after reconnecting".to_owned());
                self.apply_snapshot(snapshot)
            }
            ClientUpdate::Models { models, selected } => {
                self.apply_models(models, selected);
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::Capabilities(capabilities) => {
                self.capabilities = Some(capabilities);
                match self.tool_host_warning() {
                    Some(warning) => {
                        if self.shown_host_warning.as_deref() != Some(warning.as_str()) {
                            self.set_warning(warning.clone());
                            self.shown_host_warning = Some(warning);
                        }
                    }
                    None => self.shown_host_warning = None,
                }
                self.refresh_profile_picker();
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::Event(event) => self.apply_live_event(event),
            ClientUpdate::CommandResult { command_id, result } => {
                let failed_form = if result.is_err() {
                    self.finish_goal_form(command_id, false)
                } else {
                    self.finish_goal_form(command_id, true);
                    None
                };
                match result {
                    Ok(receipt) => {
                        let intent = self.pending.remove(&command_id);
                        if let CommandOutcome::SessionCreated { session_id } = receipt.outcome
                            && intent
                                .as_ref()
                                .is_some_and(|intent| matches!(intent, PendingIntent::Create))
                        {
                            self.adopt_created_session(session_id);
                        }
                        if let CommandOutcome::SideQuestionSubmitted {
                            side_question_id, ..
                        } = receipt.outcome
                            && let Some(PendingIntent::Side { session_id, .. }) = intent.as_ref()
                        {
                            self.acknowledge_side_question(*session_id, side_question_id);
                        }
                        if let Some(PendingIntent::Cancel { session_id }) = intent.as_ref() {
                            self.set_info_for(
                                Some(*session_id),
                                "cancellation requested".to_owned(),
                            );
                        }
                        if let CommandOutcome::ToolApprovalResolved { resolution, .. } =
                            receipt.outcome
                        {
                            self.set_info(
                                match resolution {
                                    ApprovalResolution::ApprovedOnce => "tool call approved",
                                    ApprovalResolution::Denied => "tool call denied",
                                    ApprovalResolution::DeniedTimeout => {
                                        "tool call already denied by timeout"
                                    }
                                    ApprovalResolution::Answered => "answer sent",
                                }
                                .to_owned(),
                            );
                        }
                        let cancel_side_session = match intent.as_ref() {
                            Some(PendingIntent::CancelSide { session_id }) => Some(*session_id),
                            _ => None,
                        };
                        if matches!(receipt.outcome, CommandOutcome::RunAlreadyFinished { .. }) {
                            if let Some(PendingIntent::Steer { session_id, text }) = intent
                                && self.focused() == Some(session_id)
                                && self.composer.text.is_empty()
                            {
                                self.composer.replace(text);
                                self.set_warning(
                                    "run finished before it could be steered; draft restored"
                                        .to_owned(),
                                );
                            } else {
                                self.set_warning("run already finished".to_owned());
                            }
                        }
                        match &receipt.outcome {
                            CommandOutcome::SideQuestionCancelled { side_question_id } => {
                                self.acknowledged_side_questions
                                    .retain(|_, id| id != side_question_id);
                            }
                            CommandOutcome::SideQuestionAlreadyFinished {
                                side_question_id,
                                ..
                            } => {
                                self.acknowledged_side_questions
                                    .retain(|_, id| id != side_question_id);
                                self.set_info_for(
                                    cancel_side_session,
                                    "side question already finished".to_owned(),
                                );
                            }
                            CommandOutcome::CompactionQueued { session_id, .. } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    "compacting session...".to_owned(),
                                );
                            }
                            CommandOutcome::SessionModelSet { session_id, model } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    format!(
                                        "session model set to {}",
                                        model.model.as_deref().unwrap_or("default")
                                    ),
                                );
                            }
                            CommandOutcome::CompactionRolledBack {
                                session_id,
                                remaining: 0,
                            } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    "compaction rolled back; full history restored".to_owned(),
                                );
                            }
                            CommandOutcome::CompactionRolledBack {
                                session_id,
                                remaining,
                            } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    format!("compaction rolled back; {remaining} earlier retained"),
                                );
                            }
                            CommandOutcome::ApprovalModeSet { session_id, mode } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    format!(
                                        "session approval mode set to {}",
                                        approval_mode_label(*mode)
                                    ),
                                );
                            }
                            CommandOutcome::SessionProfileSet {
                                session_id,
                                profile,
                            } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    format!("session profile set to {}", profile.as_str()),
                                );
                            }
                            CommandOutcome::SessionEffortSet { session_id, effort } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    format!("session effort set to {}", effort_label(*effort)),
                                );
                            }
                            CommandOutcome::SessionDeleted { .. } => {
                                self.set_warning("session deleted".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted: 0, .. } => {
                                self.set_warning("no empty sessions to delete".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted: 1, .. } => {
                                self.set_warning("deleted 1 empty session".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted, .. } => {
                                self.set_warning(format!("deleted {deleted} empty sessions"));
                            }
                            CommandOutcome::WorkspaceUndone {
                                before,
                                changes_total,
                                head_moved,
                                ..
                            } => {
                                let files = match changes_total {
                                    0 => "nothing had changed".to_owned(),
                                    1 => "restored 1 file".to_owned(),
                                    total => format!("restored {total} files"),
                                };
                                let head = if *head_moved {
                                    "; HEAD moved since, so only files were restored"
                                } else {
                                    ""
                                };
                                self.set_warning(format!(
                                    "{files}{head}; qq undo {before} reverses it"
                                ));
                            }
                            _ => {}
                        }
                    }
                    Err(error) => self.reject_pending(command_id, error),
                }
                let mut effects = Effects::redraw(Redraw::Scheduled);
                if let (Some(session), Some(workspace)) = (failed_form, self.workspace_id) {
                    effects.extend(Effects::send_now(ClientRequest::Snapshot(body_request(
                        workspace, session,
                    ))));
                }
                effects
            }
            ClientUpdate::SnapshotFailed(error) => {
                if let Some(Overlay::GoalForm(form)) = &mut self.overlay
                    && form.refresh == crate::input::GoalFormRefresh::Pending
                {
                    form.refresh = crate::input::GoalFormRefresh::Failed;
                    form.error = Some(
                        "Refresh failed. Enter retries the refresh; your draft is retained."
                            .to_owned(),
                    );
                }
                self.set_warning(error.message().to_owned());
                Effects::redraw(Redraw::Scheduled)
            }
        }
    }

    fn apply_snapshot(&mut self, mut snapshot: WorkspaceSnapshot) -> Effects {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            self.set_warning("server returned a snapshot for another workspace".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        let body_is_current = |body: &qq_protocol::SessionSnapshot| {
            self.sessions
                .get(&body.summary.id)
                .is_none_or(|view| view.loaded_through <= snapshot.cursor.sequence)
        };
        snapshot.included.retain(body_is_current);
        if snapshot
            .focused
            .as_ref()
            .is_some_and(|body| !body_is_current(body))
        {
            if let Some(Overlay::GoalForm(form)) = &mut self.overlay
                && snapshot
                    .focused
                    .as_ref()
                    .is_some_and(|body| body.summary.id == form.session)
            {
                form.refresh = crate::input::GoalFormRefresh::Ready;
            }
            snapshot.focused = None;
        }
        let snapshot_focus = snapshot.focused.as_ref().map(|focused| focused.summary.id);

        let may_refetch_focused = initial || !snapshot.included.is_empty();
        let installed_sessions: Vec<_> = snapshot
            .included
            .iter()
            .map(|body| body.summary.id)
            .chain(snapshot_focus)
            .collect();
        if !initial
            && self.focused().is_some()
            && snapshot_focus.is_some_and(|id| self.focused() != Some(id))
        {
            return Effects::none();
        }
        if snapshot.cursor.sequence < self.last_sequence
            && self
                .recent_events
                .front()
                .is_none_or(|event| event.cursor.sequence > snapshot.cursor.sequence + 1)
        {
            self.set_warning("snapshot was too stale; reconnecting is required".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }

        let mut rewarmed_sessions = installed_sessions
            .iter()
            .copied()
            .filter(|session| {
                self.sessions
                    .get(session)
                    .is_none_or(|view| !view.is_warm())
            })
            .collect::<Vec<_>>();
        let snapshot_sequence = snapshot.cursor.sequence;
        if initial {
            self.workspace_id = Some(snapshot.workspace.id);
            self.workspace_path = snapshot.workspace.path;
        }
        if initial || snapshot_sequence >= self.last_sequence {
            self.update_check_interruption(snapshot.check_interruption);
            for summary in snapshot.sessions {
                if self
                    .sessions
                    .get(&summary.id)
                    .is_some_and(|view| view.loaded_through > snapshot_sequence)
                {
                    continue;
                }
                self.sessions
                    .upsert_summary(summary, &self.models, snapshot_sequence);
            }
        }
        for body in snapshot.included {
            self.sessions
                .install_session_snapshot(body, &self.models, snapshot_sequence);
        }
        if let Some(focused) = snapshot.focused {
            let focused_id = focused.summary.id;
            if let Some(Overlay::GoalForm(form)) = &mut self.overlay
                && form.session == focused_id
            {
                form.refresh = crate::input::GoalFormRefresh::Ready;
            }
            if may_refetch_focused {
                self.sessions
                    .install_session_snapshot(focused, &self.models, snapshot_sequence);
            } else {
                self.sessions.install_isolated_session_snapshot(
                    focused,
                    &self.models,
                    snapshot_sequence,
                );
            }
            let side_view =
                matches!(self.view(), View::SideQuestions(id) | View::Goal(id) if id == focused_id);
            if !side_view
                && (initial || self.focused().is_none() || self.focused() == Some(focused_id))
            {
                self.set_focus(focused_id);
            } else {
                self.set_focus_clock(focused_id);
            }
        } else if self.focused().is_none()
            && let Some(first) = self.sessions.roots().first().copied()
        {
            self.set_focus(first);
        }
        self.evict_cold_bodies();
        if initial {
            self.last_sequence = snapshot_sequence;
        }
        let replay = self
            .recent_events
            .iter()
            .filter(|event| {
                event.cursor.sequence > snapshot_sequence
                    && installed_sessions.contains(&event.session_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut effects = Effects::redraw(Redraw::Scheduled);
        if may_refetch_focused
            && let Some(session_id) = snapshot_focus
            && self.focused() == Some(session_id)
            && self
                .sessions
                .get(&session_id)
                .is_some_and(|view| !view.is_warm())
            && let Some(workspace_id) = self.workspace_id
        {
            effects.push(Effect::Send(ClientRequest::Snapshot(body_request(
                workspace_id,
                session_id,
            ))));
        }
        for event in replay {
            let replayed = self.sessions.replay_event(
                &event,
                ReduceContext {
                    focused: self.view().transcript(),
                    attentive: self.terminal_focused,
                    workspace_id: self.workspace_id,
                    capabilities: self.capabilities.as_ref(),
                    models: &self.models,
                    caused_by_me: false,
                },
            );
            for effect in replayed {
                match effect {
                    StateEffect::RequestSnapshot(request) => {
                        if matches!(event.event, SessionEvent::SessionDeleted { .. }) {
                            effects.push(Effect::Send(ClientRequest::Snapshot(request)));
                        } else if let Some(index) = rewarmed_sessions
                            .iter()
                            .position(|session| *session == event.session_id)
                        {
                            rewarmed_sessions.swap_remove(index);
                            effects.push(Effect::Send(ClientRequest::Snapshot(request)));
                        }
                    }
                    structural => {
                        effects.extend(self.apply_state_effects(std::iter::once(structural)))
                    }
                }
            }
            if matches!(event.event, SessionEvent::SessionDeleted { .. }) {
                self.refresh_session_picker();
            }
        }
        effects.extend(self.replay_queued_prompts());
        effects
    }

    fn reduce_event(&mut self, envelope: &SessionEventEnvelope) -> Effects {
        let caused_by_me = envelope
            .caused_by
            .and_then(|id| self.pending.get(&id))
            .is_some_and(|intent| matches!(intent, PendingIntent::Create));
        let reduced = self.sessions.reduce_event(
            envelope,
            ReduceContext {
                focused: self.view().transcript(),
                attentive: self.terminal_focused,
                workspace_id: self.workspace_id,
                capabilities: self.capabilities.as_ref(),
                models: &self.models,
                caused_by_me,
            },
        );
        let effects = self.apply_state_effects(reduced);
        if let SessionEvent::SideQuestionUpdated { side_question } = &envelope.event
            && side_question.state != SideQuestionState::Running
            && self
                .acknowledged_side_questions
                .get(&side_question.session_id)
                == Some(&side_question.id)
        {
            self.acknowledged_side_questions
                .remove(&side_question.session_id);
        }
        if matches!(
            envelope.event,
            SessionEvent::SessionCreated { .. }
                | SessionEvent::SessionUpdated { .. }
                | SessionEvent::SessionDeleted { .. }
                | SessionEvent::PromptQueued { .. }
                | SessionEvent::RunStarted { .. }
                | SessionEvent::CancellationRequested { .. }
                | SessionEvent::SessionCompacted { .. }
                | SessionEvent::SessionCompactionRolledBack { .. }
                | SessionEvent::RunFinished { .. }
        ) {
            self.refresh_session_picker();
        }
        effects
    }

    fn update_check_interruption(&mut self, interruption: Option<qq_protocol::CheckInterruption>) {
        let changed = self.check_interruption != interruption;
        self.check_interruption = interruption;
        if changed {
            if self.check_interruption.is_some() {
                self.set_warning(
                    "An interrupted completion check holds this workspace until acknowledgment."
                        .to_owned(),
                );
            } else {
                self.set_info(
                    "Completion check interruption acknowledged; workspace work may continue."
                        .to_owned(),
                );
            }
        }
    }

    fn apply_state_effects(&mut self, reduced: impl IntoIterator<Item = StateEffect>) -> Effects {
        let mut effects = Effects::none();
        for effect in reduced {
            match effect {
                StateEffect::CheckInterruptionChanged(interruption) => {
                    self.update_check_interruption(interruption)
                }
                StateEffect::Notice {
                    session,
                    level,
                    text,
                } => self.apply_notice(session, level, text),
                StateEffect::Attention(attention) => effects.push(Effect::Attention(attention)),
                StateEffect::RequestSnapshot(request) => {
                    effects.push(Effect::Send(ClientRequest::Snapshot(request)));
                }
                StateEffect::Refocus(target) => self.set_view(View::Transcript(target)),
                StateEffect::AdoptCreated(session_id) => self.adopt_created_session(session_id),
                StateEffect::SubmitDraft { session_id, text } => {
                    effects.extend(self.submit_text(session_id, text));
                }
                StateEffect::SessionRemoved {
                    session_id,
                    tool_call_ids,
                } => {
                    self.acknowledged_side_questions.remove(&session_id);
                    if matches!(self.view(), View::SideQuestions(id) | View::Goal(id) if id == session_id)
                    {
                        effects.extend(self.leave_workspace_view());
                    }
                    for call in tool_call_ids {
                        self.answered_approvals.remove(&call);
                    }
                    self.pending.retain(|_, intent| {
                        !matches!(
                            intent,
                            PendingIntent::Prompt { session_id: target, .. } if *target == session_id
                        )
                    });
                    if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay
                        && matches!(confirm, Some(SessionConfirm::Delete(pending)) if *pending == session_id)
                    {
                        *confirm = None;
                    }
                }
            }
        }
        effects
    }

    pub(super) fn adopt_created_session(&mut self, session_id: SessionId) {
        self.sessions.warm_empty(session_id);
        self.set_focus(session_id);
        self.reset_history_browse();
        self.evict_cold_bodies();
    }

    fn set_focus(&mut self, session_id: SessionId) {
        self.set_view(View::Transcript(Some(session_id)));
        self.set_focus_clock(session_id);
    }

    fn set_focus_clock(&mut self, session_id: SessionId) {
        self.focus_clock += 1;
        self.sessions.mark_focused(session_id, self.focus_clock);
    }

    fn evict_cold_bodies(&mut self) {
        self.sessions.evict_cold_bodies(self.focused());
    }

    fn apply_live_event(&mut self, event: SessionEventEnvelope) -> Effects {
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != event.cursor.workspace_id)
        {
            self.set_warning("server sent an event for another workspace".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        if event.cursor.sequence <= self.last_sequence {
            return Effects::none();
        }
        if self.last_sequence != 0 && event.cursor.sequence != self.last_sequence + 1 {
            self.connection = ConnectionState::Replaying;
            self.set_warning("session event gap detected".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        self.workspace_id.get_or_insert(event.cursor.workspace_id);
        self.last_sequence = event.cursor.sequence;
        self.now_ms = self.now_ms.max(event.occurred_at_ms);
        let already_loaded = self
            .sessions
            .get(&event.session_id)
            .is_some_and(|session| event.cursor.sequence <= session.loaded_through);
        let mut effects = Effects::changed(self.event_is_visible(&event));
        if !already_loaded {
            effects.extend(self.reduce_event(&event));
        }
        if let Some(command_id) = event.caused_by {
            if matches!(event.event, SessionEvent::GoalUpdated { .. }) {
                self.finish_goal_form(command_id, true);
            }
            self.pending.remove(&command_id);
        }
        self.recent_events.push_back(event);
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
        effects
    }

    fn event_is_visible(&self, event: &SessionEventEnvelope) -> bool {
        let background_only = matches!(
            event.event,
            SessionEvent::TextAppended { .. }
                | SessionEvent::ReasoningDelta { .. }
                | SessionEvent::ToolCallOutputDelta { .. }
                | SessionEvent::RunActivityChanged { .. }
        );
        if !background_only {
            return true;
        }
        if self.focused() == Some(event.session_id) {
            return true;
        }
        if self
            .sessions
            .get(&event.session_id)
            .and_then(|session| session.summary.parent_id)
            .is_some_and(|parent| self.focused() == Some(parent))
        {
            return true;
        }
        self.terminal_width == 0
            || crate::view::rail_visible(self.terminal_width, self.layout, self.sessions.len())
    }

    fn set_notice_for(&mut self, session_id: Option<SessionId>, text: String, level: NoticeLevel) {
        self.status = Some(text);
        self.status_session_id = session_id;
        self.status_level = level;
        self.status_ticks_left = match level {
            NoticeLevel::Error => 0,
            NoticeLevel::Info | NoticeLevel::Warning => NOTICE_TICKS,
        };
    }

    fn set_notice(&mut self, text: String, level: NoticeLevel) {
        self.set_notice_for(self.focused(), text, level);
    }

    pub(crate) fn apply_notice(
        &mut self,
        session: Option<SessionId>,
        level: NoticeLevel,
        text: String,
    ) {
        self.set_notice_for(session.or_else(|| self.focused()), text, level);
    }

    fn set_info_for(&mut self, session_id: Option<SessionId>, text: String) {
        self.set_notice_for(session_id, text, NoticeLevel::Info);
    }

    fn set_info(&mut self, text: String) {
        self.set_notice(text, NoticeLevel::Info);
    }

    fn set_warning(&mut self, text: String) {
        self.set_notice(text, NoticeLevel::Warning);
    }

    fn set_error_for(&mut self, session_id: Option<SessionId>, text: String) {
        self.set_notice_for(session_id, text, NoticeLevel::Error);
    }

    pub(crate) fn visible_status(&self) -> Option<(&str, NoticeLevel)> {
        if self.status_session_id != self.focused() {
            return None;
        }
        self.status.as_deref().map(|text| (text, self.status_level))
    }

    pub(crate) fn configured_provider_remedy(&self) -> Option<&ProviderRemedy> {
        let provider = self.model.model.as_deref()?.split_once('/')?.0;
        self.unauthenticated_providers
            .iter()
            .find(|remedy| remedy.provider == provider)
    }

    pub(crate) fn startup_guidance(&self) -> Option<String> {
        if let Some(remedy) = self.configured_provider_remedy() {
            return Some(remedy.message());
        }
        if self.model.model.is_none() {
            return Some(CHOOSE_MODEL_NOTICE.to_owned());
        }
        None
    }

    fn expire_status(&mut self) -> bool {
        if self.status.is_none() || self.status_ticks_left == 0 {
            return false;
        }
        self.status_ticks_left -= 1;
        if self.status_ticks_left == 0 {
            self.status = None;
            return true;
        }
        false
    }

    fn reject_pending(&mut self, command_id: CommandId, error: ClientFailure) {
        let intent = self.pending.remove(&command_id);
        let status_session_id = match &intent {
            Some(PendingIntent::GoalSet { session_id })
            | Some(PendingIntent::GoalControl { session_id })
            | Some(PendingIntent::Side { session_id, .. })
            | Some(PendingIntent::CancelSide { session_id })
            | Some(PendingIntent::Prompt { session_id, .. })
            | Some(PendingIntent::Cancel { session_id })
            | Some(PendingIntent::Steer { session_id, .. })
            | Some(PendingIntent::Compact { session_id })
            | Some(PendingIntent::Rollback { session_id })
            | Some(PendingIntent::SetModel { session_id })
            | Some(PendingIntent::SetProfile { session_id })
            | Some(PendingIntent::SetApprovalMode { session_id })
            | Some(PendingIntent::SetEffort { session_id })
            | Some(PendingIntent::Delete { session_id }) => Some(*session_id),
            Some(PendingIntent::Approval { tool_call_id }) => self
                .sessions
                .values()
                .flat_map(|session| session.tool_calls.iter().flatten())
                .find(|tool_call| tool_call.id == *tool_call_id)
                .map(|tool_call| tool_call.session_id),
            Some(PendingIntent::Create | PendingIntent::Prune | PendingIntent::Undo) | None => {
                self.focused()
            }
        };
        match intent {
            Some(PendingIntent::Side { session_id, text })
            | Some(PendingIntent::Prompt { session_id, text })
            | Some(PendingIntent::Steer { session_id, text })
                if self.focused() == Some(session_id) && self.composer.text.is_empty() =>
            {
                self.composer.replace(text);
            }
            Some(PendingIntent::Approval { tool_call_id }) => {
                self.answered_approvals.remove(&tool_call_id);
            }
            _ => {}
        }
        self.set_error_for(status_session_id, error.message().to_owned());
    }

    pub fn handle_terminal_event(&mut self, event: Event) -> Effects {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Paste(text) => {
                let changed = match &mut self.overlay {
                    Some(overlay) => overlay.push_query(&text),
                    None => self.push_composer_text(&text),
                };
                Effects::changed_now(changed)
            }
            Event::Mouse(mouse) if self.overlay.is_none() => {
                let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(isize::MAX);
                let changed = match mouse.kind {
                    MouseEventKind::ScrollUp => self.viewport_mut().scroll(rows),
                    MouseEventKind::ScrollDown => self.viewport_mut().scroll(-rows),
                    _ => false,
                };
                Effects::changed_now(changed)
            }
            Event::FocusGained => {
                self.terminal_focused = true;
                Effects::redraw(Redraw::Immediate)
            }
            Event::FocusLost => {
                self.terminal_focused = false;
                Effects::redraw(Redraw::Immediate)
            }
            Event::Resize(columns, _) => {
                self.terminal_width = usize::from(columns);
                Effects::redraw(Redraw::Immediate)
            }
            Event::Key(_) | Event::Mouse(_) => Effects::none(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Effects {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.execute(Command::Quit);
        }
        match self.mode() {
            Mode::GoalForm
            | Mode::GoalCheck
            | Mode::Sessions
            | Mode::Models
            | Mode::Profiles
            | Mode::ApprovalModes
            | Mode::Effort
            | Mode::Skills
            | Mode::Themes
            | Mode::Commands
            | Mode::History => self.handle_overlay_key(key),
            Mode::Approval => self.handle_approval_key(key),
            Mode::Compose => self.handle_compose_key(key),
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> Effects {
        if is_composer_newline_key(key) {
            let changed = self.push_input('\n');
            return Effects::changed_now(changed);
        }
        if key.code != KeyCode::Esc {
            self.esc_armed_at = None;
        }
        if let Some(result) = self.handle_mention_key(key.code) {
            return result;
        }
        if let Some(result) = self.handle_slash_key(key.code) {
            return result;
        }
        if key.code == KeyCode::Char('?') && self.composer.text.is_empty() {
            return self.execute(Command::OpenHelp);
        }
        if let Some(command) = commands::command_for_key(&self.settings, key) {
            return self.execute(command);
        }
        match key.code {
            KeyCode::Esc => {
                if self.transcript_cursor.take().is_some() {
                    return Effects::redraw(Redraw::Immediate);
                }
                if matches!(
                    self.view(),
                    View::Attention | View::Changes | View::SideQuestions(_) | View::Goal(_)
                ) {
                    return self.leave_workspace_view();
                }
                if self.status.is_some()
                    && self.status_level == NoticeLevel::Error
                    && self.status_session_id == self.focused()
                {
                    self.status = None;
                    return Effects::redraw(Redraw::Immediate);
                }
                let running = self
                    .focused()
                    .and_then(|id| self.sessions.get(&id))
                    .is_some_and(|session| session.summary.active_run_id.is_some());
                if running {
                    let now = self.animation_tick;
                    if self
                        .esc_armed_at
                        .is_some_and(|armed| now.wrapping_sub(armed) <= ESC_CANCEL_TICKS)
                    {
                        self.esc_armed_at = None;
                        return self.cancel_run();
                    }
                    self.esc_armed_at = Some(now);
                    self.set_info("press Esc again to cancel the run".to_owned());
                    return Effects::redraw(Redraw::Immediate);
                }
                if let Some(parent) = self
                    .focused()
                    .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
                {
                    return self.focus_session(parent);
                }
                Effects::none()
            }
            KeyCode::Enter if self.transcript_cursor.is_some() && self.composer.text.is_empty() => {
                let Some(call) = self.transcript_cursor else {
                    return Effects::none();
                };
                if let Some(child) = self.sessions.child_spawned_by(call) {
                    self.transcript_cursor = None;
                    return self.focus_session(child);
                }
                if !self.expanded_tool_calls.remove(&call) {
                    if self.expanded_tool_calls.len() >= MAX_EXPANDED_TOOL_CALLS {
                        let live: std::collections::HashSet<_> = self
                            .sessions
                            .values()
                            .filter_map(|session| session.tool_calls.as_deref())
                            .flatten()
                            .map(|tool_call| tool_call.id)
                            .collect();
                        self.expanded_tool_calls.retain(|id| live.contains(id));
                        if self.expanded_tool_calls.len() >= MAX_EXPANDED_TOOL_CALLS {
                            self.expanded_tool_calls.clear();
                        }
                    }
                    self.expanded_tool_calls.insert(call);
                }
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::PageUp => Effects::changed_now(self.scroll_focused_page(true)),
            KeyCode::PageDown => Effects::changed_now(self.scroll_focused_page(false)),
            KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
                let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(isize::MAX);
                Effects::changed_now(self.viewport_mut().scroll(rows))
            }
            KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
                let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(isize::MAX);
                Effects::changed_now(self.viewport_mut().scroll(-rows))
            }
            KeyCode::Backspace
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                let changed = self.composer.kill_word_back();
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                Effects::changed_now(changed)
            }
            KeyCode::Backspace => {
                let changed = self.composer.backspace();
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                    self.mention.invalidate();
                    if self.composer.mention_token().is_some() {
                        return self.request_mention_completion();
                    }
                }
                Effects::changed_now(changed)
            }
            KeyCode::Delete => {
                let changed = self.composer.delete();
                if changed {
                    self.reset_history_browse();
                }
                Effects::changed_now(changed)
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Effects::changed_now(self.composer.move_word_left())
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Effects::changed_now(self.composer.move_word_right())
            }
            KeyCode::Left => Effects::changed_now(self.composer.move_left()),
            KeyCode::Right => Effects::changed_now(self.composer.move_right()),
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Effects::changed_now(self.viewport_mut().scroll(isize::MAX))
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Effects::changed_now(self.viewport_mut().scroll(isize::MIN))
            }
            KeyCode::Home => Effects::changed_now(self.composer.move_line_start()),
            KeyCode::End => Effects::changed_now(self.composer.move_line_end()),
            KeyCode::Char(character) if key.modifiers == KeyModifiers::CONTROL => {
                let changed = match character.to_ascii_lowercase() {
                    'a' => self.composer.move_line_start(),
                    'e' => self.composer.move_line_end(),
                    'w' => self.composer.kill_word_back(),
                    'k' => self.composer.kill_to_line_end(),
                    'u' => self.composer.kill_to_line_start(),
                    'y' => self.composer.yank(),
                    'z' | '_' => self.composer.undo(),
                    _ => return Effects::none(),
                };
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                Effects::changed_now(changed)
            }
            KeyCode::Up => {
                let changed = self.composer.move_up() || self.browse_prompt_history(false);
                Effects::changed_now(changed)
            }
            KeyCode::Down => {
                let changed = self.composer.move_down() || self.browse_prompt_history(true);
                Effects::changed_now(changed)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let changed = self.push_input(character);
                if changed && self.composer.mention_token().is_some() {
                    return self.request_mention_completion();
                }
                Effects::changed_now(changed)
            }
            _ => Effects::none(),
        }
    }

    pub(crate) fn focused(&self) -> Option<SessionId> {
        self.view().session()
    }

    pub(crate) fn pane(&self) -> &TranscriptPane {
        &self.panes[self.focused_pane.min(self.panes.len() - 1)]
    }

    fn pane_mut(&mut self) -> &mut TranscriptPane {
        let index = self.focused_pane.min(self.panes.len() - 1);
        &mut self.panes[index]
    }

    pub(crate) fn view(&self) -> View {
        self.pane().view
    }

    pub(crate) fn view_return(&self) -> Option<SessionId> {
        self.view_return
    }

    pub(crate) fn set_view(&mut self, view: View) {
        self.pane_mut().view = view;
    }

    pub(crate) fn viewport(&self) -> &Viewport {
        &self.pane().viewport
    }

    fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.pane_mut().viewport
    }

    fn steering(&self) -> Option<SteeringCapabilities> {
        self.capabilities
            .as_deref()
            .map(|capabilities| capabilities.steering)
    }

    fn show_workspace_view(&mut self, view: View) -> Effects {
        if self.view() == view {
            return self.leave_workspace_view();
        }
        if let Some(session) = self.focused() {
            self.view_return = Some(session);
        }
        self.set_view(view);
        Effects::redraw(Redraw::Immediate)
    }

    fn leave_workspace_view(&mut self) -> Effects {
        let target = self
            .view_return
            .take()
            .filter(|id| self.sessions.contains_key(id))
            .or_else(|| self.sessions.thread_order().first().copied());
        match target {
            Some(session) => self.focus_session(session),
            None => {
                self.set_view(View::Transcript(None));
                Effects::redraw(Redraw::Immediate)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn update_transcript_viewport(
        &mut self,
        body_rows: usize,
        height: usize,
        preserve_tail_anchor: bool,
    ) {
        let view = self.view();
        self.viewport_mut()
            .update(view, body_rows, height, preserve_tail_anchor);
    }

    #[cfg(test)]
    pub(crate) fn transcript_scroll_offset(&self) -> usize {
        self.viewport().offset()
    }

    fn scroll_focused_page(&mut self, up: bool) -> bool {
        let page = isize::try_from(self.viewport().height()).unwrap_or(isize::MAX);
        self.viewport_mut().scroll(if up { page } else { -page })
    }

    pub(crate) fn execute(&mut self, command: Command) -> Effects {
        match command {
            Command::ShowGoal => match self.focused().or(self.view_return) {
                Some(session_id) => {
                    let mut effects = self.show_workspace_view(View::Goal(session_id));
                    if self
                        .sessions
                        .get(&session_id)
                        .is_some_and(|view| !view.is_warm())
                        && let Some(workspace_id) = self.workspace_id
                    {
                        effects.push(Effect::Send(ClientRequest::Snapshot(body_request(
                            workspace_id,
                            session_id,
                        ))));
                    }
                    effects
                }
                None => {
                    self.set_warning("create a session before viewing its goal".to_owned());
                    Effects::redraw(Redraw::Immediate)
                }
            },
            Command::ShowSideQuestions => match self.focused().or(self.view_return) {
                Some(session_id) => self.show_workspace_view(View::SideQuestions(session_id)),
                None => {
                    self.set_warning("create a session before asking a side question".to_owned());
                    Effects::redraw(Redraw::Immediate)
                }
            },
            Command::NewSideThread => {
                self.composer.replace("/btw-new ".to_owned());
                Effects::redraw(Redraw::Immediate)
            }
            Command::CancelSideQuestion => self.cancel_side_question(),
            Command::OpenHelp => self.open_commands(true),
            Command::OpenCommands => self.open_commands(false),
            Command::SearchHistory => self.open_history(),
            Command::OpenModels => self.open_models(),
            Command::OpenProfiles => self.open_profiles(),
            Command::OpenApprovalModes => self.open_approval_modes(),
            Command::OpenEffort => self.open_effort(),
            Command::OpenSkills => self.open_skills(),
            Command::OpenThemes => self.open_themes(),
            Command::OpenSessions => self.open_sessions(),
            Command::OpenAgents => self.open_agents(),
            Command::ToggleSessions => {
                if matches!(self.overlay, Some(Overlay::Sessions { .. })) {
                    self.overlay = None;
                    self.cancel_prefetch();
                    Effects::redraw(Redraw::Immediate)
                } else {
                    self.open_sessions()
                }
            }
            Command::NewRootSession => self.create_session(None),
            Command::NewChildSession => self.create_session(self.focused()),
            Command::CompactSession => self.compact_session(),
            Command::RollbackCompaction => self.rollback_compaction(),
            Command::CancelRun => self.cancel_run(),
            Command::ToggleMouse => {
                self.mouse_capture = !self.mouse_capture;
                self.set_info(if self.mouse_capture {
                    "mouse on: wheel scrolls, click focuses; hold Shift to select text".to_owned()
                } else {
                    "mouse off: terminal selection works; PageUp/PageDown scroll".to_owned()
                });
                let mut effects = Effects::redraw(Redraw::Immediate);
                effects.push(Effect::MouseCapture(self.mouse_capture));
                effects
            }
            Command::PruneSessions => self.request_prune_confirmation(),
            Command::UndoWorkspace => self.undo_workspace(),
            Command::ShowAttention => self.show_workspace_view(View::Attention),
            Command::ShowChanges => self.show_workspace_view(View::Changes),
            Command::CursorUp => Effects::changed_now(self.move_transcript_cursor(false)),
            Command::CursorDown => Effects::changed_now(self.move_transcript_cursor(true)),
            Command::ToggleToolDetail => {
                self.tool_detail = self.tool_detail.next();
                Effects::redraw(Redraw::Immediate)
            }
            Command::ToggleReasoning => {
                self.reasoning_detail = match self.reasoning_detail {
                    ReasoningDetail::Collapsed => ReasoningDetail::Expanded,
                    ReasoningDetail::Expanded => ReasoningDetail::Collapsed,
                };
                Effects::redraw(Redraw::Immediate)
            }
            Command::ToggleSidebar => {
                self.layout.rail = self.layout.rail.toggled();
                Effects::redraw(Redraw::Immediate)
            }
            Command::ToggleInspector => {
                self.layout.inspector = self.layout.inspector.toggled();
                Effects::redraw(Redraw::Immediate)
            }
            Command::FocusParent => match self
                .focused()
                .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
            {
                Some(parent) => self.focus_session(parent),
                None => Effects::none(),
            },
            Command::FocusFirstChild => match self
                .focused()
                .and_then(|focused| self.sessions.children_of(focused).first().copied())
            {
                Some(child) => self.focus_session(child),
                None => Effects::none(),
            },
            Command::FocusNextSibling => match self.sibling(1) {
                Some(sibling) => self.focus_session(sibling),
                None => Effects::none(),
            },
            Command::FocusPreviousSibling => match self.sibling(-1) {
                Some(sibling) => self.focus_session(sibling),
                None => Effects::none(),
            },
            Command::OpenEditor => {
                let mut effects = Effects::redraw(Redraw::Immediate);
                effects.push(Effect::Editor(self.composer.expanded()));
                effects
            }
            Command::QueueDraft => self.queue_draft(),
            Command::DequeueDraft => self.dequeue_draft(),
            Command::SteerRun => {
                if self.steering().is_some_and(|steering| steering.boundary) {
                    return self.steer_run(false);
                }
                self.set_warning(
                    "this server does not support steering; the draft was queued instead"
                        .to_owned(),
                );
                self.queue_draft()
            }
            Command::InterruptRun => {
                if self.steering().is_some_and(|steering| steering.interrupt) {
                    return self.steer_run(true);
                }
                self.set_warning(
                    "this server does not support interrupting a run; the draft was queued instead"
                        .to_owned(),
                );
                self.queue_draft()
            }
            Command::ApproveBackground => self.respond_to_background_approval(true),
            Command::DenyBackground => self.respond_to_background_approval(false),
            Command::FocusNextApproval => match self.next_session_needing_attention() {
                Some(session_id) => self.focus_session(session_id),
                None => {
                    self.set_info("no session needs you right now".to_owned());
                    Effects::redraw(Redraw::Immediate)
                }
            },
            Command::Quit => {
                let mut effects = Effects::redraw(Redraw::Immediate);
                effects.push(Effect::Quit);
                effects
            }
        }
    }

    fn send(&mut self, intent: PendingIntent, command: SessionCommand) -> Effects {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        if matches!(&intent, PendingIntent::GoalSet { .. })
            && let Some(Overlay::GoalForm(form)) = &mut self.overlay
        {
            form.sending = Some(command_id);
        }
        self.pending.insert(command_id, intent);
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command,
        }))
    }

    fn set_session_model(&mut self, session_id: SessionId, mut model: ModelSelection) -> Effects {
        model.model_is_fallback = false;
        self.model = model.clone();
        self.send(
            PendingIntent::SetModel { session_id },
            SessionCommand::SetSessionModel { session_id, model },
        )
    }

    pub(crate) fn focus_session(&mut self, session_id: SessionId) -> Effects {
        self.set_focus(session_id);
        self.reset_history_browse();
        self.evict_cold_bodies();
        if self
            .sessions
            .get(&session_id)
            .is_some_and(SessionView::is_warm)
        {
            return Effects::redraw(Redraw::Immediate);
        }
        let Some(workspace_id) = self.workspace_id else {
            return Effects::redraw(Redraw::Immediate);
        };
        Effects::send_now(ClientRequest::Snapshot(body_request(
            workspace_id,
            session_id,
        )))
    }

    fn create_session(&mut self, parent_id: Option<SessionId>) -> Effects {
        let model = self.model_for_new_session();
        self.create_session_with_model(parent_id, model)
    }

    fn model_for_new_session(&self) -> ModelSelection {
        if self.model.model.as_deref().is_some_and(valid_model_route) {
            return self.model.clone();
        }

        let Some(route) = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .and_then(|session| session.summary.model.as_deref())
            .filter(|route| valid_model_route(route))
        else {
            return self.model.clone();
        };

        let model_is_fallback = self
            .focused()
            .and_then(|id| self.sessions.get(&id))
            .is_some_and(|session| session.summary.model_is_fallback);
        let mut selection = self
            .models
            .iter()
            .find(|option| option.selection.model.as_deref() == Some(route))
            .map(|option| option.selection.clone())
            .unwrap_or_else(|| ModelSelection {
                model_is_fallback: false,
                model: Some(route.to_owned()),
                ..ModelSelection::default()
            });
        selection.model_is_fallback = model_is_fallback;
        selection
    }

    fn create_session_with_model(
        &mut self,
        parent_id: Option<SessionId>,
        model: ModelSelection,
    ) -> Effects {
        let Some(route) = model
            .model
            .as_deref()
            .filter(|route| valid_model_route(route))
        else {
            self.set_warning("choose a model with /models before creating a session".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        if let Some(remedy) = route.split_once('/').and_then(|(provider, _)| {
            self.unauthenticated_providers
                .iter()
                .find(|remedy| remedy.provider == provider)
        }) {
            self.set_warning(remedy.message());
            return Effects::redraw(Redraw::Immediate);
        }
        let Some(workspace_id) = self.workspace_id else {
            self.set_warning("workspace is still connecting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.model = model.clone();
        self.send(
            PendingIntent::Create,
            SessionCommand::CreateSession {
                workspace_id,
                parent_id,
                model,
                approval_mode: self.approval_mode,
                profile: self.profile.clone(),
                reasoning_effort: self.reasoning_effort,
                correlation: qq_protocol::Correlation::default(),
            },
        )
    }

    fn submit_prompt(&mut self) -> Effects {
        let mut prompt = self.composer.expanded().trim().to_owned();
        if matches!(self.view(), View::SideQuestions(_))
            && !prompt.is_empty()
            && !prompt.starts_with('/')
        {
            prompt = format!("/btw {prompt}");
        }
        if prompt.is_empty() {
            return Effects::none();
        }
        if self.workspace_id.is_none() || self.resolving > 0 {
            if self.startup_submits.len() >= MAX_QUEUED_DRAFTS {
                self.set_warning(format!(
                    "at most {MAX_QUEUED_DRAFTS} prompts can wait while qq starts"
                ));
                return Effects::redraw(Redraw::Immediate);
            }
            self.startup_submits.push_back(QueuedPrompt {
                text: prompt,
                session_id: self.focused(),
                view: self.view(),
            });
            self.composer.clear();
            self.reset_history_browse();
            self.slash.select(0);
            self.set_info("prompt queued while qq starts".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let (name, question) = prompt
            .split_once(char::is_whitespace)
            .unwrap_or((&prompt, ""));
        if name == "/goal" && !question.trim().is_empty() {
            return match question.trim() {
                "pause" => self.control_goal(qq_protocol::GoalControl::Pause),
                "resume" => self.control_goal(qq_protocol::GoalControl::Resume),
                "clear" => self.control_goal(qq_protocol::GoalControl::Clear),
                "check" => self.open_goal_check(),
                "edit" => self.open_goal_form(None),
                objective => self.open_goal_form(Some(objective.to_owned())),
            };
        }
        if matches!(name, "/btw" | "/ask" | "/btw-new") {
            let Some(session_id) = self.focused().or(self.view_return) else {
                self.set_warning("create a session before asking a side question".to_owned());
                return Effects::redraw(Redraw::Immediate);
            };
            let question = question.trim();
            if question.is_empty() && name == "/btw-new" {
                self.set_warning("use /btw-new QUESTION to start a new side thread".to_owned());
                return Effects::redraw(Redraw::Immediate);
            }
            if question.is_empty() {
                self.composer.clear();
                return self.execute(Command::ShowSideQuestions);
            }
            if question.len() > 8192 {
                self.set_warning("use /btw QUESTION (at most 8 KiB)".to_owned());
                return Effects::redraw(Redraw::Immediate);
            }
            self.composer.clear();
            if self.view() != View::SideQuestions(session_id) {
                self.show_workspace_view(View::SideQuestions(session_id));
            }
            return self.send(
                PendingIntent::Side {
                    session_id,
                    text: prompt.clone(),
                },
                SessionCommand::SubmitSideQuestion {
                    session_id,
                    question: question.to_owned(),
                    new_thread: name == "/btw-new",
                },
            );
        }
        if prompt.starts_with('/') {
            let name = prompt.split_whitespace().next().unwrap_or(&prompt);
            if let Some(SlashAction::Client(command)) = commands::slash_entries()
                .find(|entry| entry.name == name)
                .map(|entry| entry.action)
            {
                self.composer.clear();
                self.slash.select(0);
                return self.execute(command);
            }
        }
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before sending a prompt".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let running = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.active_run_id.is_some());
        if running {
            if self.steering().is_some_and(|steering| steering.boundary) {
                return self.steer_run(false);
            }
            return self.queue_draft();
        }
        self.submit_text(session_id, prompt)
    }

    fn steer_run(&mut self, interrupt: bool) -> Effects {
        let text = self.composer.expanded().trim().to_owned();
        if text.is_empty() {
            return Effects::none();
        }
        self.steer_with_text(text, interrupt)
    }

    fn steer_with_text(&mut self, text: String, interrupt: bool) -> Effects {
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before steering a run".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(run_id) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.summary.active_run_id)
        else {
            self.set_warning("focused session has no active run to steer".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.record_prompt(session_id, &text);
        self.composer.clear();
        self.mention.clear();
        self.reset_history_browse();
        self.slash.select(0);
        self.esc_armed_at = None;
        if self.workspace_root.is_some() && has_mention_syntax(&text) {
            return Effects::resolve_mentions(
                self.start_resolution(),
                PendingSubmit {
                    text,
                    target: SubmitTarget::Steer {
                        session_id,
                        run_id,
                        interrupt,
                    },
                },
            );
        }
        self.send(
            PendingIntent::Steer {
                session_id,
                text: text.clone(),
            },
            SessionCommand::SteerRun {
                run_id,
                input: vec![qq_protocol::InputPart::text(text)],
                interrupt,
            },
        )
    }

    fn submit_text(&mut self, session_id: SessionId, prompt: String) -> Effects {
        self.record_prompt(session_id, &prompt);
        self.composer.clear();
        self.mention.clear();
        self.reset_history_browse();
        if self.status_level == NoticeLevel::Error && self.status_session_id == Some(session_id) {
            self.status = None;
        }
        if self.workspace_root.is_some() && has_mention_syntax(&prompt) {
            return Effects::resolve_mentions(
                self.start_resolution(),
                PendingSubmit {
                    text: prompt,
                    target: SubmitTarget::Prompt { session_id },
                },
            );
        }
        self.submit_parts(
            session_id,
            prompt.clone(),
            vec![qq_protocol::InputPart::text(prompt)],
        )
    }

    fn submit_parts(
        &mut self,
        session_id: SessionId,
        text: String,
        input: Vec<qq_protocol::InputPart>,
    ) -> Effects {
        self.send(
            PendingIntent::Prompt { session_id, text },
            SessionCommand::SubmitPrompt {
                session_id,
                input,
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
                output: None,
            },
        )
    }

    fn next_effect_id(&mut self) -> u64 {
        self.next_effect_id = self.next_effect_id.wrapping_add(1);
        self.next_effect_id
    }

    fn start_resolution(&mut self) -> u64 {
        let id = self.next_effect_id();
        self.pending_resolutions.insert(id);
        self.resolving = self.pending_resolutions.len();
        id
    }

    pub(crate) fn apply_resolved_mentions(
        &mut self,
        id: u64,
        submit: PendingSubmit,
        resolved: Result<qq_core::mentions::ResolvedPrompt, String>,
    ) -> Effects {
        let mut effects = self.finish_resolved_mentions(id, submit, resolved);
        if self.resolving == 0 && self.focused().is_some() {
            effects.extend(self.replay_queued_prompts());
        }
        effects
    }

    fn replay_queued_prompts(&mut self) -> Effects {
        if self.focused().is_none() {
            return Effects::none();
        }
        let draft = std::mem::take(&mut self.composer);
        let queued = std::mem::take(&mut self.startup_submits);
        let mut effects = Effects::none();
        for prompt in queued {
            let current_session = self.focused();
            let same_route = match prompt.session_id {
                Some(session_id) => {
                    current_session == Some(session_id) && self.view() == prompt.view
                }
                None => {
                    current_session.is_some()
                        && prompt.view == View::Transcript(None)
                        && self.view() == View::Transcript(current_session)
                }
            };
            if same_route {
                self.composer.replace(prompt.text);
                effects.extend(self.submit_prompt());
            } else {
                self.set_warning(
                    "queued prompt rejected because its session or view changed".to_owned(),
                );
                effects.extend(Effects::redraw(Redraw::Immediate));
            }
        }
        self.composer = draft;
        effects
    }

    fn finish_resolved_mentions(
        &mut self,
        id: u64,
        submit: PendingSubmit,
        resolved: Result<qq_core::mentions::ResolvedPrompt, String>,
    ) -> Effects {
        if !self.pending_resolutions.remove(&id) {
            return Effects::none();
        }
        self.resolving = self.pending_resolutions.len();
        let valid = match submit.target {
            SubmitTarget::Prompt { session_id } => self.focused() == Some(session_id),
            SubmitTarget::Steer {
                session_id, run_id, ..
            } => {
                self.focused() == Some(session_id)
                    && self
                        .sessions
                        .get(&session_id)
                        .and_then(|session| session.summary.active_run_id)
                        == Some(run_id)
            }
        };
        if !valid {
            if let SubmitTarget::Steer { session_id, .. } = submit.target
                && self.focused() == Some(session_id)
                && self.composer.text.is_empty()
            {
                self.composer.replace(submit.text);
                self.set_warning(
                    "run finished while resolving mentions; input restored".to_owned(),
                );
                return Effects::redraw(Redraw::Immediate);
            }
            return Effects::none();
        }
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                self.set_warning(format!("could not resolve mentions: {error}"));
                if self.composer.text.is_empty() {
                    self.composer.replace(submit.text);
                }
                return Effects::redraw(Redraw::Immediate);
            }
        };
        if !resolved.notes.is_empty() {
            self.set_warning(resolved.notes.join("; "));
        }
        let mut parts = resolved.parts;
        if let Some(skill) = resolved.skill {
            let rest: String = parts
                .iter()
                .filter_map(|part| match part {
                    qq_protocol::InputPart::Text { text } => Some(text.as_str()),
                    qq_protocol::InputPart::WorkspaceFile { .. } => None,
                })
                .collect();
            let text = format!("/{skill}{rest}");
            let mut rewritten = vec![qq_protocol::InputPart::text(text)];
            rewritten.extend(
                parts
                    .into_iter()
                    .filter(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. })),
            );
            parts = rewritten;
        }
        let attached = parts
            .iter()
            .filter(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. }))
            .count();
        if attached > 0 {
            self.set_info(format!(
                "attached {}",
                if attached == 1 {
                    "1 file".to_owned()
                } else {
                    format!("{attached} files")
                }
            ));
        }
        match submit.target {
            SubmitTarget::Prompt { session_id } => {
                self.submit_parts(session_id, submit.text, parts)
            }
            SubmitTarget::Steer {
                session_id,
                run_id,
                interrupt,
            } => self.send(
                PendingIntent::Steer {
                    session_id,
                    text: submit.text,
                },
                SessionCommand::SteerRun {
                    run_id,
                    input: parts,
                    interrupt,
                },
            ),
        }
    }

    pub(crate) fn apply_mention_completions(
        &mut self,
        id: u64,
        query: String,
        candidates: Vec<String>,
    ) -> bool {
        let expected = format!("{id}:{query}");
        if self.mention.query.as_deref() != Some(expected.as_str()) {
            return false;
        }
        let current = self
            .composer
            .mention_token()
            .map(|(_, token)| token.to_owned());
        if current.as_deref() != Some(query.as_str()) {
            return false;
        }
        self.mention.candidates = candidates;
        self.mention.cursor.select(0);
        true
    }

    pub(crate) fn apply_prefetch(&mut self, id: u64, session_id: SessionId) -> Effects {
        if self.pending_prefetch != Some((session_id, id)) {
            return Effects::none();
        }
        self.pending_prefetch = None;
        if self.session_picker_selected() != Some(session_id)
            || self
                .sessions
                .get(&session_id)
                .is_none_or(|session| session.is_warm())
        {
            return Effects::none();
        }
        let Some(workspace_id) = self.workspace_id else {
            return Effects::none();
        };
        Effects::send(ClientRequest::Snapshot(qq_protocol::SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: vec![session_id],
            session_limit: 512,
            message_limit: 256,
        }))
    }

    pub(crate) fn reject_prefetch(&mut self, id: u64, session_id: SessionId) {
        if self.pending_prefetch == Some((session_id, id)) {
            self.pending_prefetch = None;
        }
    }

    pub(super) fn cancel_prefetch(&mut self) {
        self.pending_prefetch = None;
    }

    pub(super) fn prefetch_session(&mut self, session_id: SessionId) -> Effects {
        let Some(workspace_id) = self.workspace_id else {
            return Effects::none();
        };
        if self
            .pending_prefetch
            .is_some_and(|(pending, _)| pending == session_id)
            || self.sessions.get(&session_id).is_none_or(|session| {
                session.is_warm() || session.summary.purpose == qq_protocol::SessionPurpose::Audit
            })
        {
            return Effects::none();
        }
        let id = self.next_effect_id();
        self.pending_prefetch = Some((session_id, id));
        let mut effects = Effects::none();
        effects.push(Effect::PrefetchBody {
            id,
            workspace_id,
            session_id,
        });
        effects
    }

    fn queue_draft(&mut self) -> Effects {
        let prompt = self.composer.expanded().trim().to_owned();
        if prompt.is_empty() {
            return Effects::none();
        }
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before queueing a prompt".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Effects::none();
        };
        if session.drafts.len() >= MAX_QUEUED_DRAFTS {
            self.set_warning(format!(
                "at most {MAX_QUEUED_DRAFTS} drafts can wait per session"
            ));
            return Effects::redraw(Redraw::Immediate);
        }
        session.drafts.push_back(prompt);
        self.composer.clear();
        self.reset_history_browse();
        self.slash.select(0);
        Effects::redraw(Redraw::Immediate)
    }

    fn dequeue_draft(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            return Effects::none();
        };
        if !self.composer.text.is_empty() {
            if self.queue_draft().is_empty() {
                return Effects::none();
            }
            if let Some(session) = self.sessions.get_mut(&session_id)
                && session.drafts.len() > 1
                && let Some(just_queued) = session.drafts.pop_back()
            {
                session.drafts.push_front(just_queued);
            }
        }
        let Some(draft) = self
            .sessions
            .get_mut(&session_id)
            .and_then(|session| session.drafts.pop_back())
        else {
            return Effects::none();
        };
        self.composer.replace(draft);
        Effects::redraw(Redraw::Immediate)
    }

    pub(crate) fn queued_drafts(&self, session_id: SessionId) -> impl Iterator<Item = &str> {
        self.sessions
            .get(&session_id)
            .into_iter()
            .flat_map(|session| &session.drafts)
            .map(String::as_str)
    }

    fn record_prompt(&mut self, session_id: SessionId, prompt: &str) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.record_prompt(prompt);
        }
    }

    fn browse_prompt_history(&mut self, forward: bool) -> bool {
        let Some(session_id) = self.focused() else {
            return false;
        };
        let Some(history) = self
            .sessions
            .get(&session_id)
            .map(|session| &session.prompt_history)
        else {
            return false;
        };
        if history.is_empty() {
            return false;
        }

        if forward {
            let Some(position) = self.history_position else {
                return false;
            };
            if position + 1 < history.len() {
                self.history_position = Some(position + 1);
                self.composer.replace(history[position + 1].clone());
            } else {
                self.history_position = None;
                self.composer
                    .replace(self.history_draft.take().unwrap_or_default());
            }
            return true;
        }

        let position = match self.history_position {
            Some(0) => return false,
            Some(position) => position - 1,
            None => {
                self.history_draft = Some(self.composer.text.clone());
                history.len() - 1
            }
        };
        self.history_position = Some(position);
        self.composer.replace(history[position].clone());
        true
    }

    fn reset_history_browse(&mut self) {
        self.history_position = None;
        self.history_draft = None;
    }

    fn compact_session(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before compacting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.status != SessionStatus::Idle)
        {
            self.set_warning("compaction needs an idle session; wait or cancel first".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        self.set_info("compacting session...".to_owned());
        self.send(
            PendingIntent::Compact { session_id },
            SessionCommand::CompactSession { session_id },
        )
    }

    fn rollback_compaction(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            self.set_warning("focus a session before rolling back a compaction".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.status != SessionStatus::Idle)
        {
            self.set_warning("rollback needs an idle session; wait or cancel first".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        self.send(
            PendingIntent::Rollback { session_id },
            SessionCommand::RollbackCompaction { session_id },
        )
    }

    fn acknowledge_side_question(&mut self, session_id: SessionId, side_question_id: RunId) {
        let settled = self.sessions.get(&session_id).is_some_and(|session| {
            session
                .side_questions
                .iter()
                .any(|item| item.id == side_question_id && item.state != SideQuestionState::Running)
        });
        if !settled {
            self.acknowledged_side_questions
                .insert(session_id, side_question_id);
        }
    }

    fn cancel_side_question(&mut self) -> Effects {
        let Some(session_id) = self.focused().or(self.view_return) else {
            self.set_warning("create a session before cancelling a side question".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let items = self
            .sessions
            .get(&session_id)
            .map(|session| session.side_questions.as_slice())
            .unwrap_or_default();
        let running = items
            .iter()
            .rev()
            .find(|item| item.state == SideQuestionState::Running)
            .map(|item| item.id);
        let acknowledged = self
            .acknowledged_side_questions
            .get(&session_id)
            .copied()
            .filter(|id| {
                !items
                    .iter()
                    .any(|item| item.id == *id && item.state != SideQuestionState::Running)
            });
        let Some(side_question_id) = running.or(acknowledged) else {
            self.set_info("no side question is running".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.send(
            PendingIntent::CancelSide { session_id },
            SessionCommand::CancelSideQuestion { side_question_id },
        )
    }

    fn cancel_run(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            self.set_warning("focused session has no active run".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(run_id) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.summary.active_run_id)
        else {
            self.set_warning("focused session has no active run".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.send(
            PendingIntent::Cancel { session_id },
            SessionCommand::CancelRun { run_id },
        )
    }

    pub(crate) fn pending_approval(&self) -> Option<&ToolCallSnapshot> {
        let session = self.sessions.get(&self.view().transcript()?)?;
        session.tool_calls.as_ref()?.iter().find(|tool_call| {
            tool_call.state == ToolCallState::AwaitingApproval
                && !self.answered_approvals.contains(&tool_call.id)
        })
    }

    pub(crate) fn pending_approval_preview(&self) -> Option<&ApprovalPreview> {
        let tool_call = self.pending_approval()?;
        self.sessions
            .get(&tool_call.session_id)?
            .approval_previews
            .get(&tool_call.id)
    }

    pub(crate) fn pending_question(&self) -> Option<&QuestionPreview> {
        self.pending_approval_preview()?.question.as_ref()
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> Effects {
        if matches!(self.settings.action_for(key), Some(Action::CancelRun)) {
            return self.cancel_run();
        }
        if let Some(question) = self.pending_question() {
            return self.handle_question_key(key, question.clone());
        }
        if let Some(choice) = self.approval_amendment.clone() {
            return match key.code {
                KeyCode::Enter => {
                    self.approval_amendment = None;
                    let text = self.composer.expanded().trim().to_owned();
                    let mut effects = self.respond_to_approval(choice);
                    if !text.is_empty() {
                        self.composer.clear();
                        effects.extend(self.steer_with_text(text, true));
                    }
                    effects
                }
                KeyCode::Esc => {
                    self.approval_amendment = None;
                    self.composer.clear();
                    Effects::redraw(Redraw::Immediate)
                }
                KeyCode::Backspace => Effects::changed_now(self.composer.backspace()),
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    Effects::changed_now(self.push_input(character))
                }
                _ => Effects::none(),
            };
        }
        match key.code {
            KeyCode::Char('y') => self.respond_to_approval(ApprovalChoice::Once),
            KeyCode::Char('n') | KeyCode::Esc => self.respond_to_approval(ApprovalChoice::Deny),
            KeyCode::Char('Y') => {
                self.approval_amendment = Some(ApprovalChoice::Once);
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Char('N') => {
                self.approval_amendment = Some(ApprovalChoice::Deny);
                Effects::redraw(Redraw::Immediate)
            }
            _ => Effects::none(),
        }
    }

    fn respond_to_background_approval(&mut self, approve: bool) -> Effects {
        let Some(session_id) = self.sessions_needing_attention().into_iter().find(|id| {
            Some(*id) != self.view().transcript()
                && !self.sessions[id].live.awaiting_approval.is_empty()
        }) else {
            self.set_info("no other session is waiting for approval".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(call) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.tool_calls.as_ref())
            .and_then(|calls| {
                calls.iter().find(|call| {
                    call.state == ToolCallState::AwaitingApproval
                        && !self.answered_approvals.contains(&call.id)
                })
            })
        else {
            return self.focus_session(session_id);
        };
        let tool_call_id = call.id;
        let run_id = call.run_id;
        let title = self.sessions[&session_id].summary.title.clone();
        self.answered_approvals.insert(tool_call_id);
        self.set_info(format!(
            "{} {title}'s {}",
            if approve { "approved" } else { "denied" },
            self.sessions[&session_id]
                .live
                .active_tool
                .as_deref()
                .unwrap_or("tool call")
        ));
        self.send(
            PendingIntent::Approval { tool_call_id },
            SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id,
                decision: if approve {
                    ApprovalDecision::ApproveOnce
                } else {
                    ApprovalDecision::Deny
                },
            },
        )
    }

    fn handle_question_key(&mut self, key: KeyEvent, question: QuestionPreview) -> Effects {
        let index = self.question_answers.len();
        let Some(current) = question.questions.get(index) else {
            return self.send_answers();
        };
        match key.code {
            KeyCode::Esc => {
                self.question_answers.clear();
                self.composer.clear();
                self.respond_to_approval(ApprovalChoice::Answer(Vec::new()))
            }
            KeyCode::Char(digit @ '1'..='9')
                if self.composer.text.is_empty()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let Some(option) = current
                    .options
                    .get(usize::from(digit as u8 - b'1'))
                    .cloned()
                else {
                    return if current.free_text {
                        Effects::changed_now(self.push_input(digit))
                    } else {
                        Effects::none()
                    };
                };
                self.question_answers.push(option);
                self.send_answers()
            }
            KeyCode::Enter => {
                let text = self.composer.expanded().trim().to_owned();
                if text.is_empty() {
                    return Effects::none();
                }
                self.composer.clear();
                self.question_answers.push(text);
                self.send_answers()
            }
            KeyCode::Backspace => Effects::changed_now(self.composer.backspace()),
            KeyCode::Char(character)
                if current.free_text
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Effects::changed_now(self.push_input(character))
            }
            _ => Effects::none(),
        }
    }

    fn send_answers(&mut self) -> Effects {
        let total = self
            .pending_question()
            .map_or(0, |question| question.questions.len());
        if self.question_answers.len() < total {
            return Effects::redraw(Redraw::Immediate);
        }
        let answers = std::mem::take(&mut self.question_answers);
        self.respond_to_approval(ApprovalChoice::Answer(answers))
    }

    fn respond_to_approval(&mut self, choice: ApprovalChoice) -> Effects {
        let Some(tool_call) = self.pending_approval() else {
            return Effects::none();
        };
        let tool_call_id = tool_call.id;
        let run_id = tool_call.run_id;
        let decision = match choice {
            ApprovalChoice::Once => ApprovalDecision::ApproveOnce,
            ApprovalChoice::Deny => ApprovalDecision::Deny,
            ApprovalChoice::Answer(answers) => ApprovalDecision::Answer { answers },
        };
        self.answered_approvals.insert(tool_call_id);
        self.send(
            PendingIntent::Approval { tool_call_id },
            SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id,
                decision,
            },
        )
    }

    fn push_input(&mut self, character: char) -> bool {
        let Some(character) = composer_character(character) else {
            return false;
        };
        if self.composer.text.len() + character.len_utf8() > MAX_INPUT_BYTES {
            return false;
        }
        self.composer.insert(character);
        self.reset_history_browse();
        self.slash.select(0);
        self.mention.invalidate();
        true
    }

    fn push_composer_text(&mut self, text: &str) -> bool {
        let mut sanitized = String::with_capacity(text.len().min(MAX_INPUT_BYTES));
        let budget = MAX_INPUT_BYTES.saturating_sub(self.composer.expanded().len());
        for character in text.chars() {
            if sanitized.len() + character.len_utf8() > budget {
                break;
            }
            if let Some(character) = composer_character(character) {
                sanitized.push(character);
            }
        }
        let changed = self.composer.paste(&sanitized);
        if changed {
            self.reset_history_browse();
            self.slash.select(0);
        }
        changed
    }

    fn handle_mention_key(&mut self, code: KeyCode) -> Option<Effects> {
        if self.mention.candidates.is_empty() || self.composer.mention_token().is_none() {
            return None;
        }
        match code {
            KeyCode::Up => {
                self.mention.cursor.move_up();
                Some(Effects::redraw(Redraw::Immediate))
            }
            KeyCode::Down => {
                self.mention.cursor.move_down(self.mention.candidates.len());
                Some(Effects::redraw(Redraw::Immediate))
            }
            KeyCode::Tab | KeyCode::Enter => {
                let index = self.mention.cursor.selected(self.mention.candidates.len());
                let candidate = self.mention.candidates[index].clone();
                let (span, _) = self.composer.mention_token()?;
                let is_dir = candidate.ends_with('/');
                let replacement = if is_dir {
                    format!("@{candidate}")
                } else {
                    format!("@{candidate} ")
                };
                self.composer.replace_span(span, &replacement);
                self.mention.clear();
                if is_dir {
                    return Some(self.request_mention_completion());
                }
                Some(Effects::redraw(Redraw::Immediate))
            }
            KeyCode::Esc => {
                self.mention.clear();
                Some(Effects::redraw(Redraw::Immediate))
            }
            _ => None,
        }
    }

    fn request_mention_completion(&mut self) -> Effects {
        if self.workspace_root.is_none() {
            return Effects::redraw(Redraw::Immediate);
        }
        let Some((_, token)) = self.composer.mention_token() else {
            self.mention.clear();
            return Effects::redraw(Redraw::Immediate);
        };
        if token.starts_with("web:")
            || token.starts_with("skill:")
            || token == "diff"
            || token.starts_with("sha:")
            || token.starts_with("diff:")
        {
            self.mention.clear();
            return Effects::redraw(Redraw::Immediate);
        }
        let query = token.to_owned();
        let id = self.next_effect_id();
        self.mention.clear();
        self.mention.query = Some(format!("{id}:{query}"));
        let recent = self
            .focused()
            .and_then(|id| self.sessions.get(&id))
            .map(|session| recently_edited_paths(session, 8))
            .unwrap_or_default();
        let mut effects = Effects::redraw(Redraw::Immediate);
        effects.push(Effect::CompleteMention { id, query, recent });
        effects
    }

    fn handle_slash_key(&mut self, code: KeyCode) -> Option<Effects> {
        if !matches!(
            code,
            KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Tab
        ) || !self.composer.text.starts_with('/')
        {
            return None;
        }
        let entries = self.filtered_slash_commands();
        if entries.is_empty() {
            return None;
        }
        match code {
            KeyCode::Up => {
                self.slash.move_up();
                Some(Effects::redraw(Redraw::Immediate))
            }
            KeyCode::Down => {
                self.slash.move_down(entries.len());
                Some(Effects::redraw(Redraw::Immediate))
            }
            KeyCode::Enter | KeyCode::Tab => {
                let entry = &entries[self.slash.selected(entries.len())];
                let name = entry.name.to_string();
                Some(self.accept_slash(entry.action, &name))
            }
            _ => None,
        }
    }

    pub(super) fn accept_slash(&mut self, action: SlashAction, name: &str) -> Effects {
        self.slash.select(0);
        match action {
            SlashAction::Client(command) => {
                self.composer.clear();
                self.execute(command)
            }
            SlashAction::WorkspaceCommand => {
                self.composer.replace(format!("{name} "));
                Effects::redraw(Redraw::Immediate)
            }
            SlashAction::Skill => {
                self.composer.replace(name.to_owned());
                self.submit_prompt()
            }
        }
    }

    pub(crate) fn filtered_slash_commands(&self) -> Vec<SlashEntry> {
        commands::matching_slash_entries(&self.composer.text, self.guidance_slash_entries())
    }

    fn guidance_slash_entries(&self) -> impl Iterator<Item = SlashEntry> + '_ {
        self.capabilities
            .as_deref()
            .and_then(|capabilities| capabilities.workspace_tools.as_ref())
            .map(|tools| tools.skills.entries.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|entry| SlashEntry {
                name: std::borrow::Cow::Owned(format!("/{}", entry.name)),
                title: std::borrow::Cow::Owned(entry.description.clone()),
                action: match entry.kind {
                    qq_protocol::GuidanceKind::Command => SlashAction::WorkspaceCommand,
                    qq_protocol::GuidanceKind::Skill => SlashAction::Skill,
                },
            })
    }

    pub(crate) fn slash_selected(&self, len: usize) -> usize {
        self.slash.selected(len)
    }

    pub(crate) fn tool_host_warning(&self) -> Option<String> {
        let tools = self.capabilities.as_deref()?.workspace_tools.as_ref()?;
        let mut warning = String::new();
        for message in tools
            .hosts
            .iter()
            .filter_map(|host| host.message.as_deref())
        {
            if !warning.is_empty() {
                warning.push_str("; ");
            }
            warning.push_str(message);
        }
        (!warning.is_empty()).then_some(warning)
    }

    pub fn advance_animation(&mut self) -> bool {
        self.animation_tick = self.animation_tick.wrapping_add(1);
        if self.now_ms > 0 {
            self.now_ms += ANIMATION_INTERVAL_MS;
        }
        for session in self.sessions.values_mut() {
            for reasoning in session.reasoning.values_mut() {
                if reasoning.streaming {
                    reasoning.ticks += 1;
                }
            }
        }
        let active = self
            .sessions
            .values()
            .any(|session| matches!(session.summary.status, qq_protocol::SessionStatus::Running));
        self.expire_status()
            || active
            || (self.has_active_goal() && self.animation_tick.is_multiple_of(10))
    }

    fn has_active_goal(&self) -> bool {
        self.focused()
            .and_then(|id| self.sessions.get(&id))
            .and_then(|session| session.goal.as_deref())
            .is_some_and(|goal| goal.status == qq_protocol::GoalStatus::Active)
    }

    pub(crate) fn refresh_goal_clock(&mut self) {
        if self.has_active_goal() {
            let ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
            self.goal_now_ms = ms.clamp(0, i128::from(u64::MAX)) as u64;
        }
    }

    pub fn has_activity(&self) -> bool {
        self.has_active_goal()
            || self.status.is_some()
            || self.sessions.values().any(|session| {
                matches!(session.summary.status, qq_protocol::SessionStatus::Running)
            })
    }

    pub fn pending_prompts(&self, session_id: SessionId) -> impl Iterator<Item = &str> {
        self.pending
            .values()
            .filter_map(move |intent| match intent {
                PendingIntent::Prompt {
                    session_id: candidate,
                    text,
                }
                | PendingIntent::Steer {
                    session_id: candidate,
                    text,
                } if *candidate == session_id => Some(text.as_str()),
                PendingIntent::Create
                | PendingIntent::GoalSet { .. }
                | PendingIntent::GoalControl { .. }
                | PendingIntent::Prompt { .. }
                | PendingIntent::Steer { .. }
                | PendingIntent::Cancel { .. }
                | PendingIntent::Compact { .. }
                | PendingIntent::Rollback { .. }
                | PendingIntent::Approval { .. }
                | PendingIntent::SetModel { .. }
                | PendingIntent::SetProfile { .. }
                | PendingIntent::SetApprovalMode { .. }
                | PendingIntent::SetEffort { .. }
                | PendingIntent::Delete { .. }
                | PendingIntent::Side { .. }
                | PendingIntent::CancelSide { .. }
                | PendingIntent::Prune
                | PendingIntent::Undo => None,
            })
    }

    pub(crate) fn focused_context_usage(&self) -> Option<(u64, u32)> {
        let session = self.focused().and_then(|id| self.sessions.get(&id))?;
        Some((session.summary.context_tokens?, session.context_window?))
    }

    fn sibling(&self, offset: isize) -> Option<SessionId> {
        let focused = self.focused()?;
        let parent = self.sessions.get(&focused)?.summary.parent_id;
        let siblings = match parent {
            Some(parent) => self.sessions.children_of(parent),
            None => self.sessions.roots(),
        };
        if siblings.len() < 2 {
            return None;
        }
        let position = siblings.iter().position(|id| *id == focused)?;
        let next = (position as isize + offset).rem_euclid(siblings.len() as isize) as usize;
        Some(siblings[next])
    }

    pub(crate) fn sessions_awaiting_approval(&self) -> Vec<SessionId> {
        self.sessions.awaiting_approval()
    }

    pub(crate) fn sessions_needing_attention(&self) -> Vec<SessionId> {
        self.sessions.needing_attention()
    }

    fn next_session_needing_attention(&self) -> Option<SessionId> {
        let waiting = self.sessions_needing_attention();
        let shown = self.view().transcript();
        let others: Vec<SessionId> = waiting
            .iter()
            .copied()
            .filter(|id| Some(*id) != shown)
            .collect();
        if others.is_empty() {
            return None;
        }
        let position = shown.and_then(|shown| waiting.iter().position(|id| *id == shown));
        match position {
            Some(index) => waiting
                .iter()
                .cycle()
                .skip(index + 1)
                .take(waiting.len())
                .copied()
                .find(|id| Some(*id) != shown),
            None => others.first().copied(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalChoice {
    Once,
    Deny,
    Answer(Vec<String>),
}

fn valid_model_route(route: &str) -> bool {
    route
        .split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
}

pub(crate) fn terminal_safe_character(character: char) -> Option<char> {
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

fn composer_character(character: char) -> Option<char> {
    match character {
        '\n' => Some('\n'),
        '\r' => None,
        character => terminal_safe_character(character),
    }
}

fn is_composer_newline_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT),
        KeyCode::Char('j' | 'J') if key.modifiers == KeyModifiers::CONTROL => true,
        KeyCode::Char('\n') => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests;

impl App {
    fn move_transcript_cursor(&mut self, down: bool) -> bool {
        let Some(session) = self
            .view()
            .transcript()
            .and_then(|id| self.sessions.get(&id))
        else {
            return false;
        };
        let Some(calls) = session.tool_calls.as_ref() else {
            return false;
        };
        if calls.is_empty() {
            return false;
        }
        let position = self
            .transcript_cursor
            .and_then(|id| calls.iter().position(|call| call.id == id));
        let next = match (position, down) {
            (None, true) => 0,
            (None, false) => calls.len() - 1,
            (Some(index), true) => (index + 1).min(calls.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
        };
        let target = calls[next].id;
        if self.transcript_cursor == Some(target) {
            return false;
        }
        self.transcript_cursor = Some(target);
        true
    }

    pub(crate) fn composer_mode(&self) -> crate::view::ComposerMode {
        use crate::view::ComposerMode;
        if self.pending_approval().is_some() {
            return ComposerMode::Approval;
        }
        if matches!(self.view(), View::SideQuestions(_)) {
            return ComposerMode::Send;
        }
        let running = self
            .focused()
            .and_then(|id| self.sessions.get(&id))
            .is_some_and(|session| session.summary.active_run_id.is_some());
        if !running {
            return ComposerMode::Send;
        }
        if self.steering().is_some_and(|steering| steering.boundary) {
            ComposerMode::Steer
        } else {
            ComposerMode::Queue
        }
    }
}
