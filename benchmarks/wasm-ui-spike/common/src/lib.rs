//! Everything the two ADR-0017 spikes share so that the framework is the only
//! variable: reading the target server from the page, attaching to `qq serve`
//! over HTTP/SSE with the real `qq-client`, the remote-mount contract shape,
//! and a frame monitor that counts long frames while events stream.
//!
//! Spike code: measured, then discarded. Not a product surface.

#![forbid(unsafe_code)]

use std::{cell::Cell, path::Path, rc::Rc, time::Duration};

use futures_util::StreamExt as _;
use qq_client::{
    ClientError, ConnectionState, SessionClient,
    state::{
        Group, ModelOption, ReduceContext, SNAPSHOT_MESSAGE_LIMIT, SNAPSHOT_SESSION_LIMIT,
        SessionStore, SessionView,
    },
};
use qq_protocol::{
    EventCursor, MessageRole, MessageState, ServerConnection, ServerInfo, SessionEventEnvelope,
    SessionId, SnapshotRequest, WorkspaceSnapshot,
};
use wasm_bindgen::{JsCast as _, closure::Closure};

/// Where the spike page points. Read from the query string
/// (`?server=http://127.0.0.1:PORT&credential=…&workspace=/abs/path`) or from
/// the JSON the host passes to `mount`. Credentials in URLs are a spike-only
/// shortcut; U2 stores them in IndexedDB and never in a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpikeConfig {
    pub base_url: String,
    pub credential: String,
    pub workspace: String,
}

impl SpikeConfig {
    #[must_use]
    pub fn from_query(query: &str) -> Option<Self> {
        let query = query.strip_prefix('?').unwrap_or(query);
        let mut base_url = None;
        let mut credential = None;
        let mut workspace = None;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            let value = js_sys::decode_uri_component(value).ok()?.as_string()?;
            match key {
                "server" => base_url = Some(value),
                "credential" => credential = Some(value),
                "workspace" => workspace = Some(value),
                _ => {}
            }
        }
        Some(Self {
            base_url: base_url?,
            credential: credential?,
            workspace: workspace?,
        })
    }

    /// The JSON shape a host passes to a remote's `mount`:
    /// `{"server":"…","credential":"…","workspace":"…"}`.
    #[must_use]
    pub fn from_json(json: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        Some(Self {
            base_url: value.get("server")?.as_str()?.to_owned(),
            credential: value.get("credential")?.as_str()?.to_owned(),
            workspace: value.get("workspace")?.as_str()?.to_owned(),
        })
    }

    #[must_use]
    pub fn from_location() -> Option<Self> {
        let search = web_sys::window()?.location().search().ok()?;
        Self::from_query(&search)
    }
}

/// One step of the feed, delivered to the framework in arrival order. The
/// framework owns the `SessionStore` and applies these to it; this crate has
/// no opinion about signals.
// `Event` is the streaming hot path; boxing it to shrink the enum would cost
// one allocation per live event for a value that is moved once.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum FeedUpdate {
    Connection(ConnectionState),
    Snapshot(Box<WorkspaceSnapshot>),
    Event(SessionEventEnvelope),
    Failed(String),
}

#[derive(Debug)]
pub enum FeedError {
    Health(String),
    Connection(String),
    Client(ClientError),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Health(message) => write!(f, "health probe failed: {message}"),
            Self::Connection(message) => write!(f, "server connection rejected: {message}"),
            Self::Client(error) => write!(f, "client: {error}"),
        }
    }
}

