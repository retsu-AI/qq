//! One open project: a client of the QQ server for one workspace.
//!
//! Each project runs an actor task that owns a `TuiClient` (the reconnecting
//! protocol driver), a `SessionStore` (the shared reducer), and the UI's
//! focus. It applies server updates exactly as the TUI does, executes the
//! UI's commands, and emits a coalesced `ProjectStateDto` at most once per
//! frame so streaming text never floods the webview.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use qq_client::{
    ClientPort, ClientRequest, ClientUpdate, ConnectionState, InitialSession, TuiClient,
    state::{
        ApprovalPreview, ModelOption, NoticeLevel, ReduceContext, SessionStore, StateEffect,
        body_request,
    },
};
use qq_protocol::{
    ApprovalDecision, ApprovalGrant, ApprovalMode, CommandId, CommandOutcome, CommandRequest,
    Correlation, InputPart, ModelSelection, RunLimits, ServerCapabilities, SessionCommand,
    SessionCommandKind, SessionEvent, SessionEventEnvelope, SessionId, ToolCallId,
    ToolCallSnapshot, WorkspaceId, WorkspaceSnapshot,
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{mpsc, oneshot};

use crate::{
    engine::Engine,
    view::{self, ConnectionDto, ModelDto, NoticeDto, ProjectStateDto, SkillDto},
};

/// Webview frame budget for coalesced state pushes.
const EMIT_INTERVAL: Duration = Duration::from_millis(33);
/// Informational notices clear themselves; errors stay until replaced.
const NOTICE_TTL: Duration = Duration::from_secs(6);
/// Events kept to replay onto a body snapshot that arrives late.
const MAX_RECENT_EVENTS: usize = 512;
/// The server's per-grant byte cap (`qq_core::MAX_GRANT_BYTES`).
const MAX_GRANT_BYTES: usize = 256;
const MAX_COMMANDS: usize = 64;

type SubmissionReply = oneshot::Sender<Result<(), String>>;

#[derive(Debug, Default, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitMode {
    #[default]
    Queue,
    Steer,
    Interrupt,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SessionOption {
    Profile(qq_protocol::AgentProfileId),
    Effort(Option<qq_protocol::ReasoningEffort>),
    Delegate(Option<qq_protocol::ApprovalDelegate>),
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Compact,
    Rollback,
    Prune,
    Child,
}

pub const STATE_EVENT: &str = "project-state";

/// What the UI asks a project to do.
#[derive(Debug)]
pub enum UiCommand {
    Focus(Option<SessionId>),
    NewSession,
    Submit {
        session_id: Option<SessionId>,
        input: Vec<InputPart>,
        reply: SubmissionReply,
        mode: SubmitMode,
    },
    Cancel(SessionId),
    /// Ask beside the run without touching its context. `new_thread` starts
    /// a fresh side thread instead of continuing the last one.
    SideQuestion {
        session_id: SessionId,
        question: String,
        new_thread: bool,
    },
    /// Cancel the session's running side question, not its run.
    CancelSideQuestion(SessionId),
    Approve {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        decision: ApprovalChoice,
    },
    SetApprovalMode(SessionId, ApprovalMode),
    SetOption(SessionId, SessionOption),
    Action(Option<SessionId>, SessionAction),
    SelectModel(String),
    Delete(SessionId),
    /// Re-emit the full state (a webview reload lost it).
    Refresh,
    /// Re-read the model catalog and capabilities: a provider credential or
    /// the project's trusted configuration changed.
    RefreshModels,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalChoice {
    Once,
    Session,
    Workspace,
    Deny,
    Answer { answers: Vec<String> },
}

#[derive(Clone)]
pub struct ProjectHandle {
    /// The canonical workspace path the server knows this project by.
    pub path: PathBuf,
    commands: mpsc::Sender<UiCommand>,
    _lifetime: Arc<ActorLifetime>,
}

struct ActorLifetime(Arc<tokio::sync::Notify>);

impl Drop for ActorLifetime {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl ProjectHandle {
    pub fn send(&self, command: UiCommand) -> Result<(), String> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => "project is busy; try again".to_owned(),
                mpsc::error::TrySendError::Closed(_) => "project is closed".to_owned(),
            })
    }
}

pub fn open(
    app: AppHandle,
    engine: Arc<Engine>,
    attentive: Arc<AtomicBool>,
    id: String,
    path: PathBuf,
    preferred_model: Option<String>,
) -> ProjectHandle {
    let (commands, receiver) = mpsc::channel(MAX_COMMANDS);
    let stopped = Arc::new(tokio::sync::Notify::new());
    let lifetime = Arc::new(ActorLifetime(Arc::clone(&stopped)));
    let handle = ProjectHandle {
        path: path.clone(),
        commands,
        _lifetime: lifetime,
    };
    tauri::async_runtime::spawn(async move {
        let mut project = Project::new(Some(app), engine, attentive, id, path, preferred_model);
        tokio::select! {
            _ = project.run(receiver) => {},
            _ = stopped.notified() => {},
        }
    });
    handle
}

enum PendingIntent {
    /// A session this client asked for, with the prompt to submit once it
    /// exists.
    Create {
        draft: Option<Vec<InputPart>>,
        reply: Option<SubmissionReply>,
    },
    Prompt {
        session_id: SessionId,
        reply: Option<SubmissionReply>,
        adopt: bool,
    },
    Other,
}

