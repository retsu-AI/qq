use super::scheduler::schedule_runs;
use super::*;

pub type RuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<LoadedRuntime, RuntimeLoadError>> + Send + 'static>>;
pub type WorkerRuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<ModelSelection, RuntimeLoadError>> + Send + 'static>>;
pub type SpawnModelValidationFuture =
    Pin<Box<dyn Future<Output = Result<(), RuntimeLoadError>> + Send + 'static>>;

#[derive(Clone)]
pub struct LoadedRuntime {
    pub runtime: Arc<Runtime>,
    pub pricing: Option<ModelPricing>,
}

pub trait RuntimeLoader: Send + Sync + 'static {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture;

    /// Resolves the application-configured worker route against the same
    /// workspace configuration and policy used by ordinary runtime loading.
    /// `parent` is returned unchanged when no worker route is configured.
    fn resolve_worker_model(
        &self,
        workspace: String,
        parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        let _ = workspace;
        Box::pin(std::future::ready(Ok(parent)))
    }

    /// Confirms a resolved child route is spawnable right now: the route
    /// parses, its provider is configured, policy-allowed, and authenticated
    /// at this moment, and the model id appears in the provider's served
    /// model list. Every spawn resolution source — the explicit tool
    /// argument, the configured worker route, and the parent-selection
    /// fallback — passes through this check before any durable child state
    /// is created. Core stays ignorant of provider-auth details; the loader
    /// answers "is this route spawnable" the same way it answers "load this
    /// route". The default accepts everything, for embeddings without an
    /// authenticated catalog.
    fn validate_spawn_model(
        &self,
        workspace: String,
        selection: ModelSelection,
    ) -> SpawnModelValidationFuture {
        let _ = (workspace, selection);
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLoadRequest {
    pub workspace: String,
    pub model: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RuntimeLoadError {
    pub kind: RunFailureKind,
    pub message: String,
}

/// The workspace-configured grants that merge into a session's grant set at
/// creation: exact tool names (including folded `mcp__<server>__<tool>`
/// allowlist entries) and word-granularity shell command prefixes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceGrantSeed {
    pub tools: Vec<String>,
    pub shell_prefixes: Vec<String>,
}

impl WorkspaceGrantSeed {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.shell_prefixes.is_empty()
    }
}

pub type GrantSeedFuture = Pin<Box<dyn Future<Output = WorkspaceGrantSeed> + Send + 'static>>;

pub type GrantPromotionFuture =
    Pin<Box<dyn Future<Output = WorkspaceGrantOutcome> + Send + 'static>>;

/// The configuration-facing seam for workspace-lifetime grants, provided at
/// runtime construction. qq-core stays configuration-agnostic: the embedding
/// application implements both directions against its config layer.
pub trait WorkspaceGrantAuthority: Send + Sync + 'static {
    /// The workspace's effective config grants, resolved when a session is
    /// created. A failure to resolve should seed nothing rather than error:
    /// session creation must not depend on a loadable configuration.
    fn seed_grants(&self, workspace: &Path) -> GrantSeedFuture;

    /// Durably promotes one approval grant into the workspace configuration.
    /// Failures are data ([`WorkspaceGrantOutcome::Failed`]), never errors:
    /// a promotion must not fail the approval that requested it.
    fn promote_grant(&self, workspace: &Path, grant: &ApprovalGrant) -> GrantPromotionFuture;
}

/// Everything an approval reviewer may see about one held tool call. The
/// transcript is deliberately absent: a poisoned context must not be able to
/// argue its own call safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    pub tool_name: String,
    pub shell: Option<ShellCommandPreview>,
    pub edit: Option<EditPreview>,
    pub workspace: String,
}

/// A reviewer's answer for one held tool call. Anything other than a clear
/// `Approve` leaves the call waiting for a human; the reviewer can expedite
/// approvals but can never widen a denial or bypass the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    /// The reviewer declines to decide; the human approval path continues.
    Escalate {
        reason: String,
    },
    /// The reviewer judges the call unsafe. Treated exactly like `Escalate`
    /// today (the human still decides); carried separately so the verdict
    /// vocabulary does not need a wire change to harden later.
    Deny {
        reason: String,
    },
}