/// Attaches to the server: health → `ServerConnection` → `SessionClient`.
pub async fn connect(config: &SpikeConfig) -> Result<SessionClient, FeedError> {
    let http = reqwest::Client::new();
    let info: ServerInfo = http
        .get(format!(
            "{}/v1/health",
            config.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.credential)
        .send()
        .await
        .map_err(|error| FeedError::Health(error.to_string()))?
        .error_for_status()
        .map_err(|error| FeedError::Health(error.to_string()))?
        .json()
        .await
        .map_err(|error| FeedError::Health(error.to_string()))?;
    let connection = ServerConnection::new(&config.base_url, config.credential.clone(), info)
        .map_err(|error| FeedError::Connection(error.to_string()))?;
    SessionClient::new(connection).map_err(FeedError::Client)
}

/// Runs the feed until the page goes away: resolve the workspace, snapshot,
/// subscribe from the snapshot cursor, reconnect with a fixed backoff on
/// stream loss, and hand every step to `sink`. Bodies for a newly focused
/// session are fetched separately with [`fetch_body`].
pub async fn run_feed(
    client: SessionClient,
    config: SpikeConfig,
    mut sink: impl FnMut(FeedUpdate),
) {
    let mut after: Option<EventCursor> = None;
    loop {
        sink(FeedUpdate::Connection(ConnectionState::Connecting));
        let (workspace_id, _committed_through) =
            match client.resolve_workspace(Path::new(&config.workspace)).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    sink(FeedUpdate::Failed(format!("resolve workspace: {error}")));
                    sink(FeedUpdate::Connection(ConnectionState::Offline));
                    gloo_timers::future::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
        let cursor = if let Some(cursor) = after {
            cursor
        } else {
            let request = SnapshotRequest::new(
                workspace_id,
                None,
                SNAPSHOT_SESSION_LIMIT,
                SNAPSHOT_MESSAGE_LIMIT,
            );
            match client.snapshot(request).await {
                Ok(snapshot) => {
                    let cursor = snapshot.cursor;
                    sink(FeedUpdate::Snapshot(Box::new(snapshot)));
                    cursor
                }
                Err(error) => {
                    sink(FeedUpdate::Failed(format!("snapshot: {error}")));
                    sink(FeedUpdate::Connection(ConnectionState::Offline));
                    gloo_timers::future::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        };
        sink(FeedUpdate::Connection(ConnectionState::Replaying));
        let mut stream = match client.events(workspace_id, cursor).await {
            Ok(stream) => stream,
            Err(error) => {
                sink(FeedUpdate::Failed(format!("events: {error}")));
                sink(FeedUpdate::Connection(ConnectionState::Offline));
                gloo_timers::future::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        sink(FeedUpdate::Connection(ConnectionState::Live));
        let mut last = cursor;
        while let Some(item) = stream.next().await {
            match item {
                Ok(envelope) => {
                    last = envelope.cursor;
                    sink(FeedUpdate::Event(envelope));
                }
                Err(error) => {
                    sink(FeedUpdate::Failed(format!("stream: {error}")));
                    break;
                }
            }
        }
        after = Some(last);
        sink(FeedUpdate::Connection(ConnectionState::Offline));
        gloo_timers::future::sleep(Duration::from_secs(1)).await;
    }
}

/// The framework-neutral view model both spikes render: the shared reducer
/// plus the bookkeeping the TUI keeps around it (cursor continuity, focus,
/// connection). Each framework wraps exactly one of these in one signal.
#[derive(Debug)]
pub struct Feed {
    pub store: SessionStore,
    pub connection: ConnectionState,
    pub focused: Option<SessionId>,
    pub workspace_id: Option<qq_protocol::WorkspaceId>,
    pub last_sequence: u64,
    pub error: Option<String>,
    pub apply: ApplyStats,
    pub frames: FrameStats,
    models: Vec<ModelOption>,
}

impl Default for Feed {
    fn default() -> Self {
        Self {
            store: SessionStore::default(),
            connection: ConnectionState::Connecting,
            focused: None,
            workspace_id: None,
            last_sequence: 0,
            error: None,
            apply: ApplyStats::default(),
            frames: FrameStats::default(),
            models: Vec::new(),
        }
    }
}

/// One session row in the grouped list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: SessionId,
    pub group: Group,
    pub depth: usize,
    pub title: String,
    pub tail: String,
}

/// One rendered transcript message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub id: qq_protocol::MessageId,
    pub user: bool,
    pub streaming: bool,
    pub text: String,
}

impl Feed {
    /// Applies one feed step exactly as the TUI's `apply_snapshot` and
    /// `apply_live_event` do, minus effects the spike does not act on.
    pub fn apply(&mut self, update: FeedUpdate) {
        match update {
            FeedUpdate::Connection(state) => self.connection = state,
            FeedUpdate::Failed(message) => self.error = Some(message),
            FeedUpdate::Snapshot(snapshot) => self.install(*snapshot),
            FeedUpdate::Event(envelope) => {
                if self
                    .workspace_id
                    .is_some_and(|workspace| workspace != envelope.cursor.workspace_id)
                    || envelope.cursor.sequence <= self.last_sequence
                {
                    return;
                }
                if self.last_sequence != 0 && envelope.cursor.sequence != self.last_sequence + 1 {
                    self.error = Some("session event gap detected".to_owned());
                    return;
                }
                self.workspace_id
                    .get_or_insert(envelope.cursor.workspace_id);
                self.last_sequence = envelope.cursor.sequence;
                let already_loaded = self
                    .store
                    .get(&envelope.session_id)
                    .is_some_and(|session| envelope.cursor.sequence <= session.loaded_through);
                if !already_loaded {
                    let context = ReduceContext {
                        focused: self.focused,
                        attentive: true,
                        workspace_id: self.workspace_id,
                        capabilities: None,
                        models: &self.models,
                        caused_by_me: false,
                    };
                    let _effects = self.store.reduce_event(&envelope, context);
                }
            }
        }
    }

    /// Installs a snapshot (initial or a focused body fetched later).
    pub fn install(&mut self, snapshot: WorkspaceSnapshot) {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            self.error = Some("snapshot for another workspace".to_owned());
            return;
        }
        let sequence = snapshot.cursor.sequence;
        if initial {
            self.workspace_id = Some(snapshot.workspace.id);
            self.last_sequence = sequence;
        }
        if initial || sequence >= self.last_sequence {
            for summary in snapshot.sessions {
                self.store.upsert_summary(summary, &self.models, sequence);
            }
        }
        for body in snapshot.included {
            self.store
                .install_session_snapshot(body, &self.models, sequence);
        }
        if let Some(focused) = snapshot.focused {
            let id = focused.summary.id;
            self.store
                .install_session_snapshot(focused, &self.models, sequence);
            if self.focused.is_none() || self.focused == Some(id) {
                self.focused = Some(id);
                self.store.mark_focused(id, sequence);
            }
        } else if self.focused.is_none()
            && let Some(first) = self.store.roots().first().copied()
        {
            self.focused = Some(first);
        }
        self.store.evict_cold_bodies(self.focused);
    }

    /// Whether `id` still needs a body fetch after being focused.
    pub fn focus(&mut self, id: SessionId) -> bool {
        self.focused = Some(id);
        self.store.mark_focused(id, self.last_sequence);
        !self.store.get(&id).is_some_and(SessionView::is_warm)
    }

    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .store
            .thread_order()
            .iter()
            .filter_map(|id| {
                let view = self.store.get(id)?;
                Some(Row {
                    id: *id,
                    group: view.group(),
                    depth: self.store.depth(*id),
                    title: view.summary.title.clone(),
                    tail: view.live.tail.clone(),
                })
            })
            .collect();
        rows.sort_by_key(|row| match row.group {
            Group::NeedsYou => 0,
            Group::Working => 1,
            Group::Idle => 2,
            Group::Done => 3,
        });
        rows
    }

    #[must_use]
    pub fn lines(&self) -> Vec<Line> {
        let Some(view) = self.focused.and_then(|id| self.store.get(&id)) else {
            return Vec::new();
        };
        view.messages
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|message| Line {
                id: message.id,
                user: message.role == MessageRole::User,
                streaming: message.state == MessageState::Streaming,
                text: message.output.clone(),
            })
            .collect()
    }
}