impl PendingIntent {
    fn finish(self, result: Result<(), String>) {
        let reply = match self {
            Self::Create { reply, .. } | Self::Prompt { reply, .. } => reply,
            Self::Other => None,
        };
        if let Some(reply) = reply {
            // A closed webview can abandon its waiter; the receipt remains authoritative.
            let _ = reply.send(result);
        }
    }
}

struct Notice {
    level: NoticeLevel,
    text: String,
    at: Instant,
}

struct Project {
    app: Option<AppHandle>,
    engine: Arc<Engine>,
    attentive: Arc<AtomicBool>,
    id: String,
    path: PathBuf,
    connection: ConnectionDto,
    error: Option<String>,
    workspace_id: Option<WorkspaceId>,
    last_sequence: u64,
    recent_events: VecDeque<SessionEventEnvelope>,
    sessions: SessionStore,
    focused: Option<SessionId>,
    focus_clock: u64,
    models: Vec<ModelOption>,
    selected_model: Option<String>,
    capabilities: Option<Arc<ServerCapabilities>>,
    pending: HashMap<CommandId, PendingIntent>,
    notice: Option<Notice>,
    /// A goal's interrupted completion check holds the workspace until it is
    /// acknowledged (from the TUI or `qq goal`); shown as a notice on change.
    check_interruption: Option<qq_protocol::CheckInterruption>,
    dirty: bool,
    last_emitted: Option<ProjectStateDto>,
}

impl Project {
    fn new(
        app: Option<AppHandle>,
        engine: Arc<Engine>,
        attentive: Arc<AtomicBool>,
        id: String,
        path: PathBuf,
        preferred_model: Option<String>,
    ) -> Self {
        Self {
            app,
            engine,
            attentive,
            id,
            path,
            connection: ConnectionDto::Starting,
            error: None,
            workspace_id: None,
            last_sequence: 0,
            recent_events: VecDeque::new(),
            sessions: SessionStore::default(),
            focused: None,
            focus_clock: 0,
            models: Vec::new(),
            selected_model: preferred_model,
            capabilities: None,
            pending: HashMap::new(),
            notice: None,
            check_interruption: None,
            dirty: true,
            last_emitted: None,
        }
    }

