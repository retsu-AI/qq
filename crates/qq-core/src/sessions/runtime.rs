use super::scheduler::schedule_runs;
use super::side_questions::SideQueryLimits;
use super::*;
use crate::plan::{
    AgentProfile, CompiledAgentPlan, HostSnapshot, PlanCompileError, ProviderDescriptor,
};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

pub type RuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<LoadedRuntime, RuntimeLoadError>> + Send + 'static>>;
pub type WorkerRuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<ModelSelection, RuntimeLoadError>> + Send + 'static>>;
pub type SpawnModelValidationFuture =
    Pin<Box<dyn Future<Output = Result<(), RuntimeLoadError>> + Send + 'static>>;

#[derive(Clone)]
pub struct LoadedRuntime {
    pub plan: Arc<CompiledAgentPlan>,
}

impl LoadedRuntime {
    pub async fn from_runtime(
        runtime: Runtime,
        profile: qq_protocol::AgentProfileId,
        workspace: PathBuf,
    ) -> Result<Self, PlanCompileError> {
        let permit = crate::plan::COMPILE_SLOTS
            .acquire()
            .await
            .expect("compile semaphore stays open");
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let model = runtime.resolved_model();
            Self::compile_blocking_for_profile(&runtime, model, workspace, profile)
        })
        .await
        {
            Ok(result) => result,
            Err(source) => Err(PlanCompileError::CompilationTask { source }),
        }
    }

    #[must_use]
    pub fn new(plan: Arc<CompiledAgentPlan>) -> Self {
        Self { plan }
    }

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
        .with_shell_policy(runtime.shell.as_ref().clone())
        .with_process_producer(Arc::clone(&runtime.process_producer))
        .with_network_policy(runtime.network.as_ref().clone())
        .with_profile_id(profile_id);
        for host in runtime.hosts.iter() {
            profile = profile.with_host(HostSnapshot::capture_blocking(Arc::clone(host)));
        }
        for registered in runtime.context_sources.iter() {
            profile = profile.with_context_source(Arc::clone(&registered.source));
        }
        if let Some(effort) = runtime.reasoning_effort {
            profile = profile.with_reasoning_effort(effort);
        }
        profile = profile
            .with_context_cache(Arc::clone(&runtime.context_cache))
            .with_turn_recovery(runtime.turn_recovery)
            .with_output_ceiling(runtime.output_ceiling);
        Ok(Self::new(CompiledAgentPlan::compile_blocking(profile)?))
    }

    #[must_use]
    pub fn resolved_model(&self) -> &Arc<ResolvedModel> {
        self.plan.resolved_model()
    }
}