/// Fetches one session body on focus change; the framework installs it.
pub async fn fetch_body(
    client: &SessionClient,
    workspace_id: qq_protocol::WorkspaceId,
    session_id: SessionId,
) -> Result<WorkspaceSnapshot, ClientError> {
    client
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            SNAPSHOT_SESSION_LIMIT,
            SNAPSHOT_MESSAGE_LIMIT,
        ))
        .await
}

/// Counts frames longer than 50 ms (the RAIL "long task" threshold) and the
/// worst frame while the page is alive. Both spikes show these numbers next
/// to the event counter so streaming smoothness is compared the same way.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FrameStats {
    pub frames: u32,
    pub long_frames: u32,
    pub max_frame_ms: f64,
}

type FrameCallbackSlot = std::cell::RefCell<Option<Closure<dyn FnMut()>>>;

pub fn start_frame_monitor(mut on_stats: impl FnMut(FrameStats) + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(performance) = window.performance() else {
        return;
    };
    let stats = Rc::new(Cell::new(FrameStats::default()));
    let last = Rc::new(Cell::new(performance.now()));
    let callback: Rc<FrameCallbackSlot> = Rc::new(std::cell::RefCell::new(None));
    let callback_handle = callback.clone();
    *callback.borrow_mut() = Some(Closure::new(move || {
        let now = performance.now();
        let elapsed = now - last.get();
        last.set(now);
        let mut current = stats.get();
        current.frames += 1;
        if elapsed > 50.0 {
            current.long_frames += 1;
        }
        if elapsed > current.max_frame_ms {
            current.max_frame_ms = elapsed;
        }
        stats.set(current);
        if current.frames % 30 == 0 {
            on_stats(current);
        }
        if let (Some(window), Some(callback)) =
            (web_sys::window(), callback_handle.borrow().as_ref())
        {
            let _ = window.request_animation_frame(callback.as_ref().unchecked_ref());
        }
    }));
    if let Some(callback) = callback.borrow().as_ref() {
        let _ = window.request_animation_frame(callback.as_ref().unchecked_ref());
    }
}