    async fn run(&mut self, mut commands: mpsc::Receiver<UiCommand>) {
        self.emit();
        loop {
            let mut client = match self.connect().await {
                Ok(client) => client,
                Err(error) => {
                    if !self.wait_for_retry(&mut commands, error).await {
                        return;
                    }
                    continue;
                }
            };
            let mut tick = tokio::time::interval(EMIT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    update = client.recv() => match update {
                        Some(update) => self.apply_update(&client, update),
                        None => {
                            let error = self.notice.as_ref().map(|notice| notice.text.clone()).unwrap_or_else(|| "the qq client stopped".to_owned());
                            for (_, intent) in self.pending.drain() {
                                intent.finish(Err("connection ended before acknowledgement; check history before retrying".to_owned()));
                            }
                            if !self.wait_for_retry(&mut commands, error).await { return; }
                            break;
                        }
                    },
                    command = commands.recv() => match command {
                        Some(command) => self.apply_command(&client, command),
                        None => return,
                    },
                    _ = tick.tick() => {
                        if self.notice.as_ref().is_some_and(|notice| notice.level != NoticeLevel::Error && notice.at.elapsed() > NOTICE_TTL) {
                            self.notice = None;
                            self.dirty = true;
                        }
                        if self.dirty { self.emit(); }
                    }
                }
            }
        }
    }

    async fn wait_for_retry(
        &mut self,
        commands: &mut mpsc::Receiver<UiCommand>,
        error: String,
    ) -> bool {
        self.connection = ConnectionDto::Failed;
        self.error = Some(error);
        self.emit();
        loop {
            match commands.recv().await {
                Some(UiCommand::Refresh) => {
                    self.connection = ConnectionDto::Starting;
                    self.error = None;
                    self.clear_error_notice();
                    self.workspace_id = None;
                    self.last_sequence = 0;
                    self.recent_events.clear();
                    self.sessions.clear();
                    self.focused = None;
                    self.emit();
                    return true;
                }
                Some(UiCommand::Submit { reply, .. }) => {
                    let _ = reply.send(Err("engine is unavailable; retry connection".to_owned()));
                }
                Some(_) => {}
                None => return false,
            }
        }
    }

    async fn connect(&mut self) -> Result<TuiClient, String> {
        let connection = self
            .engine
            .ensure()
            .await
            .map_err(|error| error.to_string())?;
        self.connection = ConnectionDto::Connecting;
        self.dirty = true;
        let engine = Arc::clone(&self.engine);
        TuiClient::start(
            connection,
            self.path.clone(),
            ModelSelection {
                model: self.selected_model.clone(),
                ..ModelSelection::default()
            },
            InitialSession::Existing,
            move || {
                let engine = Arc::clone(&engine);
                async move { engine.ensure().await.ok() }
            },
        )
        .map_err(|error| error.to_string())
    }

    // ---- server updates ------------------------------------------------

    fn apply_update(&mut self, client: &impl ClientPort, update: ClientUpdate) {
        self.dirty = true;
        match update {
            ClientUpdate::Connection(state) => {
                self.connection = match state {
                    ConnectionState::Connecting => ConnectionDto::Connecting,
                    ConnectionState::Replaying => ConnectionDto::Replaying,
                    ConnectionState::Live => ConnectionDto::Live,
                    ConnectionState::Offline => ConnectionDto::Offline,
                };
                if state == ConnectionState::Live {
                    self.error = None;
                }
            }
            ClientUpdate::Snapshot(snapshot) => self.apply_snapshot(client, snapshot),
            ClientUpdate::ResetSnapshot(snapshot) => {
                self.workspace_id = None;
                self.sessions.clear();
                self.focused = None;
                self.last_sequence = 0;
                self.recent_events.clear();
                self.set_notice(
                    NoticeLevel::Warning,
                    "session state reset after reconnecting",
                );
                self.apply_snapshot(client, snapshot);
            }
            ClientUpdate::Models { models, selected } => {
                self.models = models.into_iter().map(ModelOption::from).collect();
                self.sessions.apply_models(&self.models);
                // Keep the user's pick while it stays usable; otherwise the
                // configured route, the newest session's model, a provider's
                // recommended model, then whatever the catalog lists first.
                let valid = |id: &String| self.models.iter().any(|m| &view::model_id(m) == id);
                let recommended = self.models.iter().find_map(|option| {
                    crate::auth::recommended_model(&option.provider).filter(|id| valid(id))
                });
                self.selected_model = self
                    .selected_model
                    .clone()
                    .filter(valid)
                    .or_else(|| selected.and_then(|selection| selection.model).filter(valid))
                    .or_else(|| self.most_recent_session_model().filter(valid))
                    .or(recommended)
                    .or_else(|| self.models.first().map(view::model_id));
            }
            ClientUpdate::Capabilities(capabilities) => self.capabilities = Some(capabilities),
            ClientUpdate::Event(event) => self.apply_live_event(client, event),
            ClientUpdate::CommandResult { command_id, result } => {
                let intent = self.pending.remove(&command_id);
                match result {
                    Ok(receipt) => match (&receipt.outcome, intent) {
                        (
                            CommandOutcome::SessionCreated { session_id },
                            Some(PendingIntent::Create { draft, reply }),
                        ) => {
                            if let Some(text) = draft {
                                self.submit(client, *session_id, text, reply, true);
                            } else {
                                self.adopt_created(client, *session_id);
                                if let Some(reply) = reply {
                                    let _ = reply.send(Ok(()));
                                }
                            }
                        }
                        (
                            _,
                            Some(
                                intent @ PendingIntent::Prompt {
                                    session_id,
                                    adopt: true,
                                    ..
                                },
                            ),
                        ) => {
                            self.adopt_created(client, session_id);
                            intent.finish(Ok(()));
                        }
                        (_, Some(intent)) => intent.finish(Ok(())),
                        (_, None) => {}
                    },
                    Err(failure) => {
                        if let Some(intent) = intent {
                            intent.finish(Err(failure.message().to_owned()));
                        }
                        self.set_notice(NoticeLevel::Error, failure.message());
                    }
                }
            }
            ClientUpdate::SnapshotFailed(failure) => {
                self.set_notice(NoticeLevel::Error, failure.message());
            }
        }
    }

    fn apply_snapshot(&mut self, client: &impl ClientPort, snapshot: WorkspaceSnapshot) {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            return;
        }
        let snapshot_focus = snapshot.focused.as_ref().map(|focused| focused.summary.id);
        let sequence = snapshot.cursor.sequence;
        if initial {
            self.workspace_id = Some(snapshot.workspace.id);
        }
        if initial || sequence >= self.last_sequence {
            for summary in snapshot.sessions {
                self.sessions
                    .upsert_summary(summary, &self.models, sequence);
            }
        }
        for body in snapshot.included {
            self.sessions
                .install_session_snapshot(body, &self.models, sequence);
        }
        if let Some(focused) = snapshot.focused {
            self.sessions
                .install_session_snapshot(focused, &self.models, sequence);
        }
        if initial {
            self.last_sequence = sequence;
        }
        // Events that raced past the snapshot for the body it carried.
        let replay: Vec<_> = self
            .recent_events
            .iter()
            .filter(|event| {
                event.cursor.sequence > sequence && snapshot_focus == Some(event.session_id)
            })
            .cloned()
            .collect();
        for event in replay {
            self.reduce_event(client, &event);
        }
        self.sessions.evict_cold_bodies(self.focused);
    }

    fn apply_live_event(&mut self, client: &impl ClientPort, event: SessionEventEnvelope) {
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != event.cursor.workspace_id)
        {
            return;
        }
        if event.cursor.sequence <= self.last_sequence {
            return;
        }
        if self.last_sequence != 0 && event.cursor.sequence != self.last_sequence + 1 {
            // The client loop replays from its cursor; a gap means it will.
            self.connection = ConnectionDto::Replaying;
            return;
        }
        self.workspace_id.get_or_insert(event.cursor.workspace_id);
        self.last_sequence = event.cursor.sequence;
        let already_loaded = self
            .sessions
            .get(&event.session_id)
            .is_some_and(|session| event.cursor.sequence <= session.loaded_through);
        if !already_loaded {
            self.reduce_event(client, &event);
        }
        if let Some(command_id) = event.caused_by
            && matches!(self.pending.get(&command_id), Some(PendingIntent::Other))
        {
            self.pending.remove(&command_id);
        }
        self.recent_events.push_back(event);
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
    }

    fn reduce_event(&mut self, client: &impl ClientPort, envelope: &SessionEventEnvelope) {
        let caused_by_me = envelope
            .caused_by
            .and_then(|id| self.pending.get(&id))
            .is_some_and(|intent| matches!(intent, PendingIntent::Create { .. }));
        let effects = self.sessions.reduce_event(
            envelope,
            ReduceContext {
                focused: self.focused,
                attentive: self.attentive.load(Ordering::Relaxed),
                workspace_id: self.workspace_id,
                capabilities: self.capabilities.as_ref(),
                models: &self.models,
                caused_by_me,
            },
        );
        for effect in effects {
            match effect {
                StateEffect::CheckInterruptionChanged(interruption) => {
                    if self.check_interruption != interruption {
                        self.check_interruption = interruption;
                        if self.check_interruption.is_some() {
                            self.set_notice(
                                NoticeLevel::Warning,
                                "An interrupted goal completion check holds this workspace until it is acknowledged with `qq goal` or in the TUI.",
                            );
                        } else {
                            self.set_notice(
                                NoticeLevel::Info,
                                "Completion check interruption acknowledged; workspace work may continue.",
                            );
                        }
                    }
                }
                StateEffect::Notice { level, text, .. } => self.set_notice(level, text),
                StateEffect::Attention(attention) => {
                    if let Some(app) = &self.app {
                        let _ = app.emit("attention", attention.summary());
                        if app
                            .state::<crate::AppState>()
                            .notifications_enabled
                            .load(Ordering::Relaxed)
                            && !self.attentive.load(Ordering::Relaxed)
                            && let Err(error) = app
                                .notification()
                                .builder()
                                .title("QQ")
                                .body(attention.summary())
                                .show()
                        {
                            self.set_notice(
                                NoticeLevel::Warning,
                                format!("notification could not be shown: {error}"),
                            );
                        }
                    }
                }
                StateEffect::RequestSnapshot(request) => {
                    self.send(client, ClientRequest::Snapshot(request));
                }
                StateEffect::Refocus(target) => self.focus(client, target),
                // Adoption waits for the command receipt, which carries the
                // queued draft; the event can arrive first.
                StateEffect::AdoptCreated(_) => {}
                StateEffect::SubmitDraft { session_id, text } => {
                    self.submit(client, session_id, vec![InputPart::text(text)], None, false);
                }
                StateEffect::SessionRemoved { session_id, .. } => {
                    self.pending.retain(|_, intent| {
                        !matches!(intent, PendingIntent::Prompt { session_id: s, reply: None, .. } if *s == session_id)
                    });
                }
            }
        }
        if matches!(envelope.event, SessionEvent::SessionDeleted { .. })
            && self.focused == Some(envelope.session_id)
        {
            self.focused = None;
        }
    }

    // ---- UI commands ---------------------------------------------------

    fn apply_command(&mut self, client: &impl ClientPort, command: UiCommand) {
        self.dirty = true;
        match command {
            UiCommand::Refresh => self.last_emitted = None,
            UiCommand::RefreshModels => {
                let selection = ModelSelection {
                    model: self.selected_model.clone(),
                    ..ModelSelection::default()
                };
                self.send(client, ClientRequest::Models(selection));
                self.send(client, ClientRequest::Capabilities);
            }
            UiCommand::Focus(target) => self.focus(client, target),
            UiCommand::NewSession => self.focused = None,
            UiCommand::Submit {
                session_id,
                input,
                reply,
                mode,
            } => {
                if let Err(error) = validate_submission(self.connection, &input) {
                    let _ = reply.send(Err(error));
                    return;
                }
                self.clear_error_notice();
                if !matches!(mode, SubmitMode::Queue) {
                    let Some(session_id) = session_id else {
                        let _ = reply.send(Err("select a session to steer".to_owned()));
                        return;
                    };
                    let result = steering_command(
                        self.sessions
                            .get(&session_id)
                            .and_then(|session| session.summary.active_run_id),
                        input,
                        mode,
                    );
                    match result {
                        Ok(command) => {
                            self.command(
                                client,
                                command,
                                PendingIntent::Prompt {
                                    session_id,
                                    reply: Some(reply),
                                    adopt: false,
                                },
                            );
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                    return;
                }
                match session_id {
                    Some(session_id) => self.submit(client, session_id, input, Some(reply), false),
                    None => self.create_session(client, Some(input), Some(reply), None),
                }
            }
            UiCommand::Cancel(session_id) => {
                let Some(run_id) = self
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.summary.active_run_id)
                else {
                    return;
                };
                self.command(
                    client,
                    SessionCommand::CancelRun { run_id },
                    PendingIntent::Other,
                );
            }
            UiCommand::SideQuestion {
                session_id,
                question,
                new_thread,
            } => self.command(
                client,
                SessionCommand::SubmitSideQuestion {
                    session_id,
                    question,
                    new_thread,
                },
                PendingIntent::Other,
            ),
            UiCommand::CancelSideQuestion(session_id) => {
                let Some(side_question_id) = self
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.summary.running_side_question)
                else {
                    return;
                };
                self.command(
                    client,
                    SessionCommand::CancelSideQuestion { side_question_id },
                    PendingIntent::Other,
                );
            }
            UiCommand::Approve {
                session_id,
                tool_call_id,
                decision,
            } => self.approve(client, session_id, tool_call_id, decision),
            UiCommand::SetApprovalMode(session_id, mode) => self.command(
                client,
                SessionCommand::SetApprovalMode { session_id, mode },
                PendingIntent::Other,
            ),
            UiCommand::SetOption(session_id, option) => {
                let command = match option {
                    SessionOption::Profile(profile) => SessionCommand::SetSessionProfile {
                        session_id,
                        profile,
                    },
                    SessionOption::Effort(effort) => {
                        SessionCommand::SetSessionEffort { session_id, effort }
                    }
                    SessionOption::Delegate(delegate) => SessionCommand::SetApprovalDelegate {
                        session_id,
                        delegate,
                    },
                };
                self.command(client, command, PendingIntent::Other);
            }
            UiCommand::Action(session_id, action) => {
                let command = match action {
                    SessionAction::Prune => self
                        .workspace_id
                        .map(|workspace_id| SessionCommand::PruneSessions { workspace_id }),
                    SessionAction::Compact => {
                        session_id.map(|session_id| SessionCommand::CompactSession { session_id })
                    }
                    SessionAction::Rollback => session_id
                        .map(|session_id| SessionCommand::RollbackCompaction { session_id }),
                    SessionAction::Child => {
                        if let Some(parent) = session_id {
                            self.create_session(client, None, None, Some(parent));
                        }
                        return;
                    }
                };
                if let Some(command) = command {
                    self.command(client, command, PendingIntent::Other);
                }
            }
            UiCommand::SelectModel(model) => {
                let Some(option) = self.models.iter().find(|m| view::model_id(m) == model) else {
                    return;
                };
                let selection = option.selection.clone();
                self.selected_model = Some(model);
                if let Some(session_id) = self.focused {
                    self.command(
                        client,
                        SessionCommand::SetSessionModel {
                            session_id,
                            model: selection,
                        },
                        PendingIntent::Other,
                    );
                }
            }
            UiCommand::Delete(session_id) => self.command(
                client,
                SessionCommand::DeleteSession { session_id },
                PendingIntent::Other,
            ),
        }
    }

    fn focus(&mut self, client: &impl ClientPort, target: Option<SessionId>) {
        self.focused = target;
        let Some(session_id) = target else { return };
        self.focus_clock += 1;
        self.sessions.mark_focused(session_id, self.focus_clock);
        let warm = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.is_warm());
        if !warm && let Some(workspace_id) = self.workspace_id {
            self.send(
                client,
                ClientRequest::Snapshot(body_request(workspace_id, session_id)),
            );
        }
        self.sessions.evict_cold_bodies(self.focused);
    }

    fn adopt_created(&mut self, client: &impl ClientPort, session_id: SessionId) {
        self.focused = Some(session_id);
        self.focus_clock += 1;
        self.sessions.mark_focused(session_id, self.focus_clock);
        self.sessions.evict_cold_bodies(self.focused);
        // A receipt can precede SessionCreated; even empty child sessions need
        // an authoritative body rather than remaining cold or losing events.
        if !self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.is_warm())
            && let Some(workspace_id) = self.workspace_id
        {
            self.send(
                client,
                ClientRequest::Snapshot(body_request(workspace_id, session_id)),
            );
        }
    }

    fn create_session(
        &mut self,
        client: &impl ClientPort,
        draft: Option<Vec<InputPart>>,
        reply: Option<SubmissionReply>,
        parent_id: Option<SessionId>,
    ) {
        let Some(workspace_id) = self.workspace_id else {
            PendingIntent::Create { draft, reply }
                .finish(Err("still connecting to the workspace".to_owned()));
            self.set_notice(NoticeLevel::Warning, "still connecting to the workspace");
            return;
        };
        let model = self
            .selected_model
            .as_ref()
            .and_then(|id| self.models.iter().find(|m| &view::model_id(m) == id))
            .map(|option| option.selection.clone())
            .unwrap_or_default();
        if model.model.is_none() {
            PendingIntent::Create { draft, reply }
                .finish(Err("no model is available; connect a provider".to_owned()));
            self.set_notice(
                NoticeLevel::Error,
                "no model is available; connect a provider from Providers in the sidebar",
            );
            return;
        }
        self.command(
            client,
            SessionCommand::CreateSession {
                workspace_id,
                parent_id,
                model,
                approval_mode: ApprovalMode::default(),
                profile: qq_protocol::AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
            PendingIntent::Create { draft, reply },
        );
    }

    fn submit(
        &mut self,
        client: &impl ClientPort,
        session_id: SessionId,
        input: Vec<InputPart>,
        reply: Option<SubmissionReply>,
        adopt: bool,
    ) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            let text = input
                .iter()
                .filter_map(|part| match part {
                    InputPart::Text { text } => Some(text.as_str()),
                    InputPart::WorkspaceFile { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            session.record_prompt(&text);
        }
        self.command(
            client,
            SessionCommand::SubmitPrompt {
                session_id,
                input,
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
            PendingIntent::Prompt {
                session_id,
                reply,
                adopt,
            },
        );
    }

    fn approve(
        &mut self,
        client: &impl ClientPort,
        session_id: SessionId,
        tool_call_id: ToolCallId,
        choice: ApprovalChoice,
    ) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let Some(call) = session
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.iter().find(|call| call.id == tool_call_id))
        else {
            return;
        };
        let grant = || approval_grant(call, session.approval_previews.get(&tool_call_id));
        let decision = match choice {
            ApprovalChoice::Once => ApprovalDecision::ApproveOnce,
            ApprovalChoice::Session => ApprovalDecision::ApproveForSession { grant: grant() },
            ApprovalChoice::Workspace => ApprovalDecision::ApproveForWorkspace { grant: grant() },
            ApprovalChoice::Deny => ApprovalDecision::Deny,
            ApprovalChoice::Answer { answers } => ApprovalDecision::Answer { answers },
        };
        let run_id = call.run_id;
        self.command(
            client,
            SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id,
                decision,
            },
            PendingIntent::Other,
        );
    }

    fn command(
        &mut self,
        client: &impl ClientPort,
        command: SessionCommand,
        intent: PendingIntent,
    ) {
        if self.pending.len() >= MAX_COMMANDS {
            intent.finish(Err(
                "too many pending commands; wait for acknowledgement".to_owned()
            ));
            self.set_notice(
                NoticeLevel::Error,
                "too many pending commands; wait for acknowledgement",
            );
            return;
        }
        let command_id = match CommandId::generate() {
            Ok(id) => id,
            Err(error) => {
                intent.finish(Err(error.to_string()));
                self.set_notice(NoticeLevel::Error, error.to_string());
                return;
            }
        };
        self.pending.insert(command_id, intent);
        if !self.send(
            client,
            ClientRequest::Command(CommandRequest {
                command_id,
                command,
            }),
        ) && let Some(intent) = self.pending.remove(&command_id)
        {
            intent.finish(Err(
                "command was not sent; try again when connected".to_owned()
            ));
        }
    }

    fn send(&mut self, client: &impl ClientPort, request: ClientRequest) -> bool {
        match client.try_send(request) {
            Ok(()) => true,
            Err(failure) => {
                self.set_notice(NoticeLevel::Error, failure.message());
                false
            }
        }
    }

    // ---- presentation --------------------------------------------------

    fn most_recent_session_model(&self) -> Option<String> {
        self.sessions
            .values()
            .filter(|session| session.summary.model.is_some())
            .max_by_key(|session| session.summary.updated_at_ms)
            .and_then(|session| session.summary.model.clone())
    }

    fn set_notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notice = Some(Notice {
            level,
            text: text.into(),
            at: Instant::now(),
        });
        self.dirty = true;
    }

    fn clear_error_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.level == NoticeLevel::Error)
        {
            self.notice = None;
        }
    }

    fn state(&self) -> ProjectStateDto {
        ProjectStateDto {
            project_id: self.id.clone(),
            path: view::display_path(&self.path),
            name: view::display_name(&self.path),
            connection: self.connection,
            error: self.error.clone(),
            owns_server: self.engine.owns_server(),
            sessions: view::session_rows(&self.sessions),
            focused: self.focused,
            transcript: self
                .focused
                .and_then(|id| view::transcript(&self.sessions, id)),
            models: self.models.iter().map(ModelDto::from_option).collect(),
            selected_model: self.selected_model.clone(),
            can_steer: self
                .capabilities
                .as_ref()
                .is_some_and(|caps| caps.steering.boundary),
            can_interrupt: self
                .capabilities
                .as_ref()
                .is_some_and(|caps| caps.steering.interrupt),
            profiles: self
                .capabilities
                .as_ref()
                .and_then(|caps| caps.profiles.as_ref())
                .map(|profiles| {
                    profiles
                        .iter()
                        .map(|profile| profile.id.as_str().to_owned())
                        .collect()
                })
                .unwrap_or_default(),
            tool_hosts: self
                .capabilities
                .as_ref()
                .and_then(|caps| caps.workspace_tools.as_ref())
                .map(|tools| tools.hosts.clone())
                .unwrap_or_default(),
            skills: self
                .capabilities
                .as_ref()
                .and_then(|caps| caps.workspace_tools.as_ref())
                .map(|tools| {
                    tools
                        .skills
                        .entries
                        .iter()
                        .map(SkillDto::from_summary)
                        .collect()
                })
                .unwrap_or_default(),
            can_side_question: self.capabilities.as_ref().is_some_and(|caps| {
                caps.commands
                    .contains(&SessionCommandKind::SubmitSideQuestion)
            }),
            notice: self.notice.as_ref().map(|notice| NoticeDto {
                level: match notice.level {
                    NoticeLevel::Info => "info",
                    NoticeLevel::Warning => "warning",
                    NoticeLevel::Error => "error",
                },
                text: notice.text.clone(),
            }),
        }
    }

    fn emit(&mut self) {
        self.dirty = false;
        let state = self.state();
        if self.last_emitted.as_ref() == Some(&state) {
            return;
        }
        if let Some(app) = &self.app {
            let _ = app.emit(STATE_EVENT, &state);
        }
        self.last_emitted = Some(state);
    }
}