pub trait RuntimeLoader: Send + Sync + 'static {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture;

    fn load_with_progress(
        &self,
        request: RuntimeLoadRequest,
        progress: RuntimeLoadProgress,
    ) -> RuntimeLoadFuture {
        let _ = progress;
        self.load(request)
    }

    fn resolve_worker_model(
        &self,
        workspace: String,
        parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        let _ = workspace;
        Box::pin(std::future::ready(Ok(parent)))
    }

    fn validate_spawn_model(
        &self,
        workspace: String,
        selection: ModelSelection,
        advertised: Arc<[String]>,
    ) -> SpawnModelValidationFuture {
        let _ = (workspace, selection, advertised);
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RuntimeLoadStage {
    Starting,
    CanonicalizingWorkspace,
    LoadingConfiguration,
    ReadingCredentialMetadata,
    CompilingProvider,
    LoadingTools,
    CompilingPlan,
    Complete,
}

impl RuntimeLoadStage {
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Starting => "starting runtime preparation",
            Self::CanonicalizingWorkspace => "canonicalizing the workspace",
            Self::LoadingConfiguration => "loading configuration",
            Self::ReadingCredentialMetadata => "reading credential metadata",
            Self::CompilingProvider => "compiling the model provider",
            Self::LoadingTools => "loading tool integrations",
            Self::CompilingPlan => "compiling the agent plan",
            Self::Complete => "finishing runtime preparation",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeLoadProgress {
    stage: Arc<AtomicU8>,
    cancellation: crate::RunCancellation,
}

impl Default for RuntimeLoadProgress {
    fn default() -> Self {
        Self {
            stage: Arc::new(AtomicU8::new(RuntimeLoadStage::Starting as u8)),
            cancellation: crate::RunCancellation::new(),
        }
    }
}

impl RuntimeLoadProgress {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn set(&self, stage: RuntimeLoadStage) {
        self.stage.store(stage as u8, Ordering::Release);
    }

    #[must_use]
    pub fn stage(&self) -> RuntimeLoadStage {
        match self.stage.load(Ordering::Acquire) {
            value if value == RuntimeLoadStage::CanonicalizingWorkspace as u8 => {
                RuntimeLoadStage::CanonicalizingWorkspace
            }
            value if value == RuntimeLoadStage::LoadingConfiguration as u8 => {
                RuntimeLoadStage::LoadingConfiguration
            }
            value if value == RuntimeLoadStage::ReadingCredentialMetadata as u8 => {
                RuntimeLoadStage::ReadingCredentialMetadata
            }
            value if value == RuntimeLoadStage::CompilingProvider as u8 => {
                RuntimeLoadStage::CompilingProvider
            }
            value if value == RuntimeLoadStage::LoadingTools as u8 => {
                RuntimeLoadStage::LoadingTools
            }
            value if value == RuntimeLoadStage::CompilingPlan as u8 => {
                RuntimeLoadStage::CompilingPlan
            }
            value if value == RuntimeLoadStage::Complete as u8 => RuntimeLoadStage::Complete,
            _ => RuntimeLoadStage::Starting,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeLoadRequest {
    pub reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub workspace: String,
    pub model: ModelSelection,
    pub profile: qq_protocol::AgentProfileId,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RuntimeLoadError {
    pub kind: RunFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GoalCheckPolicyError {
    #[error("the check is denied by deny_tools or deny_shell_prefixes")]
    ShellDenied,
    #[error("check policy could not be loaded: {0}")]
    Configuration(#[source] RuntimeLoadError),
    #[error("check policy validation stopped unexpectedly")]
    Unavailable,
    #[error("too many check policy validations are active; retry the request")]
    Overloaded,
}

pub type GoalCheckPolicyFuture =
    Pin<Box<dyn Future<Output = Result<(), GoalCheckPolicyError>> + Send + 'static>>;

pub trait WorkspacePolicyAuthority: Send + Sync + 'static {
    fn goal_policy(&self, _workspace: &Path) -> GoalPolicyFuture {
        Box::pin(std::future::ready(Err(GoalPolicyError::Unavailable)))
    }

    fn validate_goal_check(&self, _workspace: &Path, _command: &str) -> GoalCheckPolicyFuture {
        Box::pin(std::future::ready(Err(GoalCheckPolicyError::Unavailable)))
    }
}

pub type SessionEventStream =
    Pin<Box<dyn Stream<Item = Result<SessionEventEnvelope, SessionRuntimeError>> + Send + 'static>>;

pub type PublishedEventStream =
    Pin<Box<dyn Stream<Item = Result<Arc<PublishedEvent>, SessionRuntimeError>> + Send + 'static>>;

#[derive(Clone)]
pub struct SessionRuntimeOptions {
    pub database_path: PathBuf,
    pub max_active_runs: usize,
    pub max_active_side_queries: Option<usize>,
    pub side_query_limits: SideQueryLimits,
    pub approval_timeout: Option<Duration>,
    pub retention: Option<Duration>,
    pub grant_authority: Option<Arc<dyn WorkspacePolicyAuthority>>,
}

impl std::fmt::Debug for SessionRuntimeOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRuntimeOptions")
            .field("database_path", &self.database_path)
            .field("max_active_runs", &self.max_active_runs)
            .field("max_active_side_queries", &self.max_active_side_queries)
            .field("side_query_limits", &self.side_query_limits)
            .field("approval_timeout", &self.approval_timeout)
            .field("retention", &self.retention)
            .field("grant_authority", &self.grant_authority.is_some())
            .finish()
    }
}

impl SessionRuntimeOptions {
    #[must_use]
    pub fn new(database_path: PathBuf) -> Self {
        Self {
            database_path,
            max_active_runs: 8,
            max_active_side_queries: None,
            side_query_limits: SideQueryLimits::default(),
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            retention: None,
            grant_authority: None,
        }
    }

    #[must_use]
    pub const fn with_approval_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.approval_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn with_retention(mut self, retention: Option<Duration>) -> Self {
        self.retention = retention;
        self
    }

    #[must_use]
    pub fn with_grant_authority(mut self, authority: Arc<dyn WorkspacePolicyAuthority>) -> Self {
        self.grant_authority = Some(authority);
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
    pub(super) grant_authority: Option<Arc<dyn WorkspacePolicyAuthority>>,
    pub(super) permits: Arc<Semaphore>,
    pub(super) side_permits: Arc<Semaphore>,
    pub(super) side_tasks: AtomicUsize,
    pub(super) restores: AtomicUsize,
    pub(super) side_limits: side_questions::SideCeilings,
    pub(super) side_sessions: Mutex<HashMap<SessionId, Arc<tokio::sync::Notify>>>,
    pub(super) side_cancellations: Mutex<HashMap<RunId, RunCancellation>>,
    pub(super) child_permits: Vec<Arc<Semaphore>>,
    pub(super) max_active_runs: usize,
    pub(super) schedule: mpsc::Sender<()>,
    pub(super) cancellations: Mutex<HashMap<RunId, watch::Sender<bool>>>,
    pub(super) goal_deadlines: Mutex<HashMap<RunId, crate::runtime::RunDeadline>>,
    pub(super) steering: Mutex<HashMap<RunId, crate::runtime::SteeringSender>>,
    approvals: Mutex<HashMap<ToolCallId, PendingApproval>>,
    pub(super) approval_timeout: Option<Duration>,
    pub(super) retention: Option<Duration>,
    wakeups: Mutex<HashMap<WorkspaceId, watch::Sender<u64>>>,
    pub(super) failed: watch::Sender<bool>,
    pub(super) shutdown: watch::Sender<bool>,
    pub(super) scheduler_stopped: watch::Sender<bool>,
    pub(super) settlements: watch::Sender<u64>,
    pub(super) lifecycle: RwLock<()>,
    pub(super) draining: AtomicBool,
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
        let side_ceiling = (options.max_active_runs / 4).max(1);
        let side_limit = options.max_active_side_queries.unwrap_or(side_ceiling);
        if side_limit == 0 || side_limit > side_ceiling {
            return Err(SessionRuntimeError::InvalidRunLimit);
        }
        let side_limits = options.side_query_limits.resolve()?;
        let store = Store::open(options.database_path).await?;
        let recovery_store_id = store.store_id();
        let recovered = store
            .call_write(store::Priority::AwaitControl, move |connection| {
                settlement::recover_interrupted_runs(connection, recovery_store_id)
            })
            .await?;
        let store_id = store.store_id();
        store
            .call_write(store::Priority::AwaitControl, move |connection| {
                let transaction = store::begin_unit(connection)?;
                let mut statement =
                    transaction.prepare("SELECT id FROM side_questions WHERE state = 'running'")?;
                let ids = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                transaction.execute(
                    "UPDATE side_questions SET state = 'interrupted', finished_at_ms = ?1,
                    usage_json = NULL, estimated_cost_usd_nanos = NULL WHERE state = 'running'",
                    [now_ms()],
                )?;
                for id in ids {
                    side_questions::append_side_event(&transaction, store_id, parse_id(&id)?)?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await?;
        let (schedule, receiver) = mpsc::channel(1);
        let (failed, _) = watch::channel(false);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let (scheduler_stopped, _) = watch::channel(false);
        let (settlements, _) = watch::channel(0_u64);
        let inner = Arc::new(SessionRuntimeInner {
            store,
            loader,
            grant_authority: options.grant_authority,
            permits: Arc::new(Semaphore::new(options.max_active_runs)),
            side_permits: Arc::new(Semaphore::new(side_limit)),
            side_tasks: AtomicUsize::new(0),
            restores: AtomicUsize::new(0),
            side_limits,
            side_sessions: Mutex::new(HashMap::new()),
            side_cancellations: Mutex::new(HashMap::new()),
            child_permits: (0..MAX_CHILD_DEPTH)
                .map(|_| Arc::new(Semaphore::new(options.max_active_runs)))
                .collect(),
            max_active_runs: options.max_active_runs,
            schedule,
            cancellations: Mutex::new(HashMap::new()),
            goal_deadlines: Mutex::new(HashMap::new()),
            steering: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            approval_timeout: options.approval_timeout,
            retention: options.retention,
            wakeups: Mutex::new(HashMap::new()),
            failed,
            shutdown,
            scheduler_stopped,
            settlements,
            lifecycle: RwLock::new(()),
            draining: AtomicBool::new(false),
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

    #[must_use]
    pub fn store_id(&self) -> StoreId {
        self.inner.store.store_id()
    }

    pub async fn startup_goal_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<EventCursor>, SessionRuntimeError> {
        self.inner.store.startup_goal_cursor(workspace_id).await
    }

    pub async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        self.command_with_pause_condition(command_id, command, false)
            .await
    }

    pub async fn pause_goal_if_active(
        &self,
        session_id: SessionId,
        expected: qq_protocol::GoalIdentity,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        self.command_with_pause_condition(
            id,
            SessionCommand::GoalControl {
                session_id,
                expected,
                action: qq_protocol::GoalControl::Pause,
            },
            true,
        )
        .await
    }

    async fn command_with_pause_condition(
        &self,
        command_id: CommandId,
        command: SessionCommand,
        pause_goal_only_if_active: bool,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if matches!(command, SessionCommand::SubmitSideQuestion { .. }) {
            return self.submit_side_question(command_id, command).await;
        }
        if matches!(command, SessionCommand::UndoWorkspace { .. }) {
            return self.undo_workspace(command_id, command).await;
        }
        let signal_side = match &command {
            SessionCommand::CancelSideQuestion { side_question_id } => Some(*side_question_id),
            _ => None,
        };
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
            } => Some((*run_id, input.clone(), *interrupt)),
            _ => None,
        };
        let should_schedule = matches!(command, SessionCommand::SubmitPrompt { .. });
        let goal_session = match &command {
            SessionCommand::SetGoal { session_id, .. }
            | SessionCommand::GoalControl {
                session_id,
                action:
                    qq_protocol::GoalControl::Resume | qq_protocol::GoalControl::ApproveCheck { .. },
                ..
            } => Some(*session_id),
            _ => None,
        };
        let replayed_goal = goal_session.is_some()
            && self.inner.grant_authority.is_some()
            && self
                .inner
                .store
                .command_is_recorded(command_id, &command)
                .await?;
        let goal_session = if replayed_goal { None } else { goal_session };
        let goal_path = if let Some(session) = goal_session
            && self.inner.grant_authority.is_some()
        {
            Some(self.inner.store.call(store::Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT w.path FROM sessions s JOIN workspaces w ON w.id=s.workspace_id WHERE s.id=?1",
                    [session.to_string()], |row| row.get::<_, String>(0),
                )?)
            }).await)
        } else {
            None
        };
        let goal_policy = goal_session.map(|_| &goal_path);
        let goal_policy = match goal_policy {
            Some(Some(Ok(path))) => Some(self.inner.resolve_goal_policy(Path::new(path)).await),
            Some(Some(Err(_))) => Some(Err(GoalPolicyError::Unavailable)),
            Some(None) => Some(Ok(GoalPolicy::default())),
            None => None,
        };
        let automatic_check = match &command {
            SessionCommand::SetGoal {
                session_id,
                check: Some(check),
                check_approval,
                ..
            } if !replayed_goal => {
                let authority = self.inner.store.tool_approval_policy(*session_id).await?;
                (check_approval.is_some() || authority.mode == ApprovalMode::Auto)
                    .then_some(check.command.as_str())
            }
            _ => None,
        };
        let check_command = match &command {
            SessionCommand::GoalControl {
                action: qq_protocol::GoalControl::ApproveCheck { command, .. },
                ..
            } => Some(command.as_str()),
            _ => automatic_check,
        };
        let goal_check_policy = if replayed_goal {
            None
        } else {
            match (check_command, &self.inner.grant_authority) {
                (Some(command), Some(authority)) => Some(match &goal_path {
                    Some(Ok(path)) => {
                        match tokio::time::timeout(
                            Duration::from_secs(10),
                            authority.validate_goal_check(Path::new(path), command),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(GoalCheckPolicyError::Unavailable),
                        }
                    }
                    _ => Err(GoalCheckPolicyError::Unavailable),
                }),
                _ => None,
            }
        };
        let lifecycle = self.inner.lifecycle.read().await;
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let applied = self
            .inner
            .store
            .command_with_admission(
                command_id,
                command,
                CommandAdmission {
                    goal_work_present: false,
                    pause_goal_only_if_active,
                    goal_check_policy,
                    goal_policy,
                },
            )
            .await?;
        self.inner.notify(applied.receipt.committed_through);

        if let Some(id) = signal_side
            && let Some(token) = self
                .inner
                .side_cancellations
                .lock()
                .expect("side cancellation lock")
                .get(&id)
        {
            token.cancel();
        }
        if let Some(run_id) = signal_run {
            self.inner.cancel(run_id);
        }
        for run_id in &applied.cascade_cancels {
            self.inner.cancel(*run_id);
        }
        if let Some(tool_call_id) = signal_approval {
            self.inner.resolve_approval(tool_call_id);
        }
        if let (Some((run_id, input, interrupt)), CommandOutcome::SteeringQueued { message_id, .. }) =
            (steer, &applied.receipt.outcome)
            && !applied.replayed
        {
            self.inner.steer(
                run_id,
                crate::runtime::SteeringMessage {
                    message_id: *message_id,
                    input,
                },
                interrupt,
            );
        }
        debug_assert!(
            applied.side_launch.is_none(),
            "side launches are owned by submit_side_question"
        );
        drop(lifecycle);
        if should_schedule || applied.schedule {
            self.request_schedule();
        }
        Ok(applied.receipt)
    }

    async fn undo_workspace(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let runtime = self.clone();
        tokio::spawn(async move { runtime.restore_workspace(command_id, command).await })
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    async fn restore_workspace(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let SessionCommand::UndoWorkspace {
            workspace_id,
            snapshot,
        } = &command
        else {
            return Err(SessionRuntimeError::CONSTRAINT);
        };
        let workspace_id = *workspace_id;
        let snapshot = snapshot.clone();
        let lifecycle = self.inner.lifecycle.read().await;
        if *self.inner.shutdown.borrow()
            || *self.inner.failed.borrow()
            || self.inner.draining.load(Ordering::Acquire)
        {
            return Err(SessionRuntimeError::Unavailable);
        }
        let started = self
            .inner
            .store
            .begin_workspace_restore(command_id, command.clone())
            .await;
        let path = match started {
            Ok(commands::WorkspaceRestore::Replayed(receipt)) => return Ok(receipt),
            Ok(commands::WorkspaceRestore::Started(path)) => path,
            Err(error) => return Err(error),
        };
        let _restore = RestoreSlot::new(&self.inner);
        drop(lifecycle);
        let restored = AssertUnwindSafe(restore_snapshot(Path::new(&path), snapshot.as_deref()))
            .catch_unwind()
            .await;
        let receipt = match restored {
            Ok(Ok(restored)) => {
                self.inner
                    .store
                    .record_workspace_restore(command_id, command, restored.outcome(workspace_id))
                    .await
            }
            Ok(Err(error)) => Err(SessionRuntimeError::UndoFailed(error.into())),
            Err(_) => Err(SessionRuntimeError::Unavailable),
        };
        if self
            .inner
            .store
            .clear_workspace_restore(path)
            .await
            .is_err()
        {
            self.inner.failed.send_replace(true);
        }
        self.request_schedule();
        receipt
    }

    pub async fn workspace_path(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<String, SessionRuntimeError> {
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner.store.workspace_path(workspace_id).await
    }

    pub async fn capture_side_context(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner.store.capture_side_context(session_id).await
    }

    pub async fn workspace_catalog(
        &self,
        request: qq_protocol::WorkspaceCatalogRequest,
    ) -> Result<qq_protocol::WorkspaceCatalog, SessionRuntimeError> {
        if *self.inner.failed.borrow() || *self.inner.shutdown.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if request.limit == 0 || request.limit > qq_protocol::MAX_WORKSPACE_PAGE {
            return Err(SessionRuntimeError::InvalidPageLimit);
        }
        if request
            .after
            .is_some_and(|after| after.store_id != self.inner.store.store_id())
        {
            return Err(SessionRuntimeError::CursorStoreMismatch);
        }
        self.inner.store.workspace_catalog(request).await
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

    pub async fn goal_pending_runs(
        &self,
        session_id: SessionId,
        expected: qq_protocol::GoalIdentity,
    ) -> Result<Vec<RunId>, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner
            .store
            .goal_pending_runs(session_id, expected)
            .await
    }

    pub async fn cancelled_goal_runs(
        &self,
        session_id: SessionId,
        goal_id: qq_protocol::GoalId,
    ) -> Result<Vec<RunId>, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner
            .store
            .cancelled_goal_runs(session_id, goal_id)
            .await
    }

    pub async fn goal_owns_run(
        &self,
        session_id: SessionId,
        expected: qq_protocol::GoalIdentity,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner
            .store
            .goal_owns_run(session_id, expected, run_id)
            .await
    }

    pub async fn completed_runs(
        &self,
        session_id: SessionId,
        after: EventCursor,
        through: EventCursor,
    ) -> Result<Vec<RunCompletion>, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if after.store_id != self.inner.store.store_id()
            || through.store_id != self.inner.store.store_id()
        {
            return Err(SessionRuntimeError::CursorStoreMismatch);
        }
        if after.workspace_id != through.workspace_id {
            return Err(SessionRuntimeError::CursorWorkspaceMismatch);
        }
        self.inner
            .store
            .completed_runs(session_id, after, through)
            .await
    }

    pub async fn approval_sessions(
        &self,
        workspace_id: WorkspaceId,
        root: SessionId,
    ) -> Result<Vec<SessionId>, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner.store.approval_sessions(workspace_id, root).await
    }

    pub async fn run_result(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<Option<RunResult>, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner
            .store
            .run_result(workspace_id, session_id, run_id)
            .await
    }

    pub async fn run_snapshot(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<qq_protocol::RunSnapshot, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        self.inner
            .store
            .run_snapshot(workspace_id, session_id, run_id)
            .await
    }

    pub fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<SessionEventStream, SessionRuntimeError> {
        let published = self.subscribe_published(request)?;
        Ok(Box::pin(
            published.map(|item| item.map(PublishedEvent::into_envelope)),
        ))
    }

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
                            after = event.envelope.cursor.sequence;
                            yield Ok(event);
                        }
                        Err(feed::RecvError::Behind) => break,
                        Err(feed::RecvError::Closed) => return,
                    }
                }
            }
        }))
    }

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
                .settlement_cancel(command_id, run_id)
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
            if self.inner.side_tasks.load(Ordering::Acquire) == 0
                && self.inner.restores.load(Ordering::Acquire) == 0
                && unfinished.is_empty()
                && preparation_quiescent
                && self
                    .inner
                    .side_sessions
                    .lock()
                    .expect("side admission lock")
                    .is_empty()
            {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, async {
                tokio::select! {
                    changed = settlements.changed() => changed,
                    () = tokio::time::sleep(Duration::from_millis(10)) => Ok(()),
                }
            })
            .await
            .map_err(|_| SessionRuntimeError::ShutdownTimedOut)?
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        }
    }

    pub async fn close(&self) -> Result<(), SessionRuntimeError> {
        self.shutdown().await?;
        self.inner.store.close().await
    }

    pub async fn has_preempting_goal_work(
        &self,
        session_id: SessionId,
        goal: qq_protocol::GoalId,
    ) -> Result<bool, SessionRuntimeError> {
        self.inner
            .store
            .has_preempting_goal_work(session_id, goal)
            .await
    }

    pub async fn close_when_idle(
        &self,
        interrupt: impl std::future::Future<Output = ()>,
    ) -> Result<(), SessionRuntimeError> {
        tokio::pin!(interrupt);
        {
            let _lifecycle = self.inner.lifecycle.write().await;
            self.inner.draining.store(true, Ordering::Release);
        }
        let mut settlements = self.inner.settlements.subscribe();
        let mut failed = self.inner.failed.subscribe();
        let mut shutdown = self.inner.shutdown.subscribe();
        loop {
            let lifecycle = self.inner.lifecycle.write().await;
            if *failed.borrow() || *shutdown.borrow() {
                return Err(SessionRuntimeError::Unavailable);
            }
            let unfinished = tokio::select! {
                biased;
                () = interrupt.as_mut() => {
                    drop(lifecycle);
                    return self.close().await;
                }
                result = self.inner.store.unfinished_run_ids() => result?,
            };
            if unfinished.is_empty() {
                self.inner.shutdown.send_replace(true);
                drop(lifecycle);
                return self.close().await;
            }
            drop(lifecycle);
            tokio::select! {
                biased;
                () = interrupt.as_mut() => return self.close().await,
                changed = settlements.changed() => {
                    changed.map_err(|_| SessionRuntimeError::Unavailable)?;
                }
                changed = failed.changed() => {
                    changed.map_err(|_| SessionRuntimeError::Unavailable)?;
                }
                changed = shutdown.changed() => {
                    changed.map_err(|_| SessionRuntimeError::Unavailable)?;
                }
            }
        }
    }

    #[doc(hidden)]
    pub async fn abandon_for_test(&self) -> Result<(), SessionRuntimeError> {
        let Some(worker) = self.inner.store.stop_worker_for_test() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || worker.join())
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
            .map_err(|_| SessionRuntimeError::Unavailable)
    }

    pub(super) fn request_schedule(&self) {
        if *self.inner.shutdown.borrow() {
            return;
        }
        let _ = self.inner.schedule.try_send(());
    }
}