pub type ReviewFuture = Pin<Box<dyn Future<Output = ReviewVerdict> + Send + 'static>>;

/// A model-backed adjudicator for tool calls that static policy holds for
/// approval. The embedding application implements this against its provider
/// layer; qq-core stays ignorant of model routing and prompting. Reviewer
/// failures and timeouts must resolve as `Escalate`, never hang: the gate
/// keeps its own human timeout regardless.
pub trait ApprovalReviewer: Send + Sync + 'static {
    fn review(&self, request: ReviewRequest) -> ReviewFuture;
}

pub type SessionEventStream =
    Pin<Box<dyn Stream<Item = Result<SessionEventEnvelope, SessionRuntimeError>> + Send + 'static>>;

#[derive(Clone)]
pub struct SessionRuntimeOptions {
    pub database_path: PathBuf,
    pub max_active_runs: usize,
    /// How long an approval request may wait for a client before it is denied.
    pub approval_timeout: Duration,
    /// Where workspace-lifetime grants come from and go to. Absent, sessions
    /// seed no config grants and approve-for-workspace decisions record only
    /// their session grant (the promotion reports failure).
    pub grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
    /// Model-backed adjudication for calls held for approval under `Auto`
    /// mode. Absent, held calls wait for a client exactly as before.
    pub approval_reviewer: Option<Arc<dyn ApprovalReviewer>>,
}

impl std::fmt::Debug for SessionRuntimeOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRuntimeOptions")
            .field("database_path", &self.database_path)
            .field("max_active_runs", &self.max_active_runs)
            .field("approval_timeout", &self.approval_timeout)
            .field("grant_authority", &self.grant_authority.is_some())
            .field("approval_reviewer", &self.approval_reviewer.is_some())
            .finish()
    }
}

impl SessionRuntimeOptions {
    #[must_use]
    pub fn new(database_path: PathBuf) -> Self {
        Self {
            database_path,
            max_active_runs: 8,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            grant_authority: None,
            approval_reviewer: None,
        }
    }

    #[must_use]
    pub fn with_grant_authority(mut self, authority: Arc<dyn WorkspaceGrantAuthority>) -> Self {
        self.grant_authority = Some(authority);
        self
    }

    #[must_use]
    pub fn with_approval_reviewer(mut self, reviewer: Arc<dyn ApprovalReviewer>) -> Self {
        self.approval_reviewer = Some(reviewer);
        self
    }
}

#[derive(Clone)]
pub struct SessionRuntime {
    pub(super) inner: Arc<SessionRuntimeInner>,
}

pub(super) struct SessionRuntimeInner {
    pub(super) store: Store,
    pub(super) loader: Arc<dyn RuntimeLoader>,
    pub(super) grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
    pub(super) approval_reviewer: Option<Arc<dyn ApprovalReviewer>>,
    pub(super) permits: Arc<Semaphore>,
    /// Run permits for child (sub-agent) sessions, deliberately a separate
    /// pool from `permits`: a parent run holds its permit for its whole
    /// lifetime, including while it awaits a spawned child. If children drew
    /// from the same pool, `max_active_runs` parents all awaiting children
    /// would leave no permit with which any child could ever start — a
    /// deadlock. Children cannot spawn (depth is one), so no such wait cycle
    /// exists against this pool. Sized like the root pool so global run
    /// concurrency stays bounded at twice `max_active_runs`.
    pub(super) child_permits: Arc<Semaphore>,
    pub(super) schedule: mpsc::Sender<()>,
    pub(super) cancellations: Mutex<HashMap<RunId, watch::Sender<bool>>>,
    approvals: Mutex<HashMap<ToolCallId, PendingApproval>>,
    pub(super) approval_timeout: Duration,
    wakeups: Mutex<HashMap<WorkspaceId, watch::Sender<u64>>>,
    pub(super) failed: watch::Sender<bool>,
    pub(super) shutdown: watch::Sender<bool>,
    pub(super) scheduler_stopped: watch::Sender<bool>,
    pub(super) settlements: watch::Sender<u64>,
    pub(super) lifecycle: RwLock<()>,
}

