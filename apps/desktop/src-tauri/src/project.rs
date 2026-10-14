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
    SessionEvent, SessionEventEnvelope, SessionId, ToolCallId, ToolCallSnapshot, WorkspaceId,
    WorkspaceSnapshot,
};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use crate::{
    engine::Engine,
    view::{self, ConnectionDto, ModelDto, NoticeDto, ProjectStateDto},
};

/// Webview frame budget for coalesced state pushes.
const EMIT_INTERVAL: Duration = Duration::from_millis(33);
/// Informational notices clear themselves; errors stay until replaced.
const NOTICE_TTL: Duration = Duration::from_secs(6);
/// Events kept to replay onto a body snapshot that arrives late.
const MAX_RECENT_EVENTS: usize = 512;
/// The server's per-grant byte cap (`qq_core::MAX_GRANT_BYTES`).
const MAX_GRANT_BYTES: usize = 256;

pub const STATE_EVENT: &str = "project-state";

/// What the UI asks a project to do.
#[derive(Debug)]
pub enum UiCommand {
    Focus(Option<SessionId>),
    NewSession,
    Submit {
        session_id: Option<SessionId>,
        text: String,
    },
    Cancel(SessionId),
    Approve {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        decision: ApprovalChoice,
    },
    SetApprovalMode(SessionId, ApprovalMode),
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
    commands: mpsc::UnboundedSender<UiCommand>,
}

impl ProjectHandle {
    pub fn send(&self, command: UiCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "project is closed".to_owned())
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
    let (commands, receiver) = mpsc::unbounded_channel();
    let handle = ProjectHandle {
        path: path.clone(),
        commands,
    };
    tauri::async_runtime::spawn(async move {
        let mut project = Project::new(app, engine, attentive, id, path, preferred_model);
        project.run(receiver).await;
    });
    handle
}

enum PendingIntent {
    /// A session this client asked for, with the prompt to submit once it
    /// exists.
    Create {
        draft: Option<String>,
    },
    Prompt {
        session_id: SessionId,
    },
    Other,
}

struct Notice {
    level: NoticeLevel,
    text: String,
    at: Instant,
}

struct Project {
    app: AppHandle,
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
    dirty: bool,
    last_emitted: Option<ProjectStateDto>,
}

impl Project {
    fn new(
        app: AppHandle,
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
            dirty: true,
            last_emitted: None,
        }
    }

    async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<UiCommand>) {
        self.emit();
        let mut client = match self.connect().await {
            Ok(client) => client,
            Err(error) => {
                self.connection = ConnectionDto::Failed;
                self.error = Some(error);
                self.emit();
                // Keep answering refreshes so the UI can show the failure.
                while let Some(command) = commands.recv().await {
                    if matches!(command, UiCommand::Refresh) {
                        self.last_emitted = None;
                        self.emit();
                    }
                }
                return;
            }
        };
        let mut tick = tokio::time::interval(EMIT_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                update = client.recv() => match update {
                    Some(update) => self.apply_update(&client, update),
                    None => {
                        self.connection = ConnectionDto::Offline;
                        self.error = Some("the qq client stopped".to_owned());
                        self.dirty = true;
                        self.emit();
                        return;
                    }
                },
                command = commands.recv() => match command {
                    Some(command) => self.apply_command(&client, command),
                    None => return,
                },
                _ = tick.tick() => {
                    if self.notice.as_ref().is_some_and(|notice| {
                        notice.level != NoticeLevel::Error && notice.at.elapsed() > NOTICE_TTL
                    }) {
                        self.notice = None;
                        self.dirty = true;
                    }
                    if self.dirty {
                        self.emit();
                    }
                }
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

    fn apply_update(&mut self, client: &TuiClient, update: ClientUpdate) {
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
                    crate::auth::recommended_model(&option.provider)
                        .map(str::to_owned)
                        .filter(|id| valid(id))
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
                    Ok(receipt) => {
                        if let (
                            CommandOutcome::SessionCreated { session_id },
                            Some(PendingIntent::Create { draft }),
                        ) = (&receipt.outcome, intent)
                        {
                            self.adopt_created(*session_id);
                            if let Some(text) = draft {
                                self.submit(client, *session_id, text);
                            }
                        }
                    }
                    Err(failure) => self.set_notice(NoticeLevel::Error, failure.message()),
                }
            }
            ClientUpdate::SnapshotFailed(failure) => {
                self.set_notice(NoticeLevel::Error, failure.message());
            }
        }
    }

    fn apply_snapshot(&mut self, client: &TuiClient, snapshot: WorkspaceSnapshot) {
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

    fn apply_live_event(&mut self, client: &TuiClient, event: SessionEventEnvelope) {
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
            && !matches!(
                self.pending.get(&command_id),
                Some(PendingIntent::Create { .. })
            )
        {
            self.pending.remove(&command_id);
        }
        self.recent_events.push_back(event);
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
    }

    fn reduce_event(&mut self, client: &TuiClient, envelope: &SessionEventEnvelope) {
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
                StateEffect::Notice { level, text, .. } => self.set_notice(level, text),
                StateEffect::Attention(attention) => {
                    let _ = self.app.emit("attention", attention.summary());
                }
                StateEffect::RequestSnapshot(request) => {
                    self.send(client, ClientRequest::Snapshot(request));
                }
                StateEffect::Refocus(target) => self.focus(client, target),
                // Adoption waits for the command receipt, which carries the
                // queued draft; the event can arrive first.
                StateEffect::AdoptCreated(_) => {}
                StateEffect::SubmitDraft { session_id, text } => {
                    self.submit(client, session_id, text);
                }
                StateEffect::SessionRemoved { session_id, .. } => {
                    self.pending.retain(|_, intent| {
                        !matches!(intent, PendingIntent::Prompt { session_id: s } if *s == session_id)
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

    fn apply_command(&mut self, client: &TuiClient, command: UiCommand) {
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
            UiCommand::Submit { session_id, text } => {
                let text = text.trim().to_owned();
                if text.is_empty() {
                    return;
                }
                self.clear_error_notice();
                match session_id {
                    Some(session_id) => self.submit(client, session_id, text),
                    None => self.create_session(client, Some(text)),
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

    fn focus(&mut self, client: &TuiClient, target: Option<SessionId>) {
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

    fn adopt_created(&mut self, session_id: SessionId) {
        self.sessions.warm_empty(session_id);
        self.focused = Some(session_id);
        self.focus_clock += 1;
        self.sessions.mark_focused(session_id, self.focus_clock);
        self.sessions.evict_cold_bodies(self.focused);
    }

    fn create_session(&mut self, client: &TuiClient, draft: Option<String>) {
        let Some(workspace_id) = self.workspace_id else {
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
                parent_id: None,
                model,
                approval_mode: ApprovalMode::default(),
                profile: qq_protocol::AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
            PendingIntent::Create { draft },
        );
    }

    fn submit(&mut self, client: &TuiClient, session_id: SessionId, text: String) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.record_prompt(&text);
        }
        self.command(
            client,
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::Text { text }],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
            PendingIntent::Prompt { session_id },
        );
    }

    fn approve(
        &mut self,
        client: &TuiClient,
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

    fn command(&mut self, client: &TuiClient, command: SessionCommand, intent: PendingIntent) {
        let command_id = match CommandId::generate() {
            Ok(id) => id,
            Err(error) => {
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
        ) {
            self.pending.remove(&command_id);
        }
    }

    fn send(&mut self, client: &TuiClient, request: ClientRequest) -> bool {
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
        let _ = self.app.emit(STATE_EVENT, &state);
        self.last_emitted = Some(state);
    }
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