fn steering_command(
    run_id: Option<qq_protocol::RunId>,
    input: Vec<InputPart>,
    mode: SubmitMode,
) -> Result<SessionCommand, String> {
    let Some(run_id) = run_id else {
        return Err("the run has finished; queue a follow-up instead".to_owned());
    };
    Ok(SessionCommand::SteerRun {
        run_id,
        input,
        interrupt: matches!(mode, SubmitMode::Interrupt),
    })
}

fn validate_submission(connection: ConnectionDto, input: &[InputPart]) -> Result<(), String> {
    if connection != ConnectionDto::Live {
        return Err("wait until the project is connected before sending".to_owned());
    }
    qq_protocol::validate_input(input).map_err(|error| error.to_string())
}

/// The approve-for-session/workspace grant for a held call: shell calls
/// allowlist their exact command as a prefix, fetch calls the host the server
/// judged, everything else the tool. Mirrors the TUI's derivation.
pub fn approval_grant(call: &ToolCallSnapshot, preview: Option<&ApprovalPreview>) -> ApprovalGrant {
    if let Some(fetch) = preview.and_then(|preview| preview.fetch.as_ref()) {
        return ApprovalGrant::Host {
            host: fetch.host.clone(),
        };
    }
    if call.name == "shell"
        && let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.arguments)
        && let Some(command) = arguments.get("command").and_then(|value| value.as_str())
        && !command.trim().is_empty()
    {
        return ApprovalGrant::ShellPrefix {
            prefix: command.to_owned(),
        };
    }
    ApprovalGrant::Tool {
        name: call.name.clone(),
    }
}