struct PendingApproval {
    run_id: RunId,
    signal: oneshot::Sender<()>,
}

impl SessionRuntime {
    pub async fn open(
        options: SessionRuntimeOptions,
        loader: Arc<dyn RuntimeLoader>,
    ) -> Result<Self, SessionRuntimeError> {
        if options.max_active_runs == 0 {
            return Err(SessionRuntimeError::InvalidRunLimit);
        }
        let store = Store::open(options.database_path).await?;
        let recovered = store.recover_interrupted_runs().await?;
        let (schedule, receiver) = mpsc::channel(1);
        let (failed, _) = watch::channel(false);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let (scheduler_stopped, _) = watch::channel(false);
        let (settlements, _) = watch::channel(0_u64);
        let inner = Arc::new(SessionRuntimeInner {
            store,
            loader,
            grant_authority: options.grant_authority,
            approval_reviewer: options.approval_reviewer,
            permits: Arc::new(Semaphore::new(options.max_active_runs)),
            child_permits: Arc::new(Semaphore::new(options.max_active_runs)),
            schedule,
            cancellations: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            approval_timeout: options.approval_timeout,
            wakeups: Mutex::new(HashMap::new()),
            failed,
            shutdown,
            scheduler_stopped,
            settlements,
            lifecycle: RwLock::new(()),
        });
        for cursor in recovered {
            inner.notify(cursor);
        }
        tokio::spawn(schedule_runs(
            Arc::downgrade(&inner),
            receiver,
            shutdown_receiver,
            inner.scheduler_stopped.clone(),
        ));
        let runtime = Self { inner };
        runtime.request_schedule();
        Ok(runtime)
    }

    pub async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let signal_run = match command {
            SessionCommand::CancelRun { run_id } => Some(run_id),
            _ => None,
        };
        let signal_approval = match &command {
            SessionCommand::RespondToolApproval { tool_call_id, .. } => Some(*tool_call_id),
            _ => None,
        };
        let should_schedule = matches!(command, SessionCommand::SubmitPrompt { .. });
        // Config grants merge into the session's grant set at creation
        // (tools.md "Grant Lifetimes"): the workspace's effective grants are
        // resolved here — the seam may do blocking configuration IO, so it
        // runs outside the store worker — and copied into `session_grants`
        // rows inside the CreateSession transaction. Copy-at-creation is
        // deliberate: the gate consults only the session's own rows
        // afterwards, so a later config edit affects new sessions only.
        let seed = match (&command, &self.inner.grant_authority) {
            (SessionCommand::CreateSession { workspace_id, .. }, Some(authority)) => {
                let path = self.inner.store.workspace_path(*workspace_id).await?;
                authority.seed_grants(Path::new(&path)).await
            }
            _ => WorkspaceGrantSeed::default(),
        };
        let lifecycle = self.inner.lifecycle.read().await;
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let applied = self
            .inner
            .store
            .command_with_seed(command_id, command, seed)
            .await?;
        drop(lifecycle);
        self.inner.notify(applied.receipt.committed_through);

