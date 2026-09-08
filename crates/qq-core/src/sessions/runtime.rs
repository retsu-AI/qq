use super::scheduler::schedule_runs;
use super::*;
use crate::plan::{
    AgentProfile, CompiledAgentPlan, HostSnapshot, PlanCompileError, ProviderDescriptor,
};

pub type RuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<LoadedRuntime, RuntimeLoadError>> + Send + 'static>>;
pub type WorkerRuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<ModelSelection, RuntimeLoadError>> + Send + 'static>>;
pub type SpawnModelValidationFuture =
    Pin<Box<dyn Future<Output = Result<(), RuntimeLoadError>> + Send + 'static>>;

/// The compiled plan a session run executes from, as returned by the
/// embedding application's [`RuntimeLoader`].
#[derive(Clone)]
pub struct LoadedRuntime {
    pub plan: Arc<CompiledAgentPlan>,
}

impl LoadedRuntime {
    /// Compiles a plan from an already constructed runtime and its resolved
    /// model, for loaders that build runtimes directly (embedders, tests,
    /// benchmarks). The runtime's provider, MCP registry, spawn routes, and
    /// retry policy are kept; its model identity must agree with
    /// `resolved_model`, which the compiled plan then reports. Performs
    /// blocking filesystem work.
    pub fn compile_blocking(
        runtime: &Runtime,
        resolved_model: ResolvedModel,
        workspace: PathBuf,
    ) -> Result<Self, PlanCompileError> {
        Self::compile_blocking_for_profile(
            runtime,
            resolved_model,
            workspace,
            qq_protocol::AgentProfileId::default(),
        )
    }

    /// [`Self::compile_blocking`] for a named agent profile. Embedders that
    /// realize profiles themselves record which one the plan implements so
    /// the run's persisted identity names it.
    pub fn compile_blocking_for_profile(
        runtime: &Runtime,
        resolved_model: ResolvedModel,
        workspace: PathBuf,
        profile_id: qq_protocol::AgentProfileId,
    ) -> Result<Self, PlanCompileError> {
        if runtime.model.as_ref() != resolved_model.provider_model.as_str()
            || runtime.max_output_tokens != resolved_model.max_output_tokens
            || runtime.context_window != resolved_model.context_window
        {
            return Err(PlanCompileError::ModelMismatch {
                route: resolved_model.route,
                runtime_model: runtime.model.to_string(),
                runtime_max_output_tokens: runtime.max_output_tokens,
                runtime_context_window: runtime.context_window,
            });
        }
        let mut profile = AgentProfile::new(
            Arc::clone(&runtime.provider),
            ProviderDescriptor::embedded(),
            resolved_model,
            workspace,
        )
        .with_spawn_model_routes(runtime.spawn_model_routes.to_vec())
        .with_delegation(runtime.delegation.as_ref().clone())
        .with_audit(runtime.audit)
        .with_profile_id(profile_id);
        for host in runtime.hosts.iter() {
            profile = profile.with_host(HostSnapshot::capture_blocking(Arc::clone(host)));
        }
        for registered in runtime.context_sources.iter() {
            profile = profile.with_context_source(Arc::clone(&registered.source));
        }
        profile = profile.with_context_cache(Arc::clone(&runtime.context_cache));
        Ok(Self {
            plan: CompiledAgentPlan::compile_blocking(profile)?,
        })
    }

    #[must_use]
    pub fn resolved_model(&self) -> &Arc<ResolvedModel> {
        self.plan.resolved_model()
    }
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
    /// Configured agent profile the session selected. Loaders that know no
    /// profiles accept `default` and reject anything else.
    pub profile: qq_protocol::AgentProfileId,
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

/// Bounds on the context a review request carries. The reviewer sees only
/// what it needs to judge one action; a poisoned transcript must not be able
/// to argue its own call safe.
pub const MAX_REVIEW_ARGUMENT_BYTES: usize = 16 * 1024;
pub const MAX_REVIEW_BRIEF_BYTES: usize = 8 * 1024;
pub const MAX_REVIEW_RECENT_ACTIONS: usize = 16;

/// Where the reviewed call originates: a root session, or a child spawned by
/// a parent run with the given nesting depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOrigin {
    Root,
    Child { depth: u16, parent_run: RunId },
}