fn grant_value(grant: &ApprovalGrant) -> &str {
    match grant {
        ApprovalGrant::Tool { name } => name,
        ApprovalGrant::ShellPrefix { prefix } => prefix,
        ApprovalGrant::Host { host } => host,
    }
}

pub fn grant_recordable(grant: &ApprovalGrant) -> bool {
    let value = grant_value(grant).trim();
    !value.is_empty() && value.len() <= MAX_GRANT_BYTES
}

pub fn grant_label(grant: &ApprovalGrant) -> String {
    match grant {
        ApprovalGrant::Tool { name } => format!("the {name} tool"),
        ApprovalGrant::ShellPrefix { prefix } => format!("commands starting with `{prefix}`"),
        ApprovalGrant::Host { host } => format!("requests to {host}"),
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;

    #[tokio::test]
    async fn closing_the_last_handle_cancels_an_actor_even_during_connect() {
        let stopped = Arc::new(tokio::sync::Notify::new());
        let lifetime = Arc::new(ActorLifetime(Arc::clone(&stopped)));
        let temporary_handle = Arc::clone(&lifetime);
        drop(temporary_handle);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), stopped.notified())
                .await
                .is_err()
        );
        drop(lifetime);
        tokio::time::timeout(Duration::from_millis(100), stopped.notified())
            .await
            .expect("final handle stops the actor");
    }

    #[tokio::test]
    async fn failed_bootstrap_rejects_submission_and_retry_clears_stale_identity() {
        let mut project = Project::new(
            None,
            Arc::new(Engine::default()),
            Arc::new(AtomicBool::new(true)),
            "retry".into(),
            PathBuf::from("."),
            None,
        );
        project.workspace_id = Some(WorkspaceId::from_bytes([2; 16]));
        project.last_sequence = 90;
        project.set_notice(NoticeLevel::Error, "old bootstrap failure");
        let (commands, mut receiver) = mpsc::channel(2);
        let (reply, accepted) = oneshot::channel();
        commands
            .try_send(UiCommand::Submit {
                session_id: None,
                input: vec![InputPart::text("keep")],
                reply,
                mode: SubmitMode::Queue,
            })
            .unwrap();
        commands.try_send(UiCommand::Refresh).unwrap();
        assert!(project.wait_for_retry(&mut receiver, "failed".into()).await);
        assert!(accepted.await.unwrap().is_err());
        assert_eq!(project.connection, ConnectionDto::Starting);
        assert!(project.workspace_id.is_none());
        assert_eq!(project.last_sequence, 0);
        assert!(project.notice.is_none());
    }

    #[tokio::test]
    async fn steering_without_a_selected_session_replies_with_an_actionable_error() {
        for mode in [SubmitMode::Steer, SubmitMode::Interrupt] {
            let client = Client::default();
            let mut project = Project::new(
                None,
                Arc::new(Engine::default()),
                Arc::new(AtomicBool::new(true)),
                "steering".into(),
                PathBuf::from("."),
                None,
            );
            project.connection = ConnectionDto::Live;
            let (reply, accepted) = oneshot::channel();
            project.apply_command(
                &client,
                UiCommand::Submit {
                    session_id: None,
                    input: vec![InputPart::text("steer")],
                    reply,
                    mode,
                },
            );
            assert_eq!(
                accepted.await.expect("submission waiter receives a reply"),
                Err("select a session to steer".to_owned())
            );
            assert!(client.requests.lock().unwrap().is_empty());
            assert!(project.pending.is_empty());
        }
    }

    #[test]
    fn session_options_reject_invalid_profile_and_effort_values() {
        assert!(
            serde_json::from_str::<SessionOption>(r#"{"type":"profile","value":"../escape"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<SessionOption>(r#"{"type":"effort","value":"unbounded"}"#)
                .is_err()
        );
        for value in ["on", "off", "by_mode"] {
            assert!(
                serde_json::from_value::<SessionOption>(
                    serde_json::json!({"type":"delegate","value":value})
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn empty_child_adoption_requests_a_body_when_receipt_precedes_summary() {
        let client = Client::default();
        let mut project = Project::new(
            None,
            Arc::new(Engine::default()),
            Arc::new(AtomicBool::new(true)),
            "child".into(),
            PathBuf::from("."),
            None,
        );
        let workspace_id = WorkspaceId::from_bytes([2; 16]);
        let session_id = SessionId::from_bytes([3; 16]);
        project.workspace_id = Some(workspace_id);
        project.adopt_created(&client, session_id);
        assert_eq!(project.focused, Some(session_id));
        assert!(client.requests.lock().unwrap().iter().any(|request| matches!(request, ClientRequest::Snapshot(snapshot) if snapshot.workspace_id == workspace_id && snapshot.focused_session_id == Some(session_id))));
    }

    #[derive(Default)]
    struct Client {
        requests: std::sync::Mutex<Vec<ClientRequest>>,
    }

    impl ClientPort for Client {
        fn try_send(&self, request: ClientRequest) -> Result<(), qq_client::ClientFailure> {
            self.requests.lock().unwrap().push(request);
            Ok(())
        }
        async fn recv(&mut self) -> Option<ClientUpdate> {
            None
        }
    }

    #[test]
    fn receipt_before_created_event_recovers_committed_prompt_from_snapshot() {
        let client = Client::default();
        let mut project = Project::new(
            None,
            Arc::new(Engine::default()),
            Arc::new(AtomicBool::new(true)),
            "test".into(),
            PathBuf::from("."),
            None,
        );
        let event: SessionEventEnvelope = serde_json::from_str(include_str!(
            "../../../../crates/qq-protocol/tests/fixtures/v32/event_prompt_queued.json"
        ))
        .unwrap();
        let SessionEvent::PromptQueued {
            session,
            message,
            run,
            ..
        } = event.event.clone()
        else {
            panic!("prompt fixture")
        };
        let session_id = session.id;
        project.workspace_id = Some(session.workspace_id);
        let create_id = CommandId::from_bytes([9; 16]);
        let (reply, mut accepted) = oneshot::channel();
        project.pending.insert(
            create_id,
            PendingIntent::Create {
                draft: Some(vec![InputPart::text("hello")]),
                reply: Some(reply),
            },
        );
        project.apply_update(
            &client,
            ClientUpdate::CommandResult {
                command_id: create_id,
                result: Ok(qq_protocol::CommandReceipt {
                    command_id: create_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionCreated { session_id },
                }),
            },
        );
        let prompt_id = client
            .requests
            .lock()
            .unwrap()
            .iter()
            .find_map(|request| match request {
                ClientRequest::Command(command) => Some(command.command_id),
                _ => None,
            })
            .unwrap();
        let mut created = event.clone();
        created.cursor.sequence = 9;
        created.caused_by = Some(create_id);
        created.event = SessionEvent::SessionCreated {
            session: session.clone(),
        };
        project.apply_live_event(&client, created);
        project.apply_live_event(&client, event.clone());
        assert!(!project.sessions.get(&session_id).unwrap().is_warm());
        project.apply_update(
            &client,
            ClientUpdate::CommandResult {
                command_id: prompt_id,
                result: Ok(qq_protocol::CommandReceipt {
                    command_id: prompt_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::PromptQueued {
                        session_id,
                        run_id: run.id,
                        queue_position: 1,
                    },
                }),
            },
        );
        assert_eq!(accepted.try_recv().unwrap(), Ok(()));
        assert!(client.requests.lock().unwrap().iter().any(|request| matches!(request, ClientRequest::Snapshot(snapshot) if snapshot.focused_session_id == Some(session_id))));
        project.apply_snapshot(
            &client,
            WorkspaceSnapshot {
                cursor: event.cursor,
                workspace: qq_protocol::WorkspaceSummary {
                    id: session.workspace_id,
                    path: ".".into(),
                },
                sessions: vec![*session.clone()],
                focused: Some(qq_protocol::SessionSnapshot {
                    goal: None,
                    side_questions: vec![],
                    summary: *session,
                    messages: vec![message],
                    runs: vec![*run],
                    tool_calls: vec![],
                    has_older_tool_calls: false,
                    has_older_messages: false,
                }),
                included: vec![],
                has_older_sessions: false,
                check_interruption: None,
            },
        );
        let transcript = project.state().transcript.unwrap();
        assert!(transcript.items.iter().any(|item| matches!(item, view::ItemDto::User { text, .. } if text.contains("Fix the login redirect"))));
    }

    #[test]
    fn steering_targets_the_active_run_and_rejects_finished_runs() {
        let run_id = qq_protocol::RunId::from_bytes([1; 16]);
        for (mode, expected) in [(SubmitMode::Steer, false), (SubmitMode::Interrupt, true)] {
            assert!(
                matches!(steering_command(Some(run_id), vec![InputPart::text("change course")], mode).unwrap(), SessionCommand::SteerRun { run_id: id, interrupt, .. } if id == run_id && interrupt == expected)
            );
            assert!(steering_command(None, vec![InputPart::text("change course")], mode).is_err());
        }
    }

    #[test]
    fn queue_is_bounded_and_reports_closed_projects() {
        let (commands, receiver) = mpsc::channel(MAX_COMMANDS);
        let handle = ProjectHandle {
            path: PathBuf::new(),
            commands,
            _lifetime: Arc::new(ActorLifetime(Arc::new(tokio::sync::Notify::new()))),
        };
        for _ in 0..MAX_COMMANDS {
            assert!(handle.send(UiCommand::Refresh).is_ok());
        }
        assert!(
            handle
                .send(UiCommand::Refresh)
                .unwrap_err()
                .contains("busy")
        );
        drop(receiver);
        assert!(
            handle
                .send(UiCommand::Refresh)
                .unwrap_err()
                .contains("closed")
        );
    }

    #[test]
    fn disconnected_empty_and_oversize_submissions_are_rejected() {
        for state in [
            ConnectionDto::Starting,
            ConnectionDto::Connecting,
            ConnectionDto::Replaying,
            ConnectionDto::Offline,
            ConnectionDto::Failed,
        ] {
            assert!(validate_submission(state, &[InputPart::text("hello")]).is_err());
        }
        assert!(validate_submission(ConnectionDto::Live, &[InputPart::text("  ")]).is_err());
        assert!(
            validate_submission(
                ConnectionDto::Live,
                &[InputPart::text(
                    "x".repeat(qq_protocol::MAX_INPUT_TEXT_BYTES + 1)
                )]
            )
            .is_err()
        );
        assert!(validate_submission(ConnectionDto::Live, &[InputPart::text("hello")]).is_ok());
    }

    #[test]
    fn submission_waiter_receives_failure_or_durable_acceptance() {
        for result in [Ok(()), Err("server rejected input".to_owned())] {
            let (reply, mut receipt) = oneshot::channel();
            let intent = PendingIntent::Create {
                draft: Some(vec![InputPart::text("keep my draft")]),
                reply: Some(reply),
            };
            assert!(matches!(
                receipt.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            intent.finish(result.clone());
            assert_eq!(receipt.try_recv().unwrap(), result);
        }
    }
}