        if let Some(run_id) = signal_run {
            self.inner.cancel(run_id);
        }
        for run_id in &applied.cascade_cancels {
            self.inner.cancel(*run_id);
        }
        if let Some(tool_call_id) = signal_approval {
            self.inner.resolve_approval(tool_call_id);
        }
        if let Some(promotion) = applied.promotion {
            self.spawn_grant_promotion(promotion);
        }
        if should_schedule || applied.schedule {
            self.request_schedule();
        }
        Ok(applied.receipt)
    }

    /// Carries an approve-for-workspace promotion to its durable conclusion
    /// in the background. The approval's session grant is already committed,
    /// so nothing here can fail it: whatever the write's fate, it is
    /// published as a persisted `workspace_grant_promoted` event — the same
    /// events-out channel every other asynchronous outcome uses.
    fn spawn_grant_promotion(&self, promotion: PendingGrantPromotion) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let outcome = match &inner.grant_authority {
                Some(authority) => {
                    authority
                        .promote_grant(Path::new(&promotion.workspace_path), &promotion.grant)
                        .await
                }
                None => WorkspaceGrantOutcome::Failed {
                    message: "this server has no workspace grant store; the approval covers \
                              this session only"
                        .to_owned(),
                },
            };
            if let Ok(event) = inner.store.record_grant_promotion(promotion, outcome).await {
                inner.notify(event.cursor);
            }
        });
    }

    pub async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if request.session_limit == 0
            || request.session_limit > MAX_SNAPSHOT_SESSIONS
            || request.message_limit == 0
            || request.message_limit > MAX_SNAPSHOT_MESSAGES
        {
            return Err(SessionRuntimeError::InvalidPageLimit);
        }
        self.inner.store.snapshot(request).await
    }

    pub fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<SessionEventStream, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if request.after.store_id != self.inner.store.store_id() {
            return Err(SessionRuntimeError::CursorStoreMismatch);
        }
        if request.after.workspace_id != request.workspace_id {
            return Err(SessionRuntimeError::CursorWorkspaceMismatch);
        }

        let store = self.inner.store.clone();
        let mut failed = self.inner.failed.subscribe();
        let mut wakeup = self
            .inner
            .subscribe(request.workspace_id, request.after.sequence)?;
        Ok(Box::pin(stream! {
            let mut after = request.after.sequence;
            loop {
                let events = match store
                    .events_after(request.workspace_id, after, MAX_REPLAY_EVENTS)
                    .await
                {
                    Ok(events) => events,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                if !events.is_empty() {
                    for event in events {
                        after = event.cursor.sequence;
                        yield Ok(event);
                    }
                    continue;
                }
                tokio::select! {
                    biased;
                    changed = failed.changed() => {
                        if changed.is_err() || *failed.borrow() {
                            yield Err(SessionRuntimeError::Unavailable);
                            return;
                        }
                    }
                    changed = wakeup.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
            }
        }))
    }

    /// Stops accepting commands and new run claims, durably cancels every
    /// accepted queued or running prompt, and waits until no unfinished run
    /// remains. Snapshot and subscription reads stay available so callers can
    /// inspect the settled state after shutdown.
    pub async fn shutdown(&self) -> Result<(), SessionRuntimeError> {
        let lifecycle = self.inner.lifecycle.write().await;
        self.inner.shutdown.send_replace(true);
        drop(lifecycle);
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;

        let mut scheduler_stopped = self.inner.scheduler_stopped.subscribe();
        while !*scheduler_stopped.borrow() {
            tokio::time::timeout_at(deadline, scheduler_stopped.changed())
                .await
                .map_err(|_| SessionRuntimeError::ShutdownTimedOut)?
                .map_err(|_| SessionRuntimeError::Unavailable)?;
        }

        let mut settlements = self.inner.settlements.subscribe();
        for run_id in self.inner.store.unfinished_run_ids().await? {
            let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let applied = self
                .inner
                .store
                .command_with_seed(
                    command_id,
                    SessionCommand::CancelRun { run_id },
                    WorkspaceGrantSeed::default(),
                )
                .await?;
            self.inner.notify(applied.receipt.committed_through);
            self.inner.cancel(run_id);
            for cascade in &applied.cascade_cancels {
                self.inner.cancel(*cascade);
            }
        }

        loop {
            if self.inner.store.unfinished_run_ids().await?.is_empty() {
                return Ok(());
            }
            if *self.inner.failed.borrow() {
                return Err(SessionRuntimeError::Unavailable);
            }
            tokio::time::timeout_at(deadline, settlements.changed())
                .await
                .map_err(|_| SessionRuntimeError::ShutdownTimedOut)?
                .map_err(|_| SessionRuntimeError::Unavailable)?;
        }
    }

    pub(super) fn request_schedule(&self) {
        if *self.inner.shutdown.borrow() {
            return;
        }
        let _ = self.inner.schedule.try_send(());
    }
}