impl SessionRuntimeInner {
    pub(super) async fn resolve_goal_policy(
        &self,
        workspace: &Path,
    ) -> Result<GoalPolicy, GoalPolicyError> {
        let policy = match &self.grant_authority {
            Some(authority) => {
                let mut shutdown = self.shutdown.subscribe();
                let mut failed = self.failed.subscribe();
                tokio::select! {
                    biased;
                    _ = shutdown.wait_for(|stopped| *stopped) => Err(GoalPolicyError::Unavailable),
                    _ = failed.wait_for(|stopped| *stopped) => Err(GoalPolicyError::Unavailable),
                    result = tokio::time::timeout(Duration::from_secs(10), authority.goal_policy(workspace)) => {
                        result.unwrap_or(Err(GoalPolicyError::Unavailable))
                    }
                }
            }
            None => Ok(GoalPolicy::default()),
        };
        policy.and_then(GoalPolicy::validate)
    }

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
pub enum SlashCommandError {
    #[error(
        "/{name} is not a valid command name: names start with a lowercase ASCII letter, contain only lowercase ASCII letters, digits, '-' or '_', and are at most 64 bytes; prefix with // to send it as text"
    )]
    InvalidName { name: String },
    #[error(
        "/{name} is a client command, not a prompt; run it from the client or prefix with // to send it as text"
    )]
    Reserved { name: String },
}

