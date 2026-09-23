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
    AgentProfileId, ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, CommandId,
    CommandOutcome, CommandRequest, ModelSelection, QuestionPreview, ReasoningEffort,
    ServerCapabilities, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    SessionStatus, SteeringCapabilities, ToolCallSnapshot, ToolCallState, WorkspaceId,
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

/// Expanded tool-call details the transcript remembers across sessions.
/// Toggling is per keypress, so this only bounds a very long interactive
/// session; past it, expansions for calls no longer retained are dropped.
const MAX_EXPANDED_TOOL_CALLS: usize = 256;

const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Milliseconds the loop's animation tick advances `now_ms` by; matches
/// `terminal::ANIMATION_INTERVAL`.
pub(crate) const ANIMATION_INTERVAL_MS: u64 = 125;
const MAX_RECENT_EVENTS: usize = 1024;
const MOUSE_SCROLL_ROWS: usize = 3;
/// Notices are deliberately ephemeral. At the 125 ms UI tick this keeps each
/// notice visible for five seconds without making it permanent UI.
const NOTICE_TICKS: u16 = 40;
/// Animation ticks (125 ms) within which a second Esc cancels the active run.
const ESC_CANCEL_TICKS: usize = 16;

#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    pub settings: Settings,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
    /// Built-in providers with no credential, each with the command or
    /// environment variable that would supply one. The empty state and
    /// `/models` show these where the provider's models would otherwise be.
    pub unauthenticated_providers: Vec<ProviderRemedy>,
    /// Every selectable theme; the first is active at startup. An empty list
    /// means the compiled `terminal` theme.
    pub themes: Vec<Theme>,
    /// The canonical workspace root `@` mentions resolve against. `None`
    /// (a remote client without the tree) leaves `@` as literal text.
    pub workspace_root: Option<std::path::PathBuf>,
}

/// A provider the configuration admits but that has no credential, and how
/// to give it one. `remedy` is imperative and complete (`run qq auth login
/// openai or set OPENAI_API_KEY`); the TUI prefixes it with the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRemedy {
    pub provider: String,
    pub remedy: String,
}

impl ProviderRemedy {
    /// The one-line guidance shown in the empty state and as a warning.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{} needs a credential: {}", self.provider, self.remedy)
    }
}

/// The composer notice a client without a configured model starts with.
pub(crate) const CHOOSE_MODEL_NOTICE: &str = "choose a model with /models";

/// Runs the TUI to exit. Returns the session focused at exit, after the
/// terminal has been restored, so the caller can tell the user how to
/// continue it.
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
    /// The client's update stream closed. Carries the last failure the client
    /// reported before closing, when there was one, so a startup that never
    /// reached a usable state (a bad `--session`, an unreachable server) is
    /// explained on the restored terminal rather than as a bare "stopped".
    #[error("TUI client stopped{}", .0.as_ref().map(|reason| format!(": {reason}")).unwrap_or_default())]
    ClientStopped(Option<String>),
}

/// How the transcript shows a run's tool calls. One row per call is the
/// default: while an agent works, what it is doing is the content. Folding
/// collapses finished quiet blocks to one summary row for reading back a
/// long transcript. A call's body is shown per call, never globally.
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

/// Whether reasoning blocks render as a collapsed one-liner or in full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReasoningDetail {
    #[default]
    Collapsed,
    Expanded,
}

/// The `@` completion popup's state.
#[derive(Debug, Default)]
pub(crate) struct MentionCompletion {
    pub(crate) candidates: Vec<String>,
    /// The token the candidates answer.
    pub(crate) query: Option<String>,
    pub(crate) cursor: Picker,
}

impl MentionCompletion {
    fn clear(&mut self) {
        self.candidates.clear();
        self.query = None;
        self.cursor.select(0);
    }

    /// The token changed under the popup: keep showing the old list until
    /// the new one arrives, but a stale accept must not fire.
    fn invalidate(&mut self) {
        self.query = None;
    }
}

/// Paths this session's completed edits and writes touched, newest first,
/// so `@` completion ranks the files the agent is working on ahead of the
/// rest of the tree.
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

/// Whether a prompt needs mention resolution at all: an `@` that the grammar
/// could accept. Cheap so plain prompts never pay for a blocking hop.
fn has_mention_syntax(text: &str) -> bool {
    !qq_protocol::parse_mentions(text).mentions.is_empty()
}

#[derive(Debug, Clone)]
enum PendingIntent {
    Create,
    Prompt {
        session_id: SessionId,
        text: String,
    },
    Cancel {
        session_id: SessionId,
    },
    /// A steering message in flight; `text` returns to the composer if the
    /// server refuses it.
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
    /// Workspace-wide; failures attach to the focused session.
    Prune,
}