/// One earlier tool call of the same run, for the reviewer's sense of what
/// the run has been doing. Names and paths only, never results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentAction {
    pub tool: String,
    pub path: Option<String>,
}

/// Everything an approval reviewer may see about one held tool call. The
/// transcript is deliberately absent; for a child the task brief its parent
/// wrote stands in, so the reviewer can judge whether the action is plausibly
/// necessary for the stated task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    pub tool_name: String,
    /// The call's arguments as JSON, truncated to `MAX_REVIEW_ARGUMENT_BYTES`.
    pub arguments: String,
    pub shell: Option<ShellCommandPreview>,
    pub edit: Option<EditPreview>,
    pub workspace: String,
    pub origin: ReviewOrigin,
    /// The child's task brief (the prompt that created its run), truncated to
    /// `MAX_REVIEW_BRIEF_BYTES`. `None` for root sessions.
    pub task_brief: Option<String>,
    /// The session's approval mode, so the reviewer knows whether its `Deny`
    /// is final (`Supervised`) or advisory (`Auto`).
    pub mode: ApprovalMode,
    /// The last `MAX_REVIEW_RECENT_ACTIONS` finished tool calls of the run.
    pub recent_actions: Vec<RecentAction>,
    /// Tool names and shell prefixes the session has been granted.
    pub granted_tools: Vec<String>,
    pub granted_shell_prefixes: Vec<String>,
}

/// What the reviewer's own provider call cost, charged to the reviewed run.
/// `None` fields mean unknown, never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewSpend {
    pub usage: Option<TokenUsage>,
    pub cost_usd_nanos: Option<u64>,
}

/// A reviewer's answer for one held tool call, with what answering cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewVerdict {
    pub decision: ReviewDecision,
    pub spend: ReviewSpend,
}

impl ReviewVerdict {
    /// A verdict that cost nothing: reviewer unavailable, not configured, or
    /// answered from policy without a model call.
    #[must_use]
    pub const fn free(decision: ReviewDecision) -> Self {
        Self {
            decision,
            spend: ReviewSpend {
                usage: None,
                cost_usd_nanos: Some(0),
            },
        }
    }
}

/// The reviewer's judgement. For `Auto` sessions anything other than a clear
/// `Approve` leaves the call waiting for a human: the reviewer can expedite
/// approvals but never widens a denial. For `Supervised` sessions `Deny` is
/// final and `Escalate` reaches the human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    /// The reviewer declines to decide; the human approval path continues.
    Escalate {
        reason: String,
    },
    /// The reviewer judges the call unsafe or unnecessary for the task.
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

/// Committed events with their persisted encoding, for transports that
/// forward bytes.
pub type PublishedEventStream =
    Pin<Box<dyn Stream<Item = Result<Arc<PublishedEvent>, SessionRuntimeError>> + Send + 'static>>;

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
    /// Run permits for child (sub-agent) sessions, one pool per depth
    /// (`child_permits[d - 1]` serves depth `d`), each separate from
    /// `permits`: a parent run holds its permit for its whole lifetime,
    /// including while it awaits a spawned child. If a depth drew from its
    /// parents' pool, `max_active_runs` parents all awaiting children would
    /// leave no permit with which any child could ever start — a deadlock —
    /// and the same holds between depth one and depth two. Each pool is
    /// sized like the root pool, so global run concurrency stays bounded at
    /// `(MAX_CHILD_DEPTH + 1) * max_active_runs`.
    pub(super) child_permits: Vec<Arc<Semaphore>>,
    pub(super) max_active_runs: usize,
    pub(super) schedule: mpsc::Sender<()>,
    pub(super) cancellations: Mutex<HashMap<RunId, watch::Sender<bool>>>,
    /// Live steering channels for executing prompt runs, registered when the
    /// run loop starts and removed when it settles.
    pub(super) steering: Mutex<HashMap<RunId, crate::runtime::SteeringSender>>,
    approvals: Mutex<HashMap<ToolCallId, PendingApproval>>,
    pub(super) approval_timeout: Duration,
    wakeups: Mutex<HashMap<WorkspaceId, watch::Sender<u64>>>,
    pub(super) failed: watch::Sender<bool>,
    pub(super) shutdown: watch::Sender<bool>,
    pub(super) scheduler_stopped: watch::Sender<bool>,
    pub(super) settlements: watch::Sender<u64>,
    grant_promotions: mpsc::Sender<()>,
    grant_promotion_stopped: watch::Sender<bool>,
    pub(super) lifecycle: RwLock<()>,
}