/// `performance.now()` in milliseconds, for per-event apply timing.
#[must_use]
pub fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}

/// Running apply-latency histogram: how long the framework took from
/// receiving an event to returning from its state update (reactive work
/// included for Leptos, excluded for Dioxus whose render is scheduled).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ApplyStats {
    pub events: u64,
    pub total_ms: f64,
    pub max_ms: f64,
    pub first_event_at_ms: Option<f64>,
    pub last_event_at_ms: Option<f64>,
}

impl ApplyStats {
    pub fn record(&mut self, started_at: f64, finished_at: f64) {
        self.events += 1;
        let elapsed = finished_at - started_at;
        self.total_ms += elapsed;
        if elapsed > self.max_ms {
            self.max_ms = elapsed;
        }
        if self.first_event_at_ms.is_none() {
            self.first_event_at_ms = Some(started_at);
        }
        self.last_event_at_ms = Some(finished_at);
    }

    #[must_use]
    pub fn mean_ms(&self) -> f64 {
        if self.events == 0 {
            0.0
        } else {
            self.total_ms / self.events as f64
        }
    }

    #[must_use]
    pub fn events_per_second(&self) -> f64 {
        match (self.first_event_at_ms, self.last_event_at_ms) {
            (Some(first), Some(last)) if last > first => {
                self.events as f64 / ((last - first) / 1000.0)
            }
            _ => 0.0,
        }
    }
}

/// Label a `ConnectionState` for the status strip.
#[must_use]
pub fn connection_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connecting => "connecting",
        ConnectionState::Replaying => "replaying",
        ConnectionState::Live => "live",
        ConnectionState::Offline => "offline",
    }
}