pub(crate) struct App {
    pub settings: Settings,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
    /// Admitted built-in providers that lack a credential, with the remedy.
    /// Read-only after startup: a credential added while the TUI runs takes
    /// effect at the next launch, like the catalog it gates.
    pub(crate) unauthenticated_providers: Vec<ProviderRemedy>,
    /// Profile new sessions are created with. `/profile` with nothing focused
    /// sets it; the server validates the name when the session is created.
    pub profile: AgentProfileId,
    /// Approval mode new sessions are created with; `/approval` with nothing
    /// focused sets it.
    pub approval_mode: ApprovalMode,
    /// Reasoning effort new sessions are created with; `/effort` with nothing
    /// focused sets it. `None` leaves the compiled plan's configured choice.
    pub reasoning_effort: Option<ReasoningEffort>,
    pub workspace_id: Option<WorkspaceId>,
    pub workspace_path: String,
    /// Local root for `@` mention resolution and completion, when this
    /// client has the tree.
    pub(crate) workspace_root: Option<std::path::PathBuf>,
    pub sessions: SessionStore,
    /// Transcript panes left to right, each following its own view with its
    /// own scroll state; the renderer reconciles them every frame. One pane
    /// until slice L4 splits the body; never more than `viewport::MAX_PANES`.
    pub(crate) panes: Vec<TranscriptPane>,
    /// Index into `panes` of the pane the composer, approvals, footers,
    /// scrolling, and tree navigation act on.
    pub(crate) focused_pane: usize,
    /// The session a workspace view replaced, so Esc can return to it.
    view_return: Option<SessionId>,
    /// Monotonic counter bumped on every focus change; stamps `last_focused`.
    focus_clock: u64,
    /// The open overlay, if any. At most one overlay owns input at a time.
    pub overlay: Option<Overlay>,
    pub composer: Composer,
    /// An approval decision waiting for a steering amendment: `Y` or `N`
    /// armed it, the composer collects the text, Enter sends the decision
    /// and then steers the run with the text.
    pub(crate) approval_amendment: Option<ApprovalChoice>,
    /// Answers collected so far for a pending `ask_user` hold, one per
    /// question in order; the composer holds a free-text answer in progress.
    pub(crate) question_answers: Vec<String>,
    history_position: Option<usize>,
    history_draft: Option<String>,
    /// Cursor into the slash autocomplete list. The query is the composer
    /// text itself, so only the cursor lives here.
    slash: Picker,
    /// The `@` completion popup: candidates for the token at the cursor and
    /// the query they answer, so a stale reply is ignored.
    pub(crate) mention: MentionCompletion,
    /// Prompts whose `@` mentions are resolving off the executor. The
    /// composer is already cleared; a failure puts the text back.
    resolving: usize,
    /// Tick at which Esc was last pressed with nothing to dismiss; a second
    /// press within [`ESC_CANCEL_TICKS`] cancels the active run.
    esc_armed_at: Option<usize>,
    /// The server's workspace-scoped capability document. `None` until it
    /// arrives, which reads as "unavailable": `Submit` queues instead of
    /// steering, and the profile and approval pickers say why.
    capabilities: Option<Arc<ServerCapabilities>>,
    pub connection: ConnectionState,
    pub status: Option<String>,
    /// Session owning the current transient notice. A notice never follows
    /// the user into another session.
    status_session_id: Option<SessionId>,
    pub(crate) status_level: NoticeLevel,
    status_ticks_left: u16,
    pub animation_tick: usize,
    /// Wall-clock estimate in server milliseconds: the newest event's
    /// `occurred_at_ms`, advanced by the animation interval between events
    /// so running rows show a live elapsed time without a system clock in
    /// the frame path. Zero until the first event.
    pub(crate) now_ms: u64,
    pub tool_detail: ToolDetail,
    /// Tool calls expanded individually (Enter on a selected row), on top of
    /// the global `tool_detail` toggle.
    pub(crate) expanded_tool_calls: std::collections::HashSet<qq_protocol::ToolCallId>,
    /// The tool call the transcript cursor rests on, if any. Ctrl-Up/Down
    /// move it through the focused session's calls; Enter toggles expansion.
    pub(crate) transcript_cursor: Option<qq_protocol::ToolCallId>,
    pub reasoning_detail: ReasoningDetail,
    /// Session sidebar visibility. `Auto` shows it when the terminal is wide
    /// enough; the toggle command cycles through explicit on and off.
    /// Standing layout choices: rail and inspector visibility. `Auto` follows
    /// the terminal's tier (`view::layout`).
    pub layout: crate::view::LayoutPrefs,
    /// Whether the terminal reports mouse events to us. On by default so the
    /// wheel scrolls the transcript; `/mouse` turns it off for native
    /// selection and copy (most terminals also select with Shift held).
    pub(crate) mouse_capture: bool,
    /// Terminal width from the last resize event, so update paths can decide
    /// whether a change to an unshown session is visible at all. Zero until
    /// the first resize, which reads as "assume visible".
    terminal_width: usize,
    /// Selectable themes and the index of the active one. Changing the
    /// index bumps `theme_generation` so the renderer repaints everything.
    pub(crate) themes: Vec<Theme>,
    pub(crate) theme: usize,
    pub(crate) theme_generation: u64,
    /// Whether the terminal window has keyboard focus, from the terminal's
    /// focus events. Assumed focused until told otherwise.
    terminal_focused: bool,
    last_sequence: u64,
    recent_events: VecDeque<SessionEventEnvelope>,
    pending: HashMap<CommandId, PendingIntent>,
    answered_approvals: std::collections::HashSet<qq_protocol::ToolCallId>,
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
            workspace_id: None,
            workspace_path: String::new(),
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
            esc_armed_at: None,
            capabilities: None,
            connection: ConnectionState::Connecting,
            status: None,
            status_session_id: None,
            status_level: NoticeLevel::Info,
            status_ticks_left: 0,
            animation_tick: 0,
            now_ms: 0,
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
        }
    }

    /// Requests queued by [`Self::apply_client_update`]; the terminal loop
    /// drains and sends them after each update.
    /// Who owns keyboard input right now. Overlays win over the approval
    /// prompt, which wins over the composer.
    pub(crate) fn mode(&self) -> Mode {
        match &self.overlay {
            Some(overlay) => overlay.mode(),
            None if self.pending_approval().is_some() => Mode::Approval,
            None => Mode::Compose,
        }
    }

    /// The external editor could not deliver text; the draft stays as it was.
    pub fn note_editor_failure(&mut self, reason: &str) {
        self.set_warning(reason.to_owned());
    }

    /// Install text returned by the external editor. `None` means it exited
    /// without changing the draft.
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
                self.refresh_profile_picker();
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::Event(event) => self.apply_live_event(event),
            ClientUpdate::CommandResult { command_id, result } => {
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
                                    ApprovalResolution::ApprovedForSession => {
                                        "tool call approved for this session"
                                    }
                                    ApprovalResolution::ApprovedForWorkspace => {
                                        "tool call approved for this workspace"
                                    }
                                    ApprovalResolution::ApprovedByReviewer => {
                                        "tool call already approved by the reviewer"
                                    }
                                    ApprovalResolution::Denied => "tool call denied",
                                    ApprovalResolution::DeniedTimeout => {
                                        "tool call already denied by timeout"
                                    }
                                    ApprovalResolution::DeniedByReviewer => {
                                        "tool call already denied by the reviewer"
                                    }
                                    ApprovalResolution::Answered => "answer sent",
                                }
                                .to_owned(),
                            );
                        }
                        if matches!(receipt.outcome, CommandOutcome::RunAlreadyFinished { .. }) {
                            // A steer that lost the race to the finishing run was never
                            // applied; hand the text back rather than losing it.
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
                            _ => {}
                        }
                    }
                    Err(error) => self.reject_pending(command_id, error),
                }
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::SnapshotFailed(error) => {
                self.set_warning(error.message().to_owned());
                Effects::redraw(Redraw::Scheduled)
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: WorkspaceSnapshot) -> Effects {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            self.set_warning("server returned a snapshot for another workspace".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        let snapshot_focus = snapshot.focused.as_ref().map(|focused| focused.summary.id);
        // A late snapshot for a session no longer shown is stale navigation
        // output; installing it would yank focus back.
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

        let snapshot_sequence = snapshot.cursor.sequence;
        if initial {
            self.workspace_id = Some(snapshot.workspace.id);
            self.workspace_path = snapshot.workspace.path;
        }
        if initial || snapshot_sequence >= self.last_sequence {
            for summary in snapshot.sessions {
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
            self.sessions
                .install_session_snapshot(focused, &self.models, snapshot_sequence);
            // The body may have been fetched for a non-focused pane (the
            // user moved on before it arrived); only the initial snapshot
            // and a still-focused pane move focus.
            if initial || self.focused().is_none() || self.focused() == Some(focused_id) {
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
                    && snapshot_focus.is_some_and(|focused| event.session_id == focused)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut effects = Effects::redraw(Redraw::Scheduled);
        for event in replay {
            effects.extend(self.reduce_event(&event));
        }
        effects
    }

    /// Reduce one event into the store and apply what the reducer asks of the
    /// surface: notices land in the status line here, focus and picker state
    /// update here, and the rest become loop effects.
    fn reduce_event(&mut self, envelope: &SessionEventEnvelope) -> Effects {
        let caused_by_me = envelope
            .caused_by
            .and_then(|id| self.pending.get(&id))
            .is_some_and(|intent| matches!(intent, PendingIntent::Create));
        let reduced = self.sessions.reduce_event(
            envelope,
            ReduceContext {
                focused: self.focused(),
                attentive: self.terminal_focused,
                workspace_id: self.workspace_id,
                capabilities: self.capabilities.as_ref(),
                models: &self.models,
                caused_by_me,
            },
        );
        let mut effects = Effects::none();
        for effect in reduced {
            match effect {
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
        // Summaries and deletions reshape the tree the picker lists.
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

    /// A session this client just created has an empty transcript by
    /// construction, so it is warm immediately: focus moves in this frame and
    /// no snapshot round trip is needed before the user can type.
    pub(super) fn adopt_created_session(&mut self, session_id: SessionId) {
        self.sessions.warm_empty(session_id);
        self.set_focus(session_id);
        self.reset_history_browse();
        self.evict_cold_bodies();
    }

    /// Show `session_id` and stamp it so warm-body eviction keeps the most
    /// recently viewed sessions. Does not request anything. Focusing a
    /// session always means reading it, so a workspace view gives way.
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
            self.pending.remove(&command_id);
        }
        self.recent_events.push_back(event);
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
        effects
    }

    /// Whether applying `event` can change anything on screen. Streaming text
    /// and tool output for a session no pane shows only matter when the
    /// sidebar (which shows every session's live tail) is visible; every
    /// other event may move focus, attention, or chrome and always redraws.
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
        // A child's live status renders under its spawn call in the parent.
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
        // Errors are sticky: a failure must stay visible until the user acts
        // (a new prompt, an interrupt, or another notice replaces it).
        // Informational notices expire on their own.
        self.status_ticks_left = match level {
            NoticeLevel::Error => 0,
            NoticeLevel::Info | NoticeLevel::Warning => NOTICE_TICKS,
        };
    }

    fn set_notice(&mut self, text: String, level: NoticeLevel) {
        self.set_notice_for(self.focused(), text, level);
    }

    /// Show a notice produced as an effect. `None` attaches it to the
    /// focused session.
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

    /// The remedy for the client default model's provider when that provider
    /// has no credential: the reason Alt-N cannot create a session yet.
    pub(crate) fn configured_provider_remedy(&self) -> Option<&ProviderRemedy> {
        let provider = self.model.model.as_deref()?.split_once('/')?.0;
        self.unauthenticated_providers
            .iter()
            .find(|remedy| remedy.provider == provider)
    }

    /// What a client that cannot create a session yet should do: name the
    /// missing credential for the configured model, or point at `/models`
    /// when no model is configured at all. `None` once a model with an
    /// authenticated provider is in hand.
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
            Some(PendingIntent::Prompt { session_id, .. })
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
            Some(PendingIntent::Create | PendingIntent::Prune) | None => self.focused(),
        };
        match intent {
            Some(PendingIntent::Prompt { session_id, text })
            | Some(PendingIntent::Steer { session_id, text })
                if self.focused() == Some(session_id) && self.composer.text.is_empty() =>
            {
                self.composer.replace(text);
            }
            Some(PendingIntent::Approval { tool_call_id }) => {
                // Re-open the prompt so the user can answer again.
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
                // The wheel scrolls the transcript wherever the pointer is, so
                // a wheel over the chrome still does something useful.
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
            Mode::Sessions
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
        // Newline chords insert into the composer. Handle them before slash
        // completion and configured bindings so they never submit.
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
        // `?` on an empty composer opens help; typed into text it is a
        // character like any other.
        if key.code == KeyCode::Char('?') && self.composer.text.is_empty() {
            return self.execute(Command::OpenHelp);
        }
        // Every chord lives in the command table; configured actions win.
        if let Some(command) = commands::command_for_key(&self.settings, key) {
            return self.execute(command);
        }
        match key.code {
            KeyCode::Esc => {
                if self.transcript_cursor.take().is_some() {
                    return Effects::redraw(Redraw::Immediate);
                }
                // A workspace view returns to the session it replaced.
                if matches!(self.view(), View::Attention | View::Changes) {
                    return self.leave_workspace_view();
                }
                // A sticky error notice dismisses first: acknowledging the
                // failure is the most immediate intent Esc can carry.
                if self.status.is_some()
                    && self.status_level == NoticeLevel::Error
                    && self.status_session_id == self.focused()
                {
                    self.status = None;
                    return Effects::redraw(Redraw::Immediate);
                }
                // While a run is active, Esc twice within a short window cancels
                // it; the first press only arms and shows the hint.
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
            // With a tool row selected and nothing typed, Enter toggles that
            // call's detail; otherwise it submits.
            KeyCode::Enter if self.transcript_cursor.is_some() && self.composer.text.is_empty() => {
                let Some(call) = self.transcript_cursor else {
                    return Effects::none();
                };
                // A spawn call opens its child; every other call toggles detail.
                if let Some(child) = self.sessions.child_spawned_by(call) {
                    self.transcript_cursor = None;
                    return self.focus_session(child);
                }
                if !self.expanded_tool_calls.remove(&call) {
                    // Bounded: an expansion for a call no session still
                    // retains has nothing to render and is dropped first.
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
            // Ctrl-Home/End jump the transcript; plain Home/End edit the line.
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
                // Typing on an `@` token asks for candidates; the reply is
                // matched to the token as it stands when it arrives.
                if changed && self.composer.mention_token().is_some() {
                    return self.request_mention_completion();
                }
                Effects::changed_now(changed)
            }
            _ => Effects::none(),
        }
    }

    /// The session shown in the focused pane; what every focus-dependent
    /// surface (composer, approvals, footers, tree navigation) acts on.
    pub(crate) fn focused(&self) -> Option<SessionId> {
        self.view().session()
    }

    /// The focused transcript pane. `panes` always holds at least one and
    /// `focused_pane` is kept in range, so this never fails.
    pub(crate) fn pane(&self) -> &TranscriptPane {
        &self.panes[self.focused_pane.min(self.panes.len() - 1)]
    }

    fn pane_mut(&mut self) -> &mut TranscriptPane {
        let index = self.focused_pane.min(self.panes.len() - 1);
        &mut self.panes[index]
    }

    /// What the focused pane shows.
    pub(crate) fn view(&self) -> View {
        self.pane().view
    }

    /// The session a workspace view replaced in the focused pane, if one is
    /// up. The renderer keeps that transcript on screen while the inspector
    /// holds the view.
    pub(crate) fn view_return(&self) -> Option<SessionId> {
        self.view_return
    }

    /// Point the focused pane at `view`. Its viewport returns to the tail on
    /// the next frame, as `Viewport::update` does for any view change.
    pub(crate) fn set_view(&mut self, view: View) {
        self.pane_mut().view = view;
    }

    /// Scroll state of the focused pane, as of its last frame.
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

    /// Show a workspace-wide view. Invoking the one already shown returns to
    /// the transcript, so `/attention` toggles.
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

    /// Return from a workspace view to the session it replaced, or to the
    /// first session when that one is gone.
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

    /// Test view of the focused pane's viewport through the renderer's
    /// reconcile step.
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

    /// Run one command from the registry. Every command surface — keybinding,
    /// slash entry, and later the palette — ends here so behavior cannot drift
    /// between them.
    pub(crate) fn execute(&mut self, command: Command) -> Effects {
        match command {
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

    /// Send one session command, remembering `intent` so the receipt or
    /// failure is attributed to the right session and can undo optimistic
    /// state. Every request that carries a `CommandId` goes through here.
    fn send(&mut self, intent: PendingIntent, command: SessionCommand) -> Effects {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.pending.insert(command_id, intent);
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command,
        }))
    }

    fn set_session_model(&mut self, session_id: SessionId, mut model: ModelSelection) -> Effects {
        model.model_is_fallback = false;
        // Remember the pick as the client default so /new and later creates
        // keep using it until the user chooses another model.
        self.model = model.clone();
        self.send(
            PendingIntent::SetModel { session_id },
            SessionCommand::SetSessionModel { session_id, model },
        )
    }

    /// Focus a session. A warm body renders immediately with no request; a
    /// cold one shows its summary and live tail while its body is fetched.
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

    /// Choose the model for an implicit create (`/new` and create shortcuts).
    /// An explicit/configured client default wins. If none is available (for
    /// example while reattaching before model discovery completes), inherit
    /// the focused session's route.
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
        // A route on a provider known to lack a credential would only fail at
        // the server; name the fix here instead.
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
        // Keep the chosen model as the client default for the rest of this TUI
        // process until /models picks something else.
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
        let prompt = self.composer.expanded().trim().to_owned();
        if prompt.is_empty() {
            return Effects::none();
        }
        // Reserved composer commands stay client-side. Every other leading
        // slash is submitted through the ordinary command path so the shared
        // runtime can resolve an explicit command or skill consistently for
        // direct, embedded, and remote clients.
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
        // Enter during an active run steers when the server supports it and
        // otherwise holds the draft locally until the run finishes. Sending
        // it to the server queue now would lose the ability to edit it.
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

    /// Send the draft to the focused session's active run as steering. The
    /// caller has checked the capability; this only checks there is a run.
    /// With `interrupt`, the run's in-flight turn is aborted first.
    fn steer_run(&mut self, interrupt: bool) -> Effects {
        let text = self.composer.expanded().trim().to_owned();
        if text.is_empty() {
            return Effects::none();
        }
        self.steer_with_text(text, interrupt)
    }

    /// Send `text` as steering for the focused session's active run.
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
            self.resolving += 1;
            return Effects::resolve_mentions(PendingSubmit {
                text,
                target: SubmitTarget::Steer { run_id, interrupt },
            });
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

    /// Send `prompt` to `session_id` as a new run.
    fn submit_text(&mut self, session_id: SessionId, prompt: String) -> Effects {
        self.record_prompt(session_id, &prompt);
        self.composer.clear();
        self.mention.clear();
        self.reset_history_browse();
        // Submitting a new prompt acknowledges any sticky failure notice.
        if self.status_level == NoticeLevel::Error && self.status_session_id == Some(session_id) {
            self.status = None;
        }
        // `@` mentions resolve off the executor (file reads, a git call);
        // the loop calls back with parts. Plain text goes straight out.
        if self.workspace_root.is_some() && has_mention_syntax(&prompt) {
            self.resolving += 1;
            return Effects::resolve_mentions(PendingSubmit {
                text: prompt,
                target: SubmitTarget::Prompt { session_id },
            });
        }
        self.submit_parts(
            session_id,
            prompt.clone(),
            vec![qq_protocol::InputPart::text(prompt)],
        )
    }

    /// Send resolved parts as a new run; `text` is what returns to the
    /// composer if the server refuses the command.
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

    /// Mention resolution came back from the loop. Notes (a literal left in
    /// place, a directory too wide) show as a warning; the prompt still goes
    /// unless nothing resolvable remained.
    pub(crate) fn apply_resolved_mentions(
        &mut self,
        submit: PendingSubmit,
        resolved: Result<qq_core::mentions::ResolvedPrompt, String>,
    ) -> Effects {
        self.resolving = self.resolving.saturating_sub(1);
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
        // `@skill:name` at the start becomes a `/name` slash command.
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
            SubmitTarget::Steer { run_id, interrupt } => {
                let Some(session_id) = self.focused() else {
                    return Effects::redraw(Redraw::Immediate);
                };
                self.send(
                    PendingIntent::Steer {
                        session_id,
                        text: submit.text,
                    },
                    SessionCommand::SteerRun {
                        run_id,
                        input: parts,
                        interrupt,
                    },
                )
            }
        }
    }

    /// Candidates for the `@` token came back. Dropped when the composer's
    /// token no longer matches the query they answer.
    pub(crate) fn apply_mention_completions(
        &mut self,
        query: String,
        candidates: Vec<String>,
    ) -> bool {
        let current = self
            .composer
            .mention_token()
            .map(|(_, token)| token.to_owned());
        if current.as_deref() != Some(query.as_str()) {
            return false;
        }
        self.mention.candidates = candidates;
        self.mention.query = Some(query);
        self.mention.cursor.select(0);
        true
    }

    /// Hold the composer text for the focused session until its run ends.
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

    /// Pull the newest queued draft back into the composer for editing. A
    /// non-empty composer is queued first so nothing is lost.
    fn dequeue_draft(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            return Effects::none();
        };
        if !self.composer.text.is_empty() {
            if self.queue_draft().is_empty() {
                return Effects::none();
            }
            // The draft just queued is newest; rotate so the previously
            // newest one comes back.
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

    /// Drafts waiting for `session_id` in submission order.
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

    /// The focused session's oldest unanswered tool approval, if any.
    pub(crate) fn pending_approval(&self) -> Option<&ToolCallSnapshot> {
        let session = self.sessions.get(&self.focused()?)?;
        session.tool_calls.as_ref()?.iter().find(|tool_call| {
            tool_call.state == ToolCallState::AwaitingApproval
                && !self.answered_approvals.contains(&tool_call.id)
        })
    }

    /// The diff preview carried by the pending approval's request, if any.
    /// The server-computed preview of what the pending approval would do.
    pub(crate) fn pending_approval_preview(&self) -> Option<&ApprovalPreview> {
        let tool_call = self.pending_approval()?;
        self.sessions
            .get(&tool_call.session_id)?
            .approval_previews
            .get(&tool_call.id)
    }

    /// The pending hold's question, when it is an `ask_user` call.
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
        // With an amendment armed, the composer collects the steering text;
        // Enter sends the decision then the steer, Esc drops the amendment.
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
            KeyCode::Char('a' | 'A') => self.respond_to_approval(ApprovalChoice::Session),
            KeyCode::Char('w' | 'W') => self.respond_to_approval(ApprovalChoice::Workspace),
            KeyCode::Char('n') | KeyCode::Esc => self.respond_to_approval(ApprovalChoice::Deny),
            // Shift-Y / Shift-N: decide and then steer the run with a note,
            // for "yes, but…" and "no, do this instead".
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

    /// Answer the first approval waiting in a session other than the focused
    /// one, without moving focus. The banner names that session; this is
    /// its inline answer.
    fn respond_to_background_approval(&mut self, approve: bool) -> Effects {
        let Some(session_id) = self.sessions_needing_attention().into_iter().find(|id| {
            Some(*id) != self.focused() && !self.sessions[id].live.awaiting_approval.is_empty()
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
            // The session is waiting but its body is cold: focus it so the
            // body loads and the inline block appears.
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

    /// Keys while an `ask_user` hold is pending: a digit picks that option
    /// for the current question, typed text is a free answer Enter submits,
    /// Esc declines the whole question set. Answers accumulate until every
    /// question has one, then the decision is sent.
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
                    // A digit past the option list starts a free answer if
                    // one is allowed; otherwise nothing.
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

    /// Sends the collected answers once every question has one; otherwise
    /// redraws so the block shows the next question.
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
        let preview = self.pending_approval_preview();
        // A session or workspace choice whose grant cannot be stored must not
        // be sent as one: the server would reject the whole approval. Approve
        // this call once and say why, so the key never produces a 400.
        let (decision, notice) = match choice {
            ApprovalChoice::Once => (ApprovalDecision::ApproveOnce, None),
            ApprovalChoice::Session | ApprovalChoice::Workspace => {
                let grant = approval_grant(tool_call, preview);
                let workspace = matches!(choice, ApprovalChoice::Workspace);
                match grant_recording(&grant) {
                    GrantRecording::Recorded => (
                        if workspace {
                            ApprovalDecision::ApproveForWorkspace { grant }
                        } else {
                            ApprovalDecision::ApproveForSession { grant }
                        },
                        None,
                    ),
                    GrantRecording::TooLong { bytes } => (
                        ApprovalDecision::ApproveOnce,
                        Some(format!(
                            "approved once; a session grant may be at most {} bytes and this command is {bytes}",
                            qq_core::MAX_GRANT_BYTES
                        )),
                    ),
                    GrantRecording::Empty => (
                        ApprovalDecision::ApproveOnce,
                        Some("approved once; there is no command or tool name to grant".to_owned()),
                    ),
                }
            }
            ApprovalChoice::Deny => (ApprovalDecision::Deny, None),
            ApprovalChoice::Answer(answers) => (ApprovalDecision::Answer { answers }, None),
        };
        if let Some(notice) = notice {
            self.set_warning(notice);
        }
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

    /// Insert pasted text. The sanitized content goes through the composer's
    /// paste path, which collapses large pastes to a placeholder; the byte
    /// bound applies to the expanded content so a placeholder cannot hide an
    /// oversized prompt.
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

    /// Up/Down/Tab/Enter on the `@` popup; Esc closes it. Returns `None`
    /// when no popup is showing so the key falls through.
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
                // A directory keeps the popup open one level deeper; a file
                // closes it and adds the trailing space the grammar ends on.
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

    /// Ask the loop for candidates for the `@` token at the cursor.
    fn request_mention_completion(&mut self) -> Effects {
        if self.workspace_root.is_none() {
            return Effects::redraw(Redraw::Immediate);
        }
        let Some((_, token)) = self.composer.mention_token() else {
            self.mention.clear();
            return Effects::redraw(Redraw::Immediate);
        };
        // Special refs complete nothing.
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
        let recent = self
            .focused()
            .and_then(|id| self.sessions.get(&id))
            .map(|session| recently_edited_paths(session, 8))
            .unwrap_or_default();
        let mut effects = Effects::redraw(Redraw::Immediate);
        effects.push(Effect::CompleteMention { query, recent });
        effects
    }

    fn handle_slash_key(&mut self, code: KeyCode) -> Option<Effects> {
        // Only navigation and acceptance keys consult the list; ordinary typing
        // must not pay for building it.
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

    /// Run a client command, submit a skill, or leave a workspace command in
    /// the composer for its arguments.
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

    /// Slash entries matching the composer text: client commands first, then
    /// the workspace's commands and skills as the server indexed them. Empty
    /// unless the composer holds a bare `/token`.
    pub(crate) fn filtered_slash_commands(&self) -> Vec<SlashEntry> {
        commands::matching_slash_entries(&self.composer.text, self.guidance_slash_entries())
    }

    /// The workspace's indexed commands and skills as slash entries. Nothing
    /// until the capability document arrives.
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

    /// Highlighted row in the slash autocomplete list, clamped to `len`.
    pub(crate) fn slash_selected(&self, len: usize) -> usize {
        self.slash.selected(len)
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
        self.expire_status() || active
    }

    pub fn has_activity(&self) -> bool {
        self.status.is_some()
            || self.sessions.values().any(|session| {
                matches!(session.summary.status, qq_protocol::SessionStatus::Running)
            })
    }

    /// Prompts and steering sent to `session_id` whose receipt has not
    /// arrived; shown optimistically until the server's row replaces them.
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
                | PendingIntent::Prune => None,
            })
    }

    pub(crate) fn focused_context_usage(&self) -> Option<(u64, u32)> {
        let session = self.focused().and_then(|id| self.sessions.get(&id))?;
        Some((session.summary.context_tokens?, session.context_window?))
    }

    /// The focused session's sibling `offset` places away in spawn order
    /// (oldest-first), wrapping at either end. Roots are siblings of roots.
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

    /// Sessions with a tool call awaiting approval, in tree order.
    pub(crate) fn sessions_awaiting_approval(&self) -> Vec<SessionId> {
        self.sessions.awaiting_approval()
    }

    /// Sessions that need the user, most urgent first and then in tree
    /// order: approvals, then unread failures, then unread finishes.
    pub(crate) fn sessions_needing_attention(&self) -> Vec<SessionId> {
        self.sessions.needing_attention()
    }

    /// The next session (after the focused one, wrapping) that needs the
    /// user, excluding the focused session itself. Approvals come first so
    /// Ctrl-G always lands on the most urgent thing.
    fn next_session_needing_attention(&self) -> Option<SessionId> {
        let waiting = self.sessions_needing_attention();
        let others: Vec<SessionId> = waiting
            .iter()
            .copied()
            .filter(|id| Some(*id) != self.focused())
            .collect();
        if others.is_empty() {
            return None;
        }
        // Cycle: the item after the focused one in the priority list, or the
        // first when the focused session is not in the list.
        let position = self
            .focused()
            .and_then(|focused| waiting.iter().position(|id| *id == focused));
        match position {
            Some(index) => waiting
                .iter()
                .cycle()
                .skip(index + 1)
                .take(waiting.len())
                .copied()
                .find(|id| Some(*id) != self.focused()),
            None => others.first().copied(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalChoice {
    Once,
    Session,
    Workspace,
    Deny,
    /// Answers to an `ask_user` hold; empty declines.
    Answer(Vec<String>),
}

/// Derives the approve-for-session grant from the pending call: shell calls
/// allowlist their exact command as a prefix, fetch calls grant the host the
/// server judged, everything else grants the tool.
fn approval_grant(
    tool_call: &ToolCallSnapshot,
    preview: Option<&ApprovalPreview>,
) -> ApprovalGrant {
    if let Some(fetch) = preview.and_then(|preview| preview.fetch.as_ref()) {
        return ApprovalGrant::Host {
            host: fetch.host.clone(),
        };
    }
    if tool_call.name == "shell"
        && let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
        && let Some(command) = arguments.get("command").and_then(|value| value.as_str())
        && !command.trim().is_empty()
    {
        return ApprovalGrant::ShellPrefix {
            prefix: command.to_owned(),
        };
    }
    ApprovalGrant::Tool {
        name: tool_call.name.clone(),
    }
}

/// Why a derived grant cannot be recorded, or that it can. The byte cap is the
/// server's `MAX_GRANT_BYTES`; a value past it used to fail the whole approval
/// with "approval grant is empty or exceeds the session limit".
enum GrantRecording {
    Recorded,
    Empty,
    TooLong { bytes: usize },
}

fn grant_recording(grant: &ApprovalGrant) -> GrantRecording {
    let value = match grant {
        ApprovalGrant::Tool { name } => name,
        ApprovalGrant::ShellPrefix { prefix } => prefix,
        ApprovalGrant::Host { host } => host,
    };
    let value = value.trim();
    if value.is_empty() {
        GrantRecording::Empty
    } else if value.len() > qq_core::MAX_GRANT_BYTES {
        GrantRecording::TooLong { bytes: value.len() }
    } else {
        GrantRecording::Recorded
    }
}

/// Whether the approval prompt may offer a session or workspace grant for this
/// call. The server's per-session count is not known here; a grant that is
/// empty or over the byte cap is the case the prompt can prevent.
pub(crate) fn approval_grant_recordable(
    tool_call: &ToolCallSnapshot,
    preview: Option<&ApprovalPreview>,
) -> bool {
    matches!(
        grant_recording(&approval_grant(tool_call, preview)),
        GrantRecording::Recorded
    )
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

/// Sanitizes characters for the prompt composer.
///
/// Unlike [`terminal_safe_character`], hard newlines are preserved so Shift-Enter
/// and multiline paste can build multi-line prompts. Carriage returns are dropped
/// so CRLF paste collapses to a single newline.
fn composer_character(character: char) -> Option<char> {
    match character {
        '\n' => Some('\n'),
        '\r' => None,
        character => terminal_safe_character(character),
    }
}

/// Keys that insert a hard newline in the composer without submitting.
///
/// Shift-Enter is the primary chord. Without kitty keyboard enhancement many
/// terminals cannot report Shift on Enter, so Alt-Enter and Ctrl-J (the raw
/// line-feed / historical newline) are accepted as fallbacks.
fn is_composer_newline_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT),
        // In raw mode a bare LF arrives as Ctrl-J rather than Enter.
        KeyCode::Char('j' | 'J') if key.modifiers == KeyModifiers::CONTROL => true,
        KeyCode::Char('\n') => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests;

impl App {
    /// Move the transcript cursor to the adjacent tool call of the focused
    /// session in transcript order (the order the server persisted them).
    /// From no selection, up starts at the newest call and down at the oldest.
    fn move_transcript_cursor(&mut self, down: bool) -> bool {
        let Some(session) = self.focused().and_then(|id| self.sessions.get(&id)) else {
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

    /// What Enter does in the composer right now, for the prompt glyph.
    pub(crate) fn composer_mode(&self) -> crate::view::ComposerMode {
        use crate::view::ComposerMode;
        if self.pending_approval().is_some() {
            return ComposerMode::Approval;
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