pub(super) fn validate_slash_prompt(prompt: &str) -> Result<(), SlashCommandError> {
    if prompt.starts_with("//") {
        return Ok(());
    }
    let Some(invocation) = prompt.strip_prefix('/') else {
        return Ok(());
    };
    let name = invocation
        .split_once(char::is_whitespace)
        .map_or(invocation, |(name, _)| name);
    let bounded = |name: &str| -> String { name.chars().take(64).collect() };
    if !crate::workspace::valid_slash_name(name) {
        return Err(SlashCommandError::InvalidName {
            name: bounded(name),
        });
    }
    if qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS
        .iter()
        .any(|reserved| reserved.strip_prefix('/') == Some(name))
    {
        return Err(SlashCommandError::Reserved {
            name: name.to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionRuntimeError {
    #[error("goal identity or revision conflicts with the current goal")]
    GoalConflict {
        current: Option<qq_protocol::GoalIdentity>,
    },
    #[error("goal was not found")]
    GoalNotFound,
    #[error("an achieved goal cannot be edited or resumed; clear or replace it")]
    GoalImmutable,
    #[error("goal cancellation is still draining")]
    GoalCancellationPending,
    #[error("the interrupted check does not match an unacknowledged workspace hold")]
    CheckInterruptionMismatch,
    #[error("the named question is not pending in this goal's active paid family")]
    GoalInputNotPending,
    #[error("goal check authorization is required")]
    GoalCheckApprovalRequired,
    #[error("goal has no check to authorize")]
    GoalCheckMissing,
    #[error("approval command does not match the goal's current check")]
    GoalCheckCommandMismatch,
    #[error("goal check is forbidden by shell policy")]
    GoalCheckForbidden,
    #[error(transparent)]
    GoalCheckPolicy(#[from] GoalCheckPolicyError),
    #[error(transparent)]
    GoalPolicy(#[from] GoalPolicyError),
    #[error("goal budget or deadline has been spent")]
    GoalBudgetSpent,
    #[error("invalid goal: {0}")]
    InvalidGoal(GoalValidationError),
    #[error(
        "tool projection for run {run} is unresolved: it lacks the exact tool-result projection needed to preserve its history; start a new session to continue without changing this history"
    )]
    ToolProjectionUnavailable { run: String },
    #[error("maximum active runs must be greater than zero")]
    InvalidRunLimit,
    #[error("a side question is already active for this session")]
    SideQuestionBusy,
    #[error("side questions must contain 1 to 8192 bytes")]
    InvalidSideQuestion,
    #[error("side question was cancelled or interrupted")]
    SideQuestionCancelled,
    #[error("side question deadline expired")]
    SideQuestionTimedOut,
    #[error("side question was not found")]
    SideQuestionNotFound,
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
    #[error("{0}")]
    InvalidSlashCommand(SlashCommandError),
    #[error("run limits must be greater than zero and within the runtime ceilings")]
    InvalidRunLimits,
    #[error("invalid output contract: {0}")]
    InvalidOutputContract(crate::output::OutputSchemaError),
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
    #[error(
        "a run in this workspace is active, queued or due from a goal; cancel or pause it, then undo"
    )]
    WorkspaceBusy,
    #[error("undo failed: {0}")]
    UndoFailed(#[source] crate::undo::UndoFailure),
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
    #[error("pending approval details exceed the snapshot byte budget")]
    SnapshotTooLarge,
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
    #[error("delivery history expired; load a fresh snapshot")]
    CursorExpired,
    #[error("session accounting is unavailable")]
    AccountingUnavailable,
    #[error("session runtime is overloaded")]
    Overloaded,
    #[error("session runtime shutdown timed out before every run settled")]
    ShutdownTimedOut,
    #[error("session runtime is unavailable")]
    Unavailable,
    #[error("session store is owned by another running qq process")]
    StoreBusy,
    #[error("session persistence failed: {0}")]
    Persistence(PersistenceFault),
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceFault {
    #[error("sqlite reported {0:?}")]
    Sqlite(rusqlite::ffi::ErrorCode),
    #[error("a stored row failed to decode")]
    Codec,
    #[error("a store invariant was violated")]
    Constraint,
}

impl SessionRuntimeError {
    pub(crate) const CONSTRAINT: Self = Self::Persistence(PersistenceFault::Constraint);
    pub(crate) const CODEC: Self = Self::Persistence(PersistenceFault::Codec);
}

impl From<rusqlite::Error> for SessionRuntimeError {
    fn from(error: rusqlite::Error) -> Self {
        use rusqlite::Error;
        Self::Persistence(match error {
            Error::SqliteFailure(failure, _) => match failure.code {
                rusqlite::ffi::ErrorCode::ConstraintViolation => PersistenceFault::Constraint,
                code => PersistenceFault::Sqlite(code),
            },
            Error::QueryReturnedNoRows
            | Error::InvalidColumnIndex(_)
            | Error::InvalidColumnName(_)
            | Error::InvalidColumnType(..)
            | Error::FromSqlConversionFailure(..)
            | Error::IntegralValueOutOfRange(..)
            | Error::Utf8Error(_)
            | Error::NulError(_)
            | Error::InvalidParameterName(_)
            | Error::InvalidPath(_)
            | Error::InvalidQuery
            | Error::MultipleStatement
            | Error::InvalidParameterCount(..)
            | Error::StatementChangedRows(_)
            | Error::ToSqlConversionFailure(_)
            | Error::InvalidDatabaseIndex(_)
            | Error::ExecuteReturnedResults
            | Error::SqlInputError { .. } => PersistenceFault::Codec,
            _ => PersistenceFault::Sqlite(rusqlite::ffi::ErrorCode::Unknown),
        })
    }
}

impl From<serde_json::Error> for SessionRuntimeError {
    fn from(_: serde_json::Error) -> Self {
        Self::CODEC
    }
}

const MAX_UNDO_CHANGE_BYTES: usize = 8 * 1024;

struct RestoreSlot(Arc<SessionRuntimeInner>);

impl RestoreSlot {
    fn new(inner: &Arc<SessionRuntimeInner>) -> Self {
        inner.restores.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(inner))
    }
}

impl Drop for RestoreSlot {
    fn drop(&mut self) {
        self.0.restores.fetch_sub(1, Ordering::AcqRel);
        self.0
            .settlements
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

struct RestoredSnapshot {
    snapshot: String,
    restored: crate::undo::Restored,
}

impl RestoredSnapshot {
    fn outcome(&self, workspace_id: WorkspaceId) -> CommandOutcome {
        CommandOutcome::WorkspaceUndone {
            workspace_id,
            snapshot: self.snapshot.clone(),
            before: self.restored.before.name.clone(),
            changes: bounded_changes(&self.restored.changes),
            changes_total: u32::try_from(self.restored.changes.len()).unwrap_or(u32::MAX),
            head_moved: self.restored.head_moved.is_some(),
        }
    }
}

async fn restore_snapshot(
    workspace: &Path,
    name: Option<&str>,
) -> Result<RestoredSnapshot, crate::undo::UndoError> {
    let mut snapshots = crate::undo::list(workspace).await?;
    snapshots.retain(|snapshot| snapshot.workspace == workspace);
    let target = crate::undo::select(&snapshots, workspace, name)?;
    Ok(RestoredSnapshot {
        snapshot: target.name.clone(),
        restored: crate::undo::restore(target).await?,
    })
}

pub(super) fn bounded_changes(changes: &[String]) -> Vec<String> {
    let mut bytes = 0;
    changes
        .iter()
        .take_while(|change| {
            bytes += serde_json::to_string(change).map_or(usize::MAX, |encoded| encoded.len() + 1);
            bytes <= MAX_UNDO_CHANGE_BYTES
        })
        .cloned()
        .collect()
}