impl SessionRuntimeInner {
    pub(super) fn notify(&self, cursor: EventCursor) {
        let Ok(mut wakeups) = self.wakeups.lock() else {
            return;
        };
        match wakeups.get(&cursor.workspace_id) {
            Some(sender) => {
                sender.send_replace(cursor.sequence);
            }
            None => {
                let (sender, _) = watch::channel(cursor.sequence);
                wakeups.insert(cursor.workspace_id, sender);
            }
        }
    }

    pub(super) fn subscribe(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
    ) -> Result<watch::Receiver<u64>, SessionRuntimeError> {
        let mut wakeups = self
            .wakeups
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        let sender = wakeups.entry(workspace_id).or_insert_with(|| {
            let (sender, _) = watch::channel(sequence);
            sender
        });
        Ok(sender.subscribe())
    }

    pub(super) fn cancel(&self, run_id: RunId) {
        let Ok(cancellations) = self.cancellations.lock() else {
            return;
        };
        if let Some(sender) = cancellations.get(&run_id) {
            sender.send_replace(true);
        }
    }

    pub(super) fn register_approval(
        &self,
        tool_call_id: ToolCallId,
        run_id: RunId,
    ) -> oneshot::Receiver<()> {
        let (signal, receiver) = oneshot::channel();
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.insert(tool_call_id, PendingApproval { run_id, signal });
        }
        receiver
    }

    fn resolve_approval(&self, tool_call_id: ToolCallId) {
        let Ok(mut approvals) = self.approvals.lock() else {
            return;
        };
        if let Some(pending) = approvals.remove(&tool_call_id) {
            let _ = pending.signal.send(());
        }
    }

    pub(super) fn remove_approval(&self, tool_call_id: ToolCallId) {
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.remove(&tool_call_id);
        }
    }

    pub(super) fn clear_run_approvals(&self, run_id: RunId) {
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.retain(|_, pending| pending.run_id != run_id);
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionRuntimeError {
    #[error("maximum active runs must be greater than zero")]
    InvalidRunLimit,
    #[error("page limits must be greater than zero")]
    InvalidPageLimit,
    #[error("workspace path must not be empty")]
    EmptyWorkspace,
    #[error("workspace path must identify an existing directory")]
    InvalidWorkspace,
    #[error("prompt must not be empty")]
    EmptyPrompt,
    #[error("prompt exceeds the session limit")]
    PromptTooLarge,
    #[error("workspace was not found")]
    WorkspaceNotFound,
    #[error("workspace limit reached")]
    WorkspaceLimitReached,
    #[error("session was not found")]
    SessionNotFound,
    #[error("session has an active run")]
    SessionActive,
    #[error("workspace session limit reached")]
    SessionLimitReached,
    #[error("parent session does not belong to the workspace")]
    ParentWorkspaceMismatch,
    #[error("run was not found")]
    RunNotFound,
    #[error("tool call was not found for that run")]
    ToolCallNotFound,
    #[error("tool call is not awaiting approval")]
    ApprovalNotPending,
    #[error("approval grant is empty or exceeds the session limit")]
    InvalidApprovalGrant,
    #[error("session follow-up queue is full")]
    QueueFull,
    #[error("session context exceeds the size limit")]
    ContextTooLarge,
    #[error("model output exceeds the session size limit")]
    OutputTooLarge,
    #[error("session event exceeds the durable size limit")]
    EventTooLarge,
    #[error("command ID was reused with different content")]
    IdempotencyConflict,
    #[error("durable command limit reached")]
    CommandLimitReached,
    #[error("model selection exceeds the session limit")]
    InvalidModelSelection,
    #[error("event cursor belongs to another store")]
    CursorStoreMismatch,
    #[error("event cursor belongs to another workspace")]
    CursorWorkspaceMismatch,
    #[error("session accounting is unavailable")]
    AccountingUnavailable,
    #[error("session runtime is overloaded")]
    Overloaded,
    #[error("session runtime shutdown timed out before every run settled")]
    ShutdownTimedOut,
    #[error("session runtime is unavailable")]
    Unavailable,
    #[error("session persistence failed")]
    Persistence,
}