struct PendingApproval {
    run_id: RunId,
    signal: oneshot::Sender<()>,
}

struct GrantPromotionStopGuard(watch::Sender<bool>);

impl Drop for GrantPromotionStopGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// Drains the durable workspace-grant outbox serially. The channel is only a
/// capacity-one wakeup: accepted work lives in SQLite, survives process loss,
/// and is deleted only with its persisted fate event.
async fn run_grant_promotions(
    inner: std::sync::Weak<SessionRuntimeInner>,
    mut receiver: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
) {
    let _stopped = GrantPromotionStopGuard(stopped);
    loop {
        let Some(runtime) = inner.upgrade() else {
            return;
        };
        if *runtime.failed.borrow() {
            return;
        }
        let promotion = match runtime.store.next_grant_promotion().await {
            Ok(promotion) => promotion,
            Err(_) => {
                runtime.failed.send_replace(true);
                return;
            }
        };
        let Some(promotion) = promotion else {
            drop(runtime);
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                wake = receiver.recv() => {
                    if wake.is_none() {
                        return;
                    }
                }
            }
            continue;
        };
        let outcome = match &runtime.grant_authority {
            Some(authority) => {
                match AssertUnwindSafe(async {
                    authority
                        .promote_grant(Path::new(&promotion.workspace_path), &promotion.grant)
                        .await
                })
                .catch_unwind()
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => WorkspaceGrantOutcome::Failed {
                        message: "the workspace grant authority panicked".to_owned(),
                    },
                }
            }
            None => WorkspaceGrantOutcome::Failed {
                message: "this server has no workspace grant store; the approval covers this \
                          session only"
                    .to_owned(),
            },
        };
        match runtime
            .store
            .settle_grant_promotion(promotion, outcome)
            .await
        {
            Ok(Some(event)) => runtime.notify(event.cursor),
            Ok(None) => {}
            Err(_) => {
                runtime.failed.send_replace(true);
                return;
            }
        }
    }
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
        let (grant_promotions, grant_promotion_receiver) = mpsc::channel(1);
        let (failed, _) = watch::channel(false);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let (scheduler_stopped, _) = watch::channel(false);
        let (grant_promotion_stopped, _) = watch::channel(false);
        let (settlements, _) = watch::channel(0_u64);
        let inner = Arc::new(SessionRuntimeInner {
            store,
            loader,
            grant_authority: options.grant_authority,
            approval_reviewer: options.approval_reviewer,
            permits: Arc::new(Semaphore::new(options.max_active_runs)),
            child_permits: (0..MAX_CHILD_DEPTH)
                .map(|_| Arc::new(Semaphore::new(options.max_active_runs)))
                .collect(),
            max_active_runs: options.max_active_runs,
            schedule,
            cancellations: Mutex::new(HashMap::new()),
            steering: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            approval_timeout: options.approval_timeout,
            wakeups: Mutex::new(HashMap::new()),
            failed,
            shutdown,
            scheduler_stopped,
            settlements,
            grant_promotions,
            grant_promotion_stopped,
            lifecycle: RwLock::new(()),
        });
        for cursor in recovered {
            inner.notify(cursor);
        }
        tokio::spawn(schedule_runs(
            Arc::downgrade(&inner),
            receiver,
            shutdown_receiver.clone(),
            inner.scheduler_stopped.clone(),
        ));
        tokio::spawn(run_grant_promotions(
            Arc::downgrade(&inner),
            grant_promotion_receiver,
            shutdown_receiver,
            inner.grant_promotion_stopped.clone(),
        ));
        let runtime = Self { inner };
        runtime.request_schedule();
        runtime.request_grant_promotions();
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
        let steer = match &command {
            SessionCommand::SteerRun {
                run_id,
                input,
                interrupt,
            } => Some((*run_id, crate::input::render_text(input), *interrupt)),
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
            .command_with_seed(
                command_id,
                command,
                seed,
                Some(self.inner.grant_promotions.clone()),
            )
            .await?;
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
        // The row is durable (the receipt names its id). A replayed command
        // returns the same receipt without re-queuing: the first delivery
        // already reached the loop, or the run finished and superseded it.
        if let (Some((run_id, text, interrupt)), CommandOutcome::SteeringQueued { message_id, .. }) =
            (steer, &applied.receipt.outcome)
            && !applied.replayed
        {
            self.inner.steer(
                run_id,
                crate::runtime::SteeringMessage {
                    message_id: *message_id,
                    text: text.trim().to_owned(),
                },
                interrupt,
            );
        }
        if applied.grant_promotion_pending {
            self.request_grant_promotions();
        }
        drop(lifecycle);
        if should_schedule || applied.schedule {
            self.request_schedule();
        }
        Ok(applied.receipt)
    }

    /// The canonical path a resolved workspace id names, for callers that
    /// consult workspace-scoped configuration (capabilities) outside a run.
    pub async fn workspace_path(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<String, SessionRuntimeError> {
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner.store.workspace_path(workspace_id).await
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
            || request.include_sessions.len() > qq_protocol::MAX_INCLUDED_SESSIONS
        {
            return Err(SessionRuntimeError::InvalidPageLimit);
        }
        self.inner.store.snapshot(request).await
    }

    /// Committed events after `request.after`, as parsed envelopes.
    pub fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<SessionEventStream, SessionRuntimeError> {
        let published = self.subscribe_published(request)?;
        Ok(Box::pin(
            published.map(|item| item.map(PublishedEvent::into_envelope)),
        ))
    }

    /// Committed events after `request.after`, each carrying the exact JSON
    /// the store persisted so a transport can forward it without
    /// re-serializing.
    ///
    /// Delivery has two phases. Catch-up reads pages until the subscriber is
    /// current: from the workspace ring when it still covers the cursor
    /// (warm reconnect), otherwise from SQLite. Then events are read from the
    /// ring by cursor with no store access. A subscriber that falls more than
    /// the ring capacity behind is redirected to catch-up from its last
    /// cursor, so the sequence it observes is contiguous and complete
    /// whatever the pace. SQLite is authoritative throughout: nothing is
    /// published to the ring that was not already committed.
    pub fn subscribe_published(
        &self,
        request: SubscribeRequest,
    ) -> Result<PublishedEventStream, SessionRuntimeError> {
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
        let workspace_id = request.workspace_id;
        Ok(Box::pin(stream! {
            let mut after = request.after.sequence;
            let attachment = match store.attach_feed(workspace_id, after, MAX_REPLAY_EVENTS).await {
                Ok(attachment) => attachment,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let live = attachment.live;
            let mut first_page = Some(attachment.page);
            loop {
                loop {
                    let page = match first_page.take() {
                        Some(page) => page,
                        None => match store
                            .published_events_after(workspace_id, after, MAX_REPLAY_EVENTS)
                            .await
                        {
                            Ok(page) => page,
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        },
                    };
                    // A short page is the end of the backlog: one read
                    // suffices for a subscriber that is nearly caught up.
                    let caught_up = page.len() < usize::from(MAX_REPLAY_EVENTS);
                    for event in page {
                        after = event.envelope.cursor.sequence;
                        yield Ok(event);
                    }
                    if caught_up {
                        break;
                    }
                }
                loop {
                    let received = tokio::select! {
                        biased;
                        changed = failed.changed() => {
                            if changed.is_err() || *failed.borrow() {
                                yield Err(SessionRuntimeError::Unavailable);
                                return;
                            }
                            continue;
                        }
                        received = live.recv(after) => received,
                    };
                    match received {
                        Ok(event) => {
                            // Reads are by cursor, so this is exactly
                            // `after + 1`: no duplicate or gap handling.
                            after = event.envelope.cursor.sequence;
                            yield Ok(event);
                        }
                        // Recover the missing range from the authoritative log.
                        Err(feed::RecvError::Behind) => break,
                        Err(feed::RecvError::Closed) => return,
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
        let mut grant_promotion_stopped = self.inner.grant_promotion_stopped.subscribe();
        for run_id in self.inner.store.unfinished_run_ids().await? {
            let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let applied = self
                .inner
                .store
                .command_with_seed(
                    command_id,
                    SessionCommand::CancelRun { run_id },
                    WorkspaceGrantSeed::default(),
                    None,
                )
                .await?;
            self.inner.notify(applied.receipt.committed_through);
            self.inner.cancel(run_id);
            for cascade in &applied.cascade_cancels {
                self.inner.cancel(*cascade);
            }
        }

        loop {
            if *self.inner.failed.borrow() {
                return Err(SessionRuntimeError::Unavailable);
            }
            let unfinished = self.inner.store.unfinished_run_ids().await?;
            let preparation_quiescent = self.inner.permits.available_permits()
                == self.inner.max_active_runs
                && self
                    .inner
                    .child_permits
                    .iter()
                    .all(|pool| pool.available_permits() == self.inner.max_active_runs);
            if unfinished.is_empty() && preparation_quiescent && *grant_promotion_stopped.borrow() {
                // The promotion worker publishes `failed` before its stopped
                // guard fires. Re-read after observing stopped so its failure
                // cannot race this success boundary.
                if *self.inner.failed.borrow() {
                    return Err(SessionRuntimeError::Unavailable);
                }
                return Ok(());
            }
            tokio::time::timeout_at(deadline, async {
                tokio::select! {
                    changed = settlements.changed() => changed,
                    changed = grant_promotion_stopped.changed() => changed,
                }
            })
            .await
            .map_err(|_| SessionRuntimeError::ShutdownTimedOut)?
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        }
    }

    /// Settles all accepted work and then closes the durable store worker.
    /// Unlike [`Self::shutdown`], snapshots and subscriptions are unavailable
    /// after this final owner-lifecycle operation completes.
    pub async fn close(&self) -> Result<(), SessionRuntimeError> {
        self.shutdown().await?;
        self.inner.store.close().await
    }

    pub(super) fn request_schedule(&self) {
        if *self.inner.shutdown.borrow() {
            return;
        }
        let _ = self.inner.schedule.try_send(());
    }

    fn request_grant_promotions(&self) {
        match self.inner.grant_promotions.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                self.inner.failed.send_replace(true);
            }
        }
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

    /// Hands a durably recorded steering message to the executing run. A run
    /// that is no longer registered finished (or is finishing); its
    /// settlement marks the message superseded, so nothing is lost here.
    pub(super) fn steer(
        &self,
        run_id: RunId,
        message: crate::runtime::SteeringMessage,
        interrupt: bool,
    ) {
        let Ok(steering) = self.steering.lock() else {
            return;
        };
        let Some(sender) = steering.get(&run_id) else {
            return;
        };
        // The channel is sized to `MAX_PENDING_STEERING`, the same bound the
        // durable admission enforces, so a full channel means the store and
        // the loop disagree only transiently; the message stays `queued` and
        // settles superseded with the run if it never applies.
        match sender.messages.try_send(message) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {}
        }
        if interrupt {
            sender.interrupt();
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
    #[error("run limits must be greater than zero and within the runtime ceilings")]
    InvalidRunLimits,
    #[error("invalid input: {0}")]
    InvalidInput(qq_protocol::InputError),
    #[error("run is not executing a prompt, so it cannot be steered")]
    RunNotSteerable,
    #[error("run steering queue is full")]
    SteeringQueueFull,
    #[error("agent profile {0} is not declared by the workspace configuration")]
    UnknownProfile(qq_protocol::AgentProfileId),
    #[error("session has no compaction to roll back")]
    NoCompactionToRollBack,
    #[error("workspace was not found")]
    WorkspaceNotFound,
    #[error("workspace limit reached")]
    WorkspaceLimitReached,
    #[error("session was not found")]
    SessionNotFound,
    #[error("session or an owning run is active")]
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
    #[error("a spawned child session cannot be raised above the authority its parent granted")]
    ChildAuthorityEscalation,
    #[error("sub-agent nesting would exceed the runtime depth ceiling")]
    ChildDepthExceeded,
    #[error("this run's delegation tree already holds the maximum number of sub-agents")]
    DescendantLimitReached,
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
