//! Trusted preparation and publication; payloads receive only the detached root.
use super::{claim::ClaimedRun, runtime::SessionRuntimeInner, store::Priority, *};
use crate::{
    command_owner::{
        NativeCommandScope, NativeDirectoryGrant, NativeNetwork, NativeResources,
        NativeWorkspaceIdentity,
    },
    task_workspace::{TaskWorkspace, TaskWorkspaceError, TaskWorkspaceLimits, TaskWorkspaceSource},
    workspace::Workspace,
};
use std::{collections::BTreeSet, io};

const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const RUN_PREFIX: &str = "qq_task_run:";
const RECORD_PREFIX: &str = "qq_task_workspace:";
const GOAL_PREFIX: &str = "qq_task_goal:";
const PRIVATE_HOME: &str = ".qq/task-home";
const PUBLICATION_PREFIX: &str = "qq_task_publication:";

#[cfg(test)]
static PREPARATION_WAITERS: Mutex<Vec<(String, tokio::sync::oneshot::Sender<()>)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
struct PublicationGate {
    source: String,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
static PUBLICATION_GATES: Mutex<Vec<PublicationGate>> = Mutex::new(Vec::new());

#[cfg(test)]
static PUBLICATION_WAITERS: Mutex<Vec<(String, tokio::sync::oneshot::Sender<()>)>> =
    Mutex::new(Vec::new());

/// Authority supplied by the server, independently of project/model output.
/// `directory` is a private control root outside every project and task grant.
#[derive(Debug, Clone)]
pub struct TaskWorkspacePolicy {
    pub environment: Option<crate::command_owner::NativeEnvironment>,
    pub directory: NativeDirectoryGrant,
    pub read_only: Vec<NativeDirectoryGrant>,
    pub resources: NativeResources,
    pub limits: TaskWorkspaceLimits,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum TaskExecutionError {
    #[error("task preparation was cancelled or exceeded its deadline")]
    Stopped,
    #[error("invalid task execution state: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Storage(#[from] crate::storage::Pressure),
    #[error("task workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Workspace(TaskWorkspaceError),
    #[error("task workspace traversal failed: {0}")]
    Traverse(#[from] crate::tools::walk::ListError),
    #[error("task record could not be encoded or decoded: {0}")]
    Codec(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] SessionRuntimeError),
    #[error("task source configuration failed: {0}")]
    Runtime(#[from] RuntimeLoadError),
    #[error("task workspace preparation stopped: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl From<TaskWorkspaceError> for TaskExecutionError {
    fn from(error: TaskWorkspaceError) -> Self {
        match error {
            TaskWorkspaceError::Stopped => Self::Stopped,
            error => Self::Workspace(error),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRecord {
    source: NativeDirectoryGrant,
    workspace: TaskWorkspace,
    scope: NativeCommandScope,
}

#[derive(Debug)]
pub(super) struct TaskExecution {
    owner: RunId,
    publisher: bool,
    source_request: RuntimeLoadRequest,
    pub(super) scope: NativeCommandScope,
    record: Mutex<TaskRecord>,
}

impl TaskExecution {
    pub(super) fn directory_grant(&self) -> NativeDirectoryGrant {
        self.scope.workspace.clone()
    }

    pub(super) fn home(&self) -> PathBuf {
        self.scope.workspace.path.join(PRIVATE_HOME)
    }

    /// Called only with the session's teardown proof, after descendants drain.
    pub(super) async fn publish(
        self: Arc<Self>,
        inner: &SessionRuntimeInner,
        session: SessionId,
        run: RunId,
    ) -> Result<(), TaskExecutionError> {
        if !self.publisher {
            return Ok(());
        }
        let mut cancellation = inner
            .cancellations
            .lock()
            .map_err(|_| TaskExecutionError::Invalid("run cancellation registry unavailable"))?
            .get(&run)
            .map(watch::Sender::subscribe)
            .ok_or(TaskExecutionError::Invalid(
                "publication run has no cancellation owner",
            ))?;
        if *cancellation.borrow() {
            return Err(TaskExecutionError::Stopped);
        }
        // Configuration is trusted source authority. Reload it after payloads
        // drain so newly protected pack files cannot be installed as output.
        let loaded = inner.loader.load(self.source_request.clone()).await?;
        let task = self.clone();
        #[cfg(test)]
        {
            let observer = {
                let mut observers = PUBLICATION_WAITERS.lock().unwrap();
                observers
                    .iter()
                    .position(|(source, _)| source == &self.source_request.workspace)
                    .map(|index| observers.remove(index).1)
            };
            if let Some(observer) = observer {
                let _ = observer.send(());
            }
        }
        let permit = tokio::select! {
            biased;
            () = async {
                loop {
                    if *cancellation.borrow() { break; }
                    if cancellation.changed().await.is_err() { break; }
                }
            } => return Err(TaskExecutionError::Stopped),
            permit = crate::workspace::blocking_permits().acquire_owned() => {
                permit.map_err(|_| TaskExecutionError::Invalid("filesystem workers unavailable"))?
            }
        };
        let stopped = cancellation.clone();
        let (outputs, permit) = tokio::task::spawn_blocking(move || {
            let is_stopped = || *stopped.borrow();
            if is_stopped() {
                return Err(TaskExecutionError::Stopped);
            }
            let mut record = task
                .record
                .lock()
                .map_err(|_| TaskExecutionError::Invalid("task record poisoned"))?;
            record
                .workspace
                .protect_source(&TaskWorkspaceSource::from_plan(&loaded.plan)?)?;
            let root = Workspace::open(record.workspace.path())?;
            crate::storage::check_blocking(&root, 0)?;
            crate::storage::check_blocking(
                &loaded.plan.workspace_handle(),
                record.workspace_limits().max_bytes,
            )?;
            let mut outputs: BTreeSet<_> = record.workspace.input_paths().cloned().collect();
            outputs.extend(files_until(
                &root,
                record.workspace_limits(),
                false,
                &is_stopped,
            )?);
            if outputs.len() > record.workspace_limits().max_entries {
                return Err(TaskExecutionError::Invalid("too many task outputs"));
            }
            Ok::<_, TaskExecutionError>((outputs.into_iter().collect::<Vec<_>>(), permit))
        })
        .await??;
        // This durable decision orders publication against cancellation. No
        // store transaction or record lock spans filesystem work. Cancellation
        // before this point wins; after it, settlement must report the outcome
        // of the committed publication, including any publication failure.
        if !authorize_publication(&inner.store, session, run).await? {
            return Err(TaskExecutionError::Stopped);
        }
        #[cfg(test)]
        {
            let gate = {
                let mut gates = PUBLICATION_GATES.lock().unwrap();
                gates
                    .iter()
                    .position(|gate| gate.source == self.source_request.workspace)
                    .map(|index| gates.remove(index))
            };
            if let Some(gate) = gate {
                let _ = gate.entered.send(());
                let _ = gate.release.await;
            }
        }
        let task = self.clone();
        let encoded = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut record = task
                .record
                .lock()
                .map_err(|_| TaskExecutionError::Invalid("task record poisoned"))?;
            record.workspace.publish(&outputs)?;
            encode(&record)
        })
        .await??;
        save(
            &inner.store,
            format!("{RECORD_PREFIX}{}", self.owner),
            encoded,
        )
        .await?;
        Ok(())
    }
}

/// Publication is an irrevocable file-install decision, not permission for a
/// new agent action. Its marker lives in trusted storage alongside task records.
async fn authorize_publication(
    store: &Store,
    session: SessionId,
    run: RunId,
) -> Result<bool, SessionRuntimeError> {
    store.call_write(Priority::AwaitControl, move |connection| {
        let transaction = super::store::begin_unit(connection)?;
        let cancelled = transaction.query_row(
            "SELECT cancel_requested FROM runs WHERE id=?1 AND session_id=?2 AND status='running' AND outcome_json IS NULL",
            params![run.to_string(), session.to_string()], |row| row.get::<_, bool>(0),
        ).optional()?.ok_or(SessionRuntimeError::CONSTRAINT)?;
        if publication_authorized(&transaction, run)? { return Ok(true); }
        if cancelled { return Ok(false); }
        let policy = super::tool_calls::load_tool_approval_policy(&transaction, session)?;
        if !matches!(policy.mode, ApprovalMode::Auto | ApprovalMode::Full)
            || policy.ancestors.iter().any(|(mode, _)| !matches!(mode, ApprovalMode::Auto | ApprovalMode::Full))
        {
            return Err(SessionRuntimeError::InvalidNativeScope { reason: "source publication permission changed".to_owned() });
        }
        transaction.execute("INSERT INTO metadata(key,value) VALUES(?1,'authorized') ON CONFLICT(key) DO NOTHING", [format!("{PUBLICATION_PREFIX}{run}")])?;
        transaction.commit()?;
        Ok(true)
    }).await
}

pub(super) fn publication_authorized(
    connection: &Connection,
    run: RunId,
) -> Result<bool, SessionRuntimeError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1 AND value='authorized')",
        [format!("{PUBLICATION_PREFIX}{run}")],
        |row| row.get(0),
    )?)
}

impl TaskRecord {
    fn validate_source(
        &mut self,
        source: &NativeDirectoryGrant,
        protections: &TaskWorkspaceSource,
    ) -> Result<(), TaskExecutionError> {
        if self.source.path != source.path
            || self.source.identity != source.identity
            || self.workspace.path() != self.scope.workspace.path
            || self.workspace.directory_grant().identity != self.scope.workspace.identity
        {
            return Err(TaskExecutionError::Invalid(
                "task record does not match this project",
            ));
        }
        self.workspace.protect_source(protections)?;
        Ok(())
    }

    fn workspace_limits(&self) -> TaskWorkspaceLimits {
        self.workspace.limits()
    }
}

pub(super) async fn prepare(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    source_plan: Arc<crate::plan::CompiledAgentPlan>,
    cancellation: &RunCancellation,
    deadline: Option<tokio::time::Instant>,
) -> Result<Option<Arc<TaskExecution>>, TaskExecutionError> {
    let Some(policy) = inner.task_workspaces.clone() else {
        return Ok(None);
    };
    if claimed.identity.kind == RunKind::Compaction {
        return Ok(None);
    }
    let session = claimed.identity.session_id.to_string();
    let parent: Option<String> = inner.store.call(Priority::AwaitControl, move |connection| {
        Ok(connection.query_row(
            "SELECT m.value FROM sessions s JOIN metadata m ON m.key=?2||s.owner_run_id WHERE s.id=?1",
            params![session, RUN_PREFIX], |row| row.get(0),
        ).optional()?)
    }).await?;
    if parent.is_none() && claimed.approval_mode != ApprovalMode::Auto {
        return Ok(None);
    }
    let publisher = parent.is_none();
    // Only goal-origin work resumes a goal's tree. Ordinary prompts and
    // completion checks do not revive it; checks import the published artifact.
    let goal_key = match claimed.origin.as_ref() {
        Some(qq_protocol::RunOrigin::Goal { goal_id, .. }) if publisher => {
            Some(format!("{GOAL_PREFIX}{goal_id}"))
        }
        _ => None,
    };
    let goal_owner = match &goal_key {
        Some(key) => read(&inner.store, key.clone()).await?,
        None => None,
    };
    let existing = parent.or(goal_owner);
    let owner = match existing.as_deref() {
        Some(owner) => owner
            .parse()
            .map_err(|_| TaskExecutionError::Invalid("invalid task owner"))?,
        None => claimed.identity.run_id,
    };
    let saved = read(&inner.store, format!("{RECORD_PREFIX}{owner}")).await?;
    if existing.is_some() && saved.is_none() {
        return Err(TaskExecutionError::Invalid(
            "existing task record is missing",
        ));
    }
    // A verifier gets fresh source bytes, including ignored outputs recorded
    // by the goal's last publication. It never resumes that private tree or
    // replaces the goal's owner with the verifier's own fresh task owner.
    let published = match claimed.origin.as_ref() {
        Some(qq_protocol::RunOrigin::GoalCheck { goal_id, .. }) if publisher => {
            match read(&inner.store, format!("{GOAL_PREFIX}{goal_id}")).await? {
                Some(owner) => {
                    let owner: RunId = owner.parse().map_err(|_| {
                        TaskExecutionError::Invalid("invalid goal publication owner")
                    })?;
                    Some(
                        read(&inner.store, format!("{RECORD_PREFIX}{owner}"))
                            .await?
                            .ok_or(TaskExecutionError::Invalid(
                                "goal publication record is missing",
                            ))?,
                    )
                }
                None => None,
            }
        }
        _ => None,
    };
    let source_request = RuntimeLoadRequest {
        execution_workspace: None,
        workspace: claimed.workspace.clone(),
        model: claimed.model.clone(),
        profile: claimed.profile.clone(),
        reasoning_effort: claimed.reasoning_effort,
    };
    #[cfg(test)]
    {
        let observer = {
            let mut observers = PREPARATION_WAITERS.lock().unwrap();
            observers
                .iter()
                .position(|(path, _)| path == &claimed.workspace)
                .map(|index| observers.remove(index).1)
        };
        if let Some(observer) = observer {
            let _ = observer.send(());
        }
    }
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(TaskExecutionError::Stopped),
        () = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        } => {
            cancellation.expire();
            return Err(TaskExecutionError::Stopped);
        }
        permit = crate::workspace::blocking_permits().acquire_owned() => {
            permit.map_err(|_| TaskExecutionError::Invalid("filesystem workers unavailable"))?
        }
    };
    let stopped = cancellation.clone();
    let attachments = claimed
        .input
        .iter()
        .filter_map(|part| match part {
            InputPart::WorkspaceFile { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let prepared = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let is_stopped = || {
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                stopped.expire();
            }
            stopped.is_cancelled()
        };
        if is_stopped() {
            return Err(TaskExecutionError::Stopped);
        }
        let source = source_plan.workspace_handle();
        let source_grant = NativeDirectoryGrant {
            path: source.path().to_owned(),
            identity: NativeWorkspaceIdentity::held(source.root())?,
        };
        let source_authority = TaskWorkspaceSource::from_plan(&source_plan)?;
        let mut policy = policy;
        // A software-input root must never expose the current source checkout's
        // protected configuration or Git trust files to its detached payload.
        policy.read_only.retain(|root| {
            !root.path.starts_with(&source_grant.path) && !source_grant.path.starts_with(&root.path)
        });
        let mut record: TaskRecord = match saved {
            Some(saved) => serde_json::from_str(&saved)?,
            None => {
                if NativeWorkspaceIdentity::capture(&policy.directory.path)?
                    != policy.directory.identity
                {
                    return Err(TaskExecutionError::Invalid(
                        "task control directory changed",
                    ));
                }
                let control = Workspace::open(&policy.directory.path)?;
                crate::storage::check_blocking(&control, policy.limits.max_bytes)?;
                let mut inputs: BTreeSet<_> =
                    files_until(&source, policy.limits, true, &is_stopped)?
                        .into_iter()
                        .collect();
                if let Some(published) = published {
                    let mut published: TaskRecord = serde_json::from_str(&published)?;
                    published.validate_source(&source_grant, &source_authority)?;
                    for path in published.workspace.input_paths() {
                        if is_stopped() {
                            return Err(TaskExecutionError::Stopped);
                        }
                        crate::task_workspace::validate_path(path)?;
                        if source
                            .check_write_path(&path.to_string_lossy(), path)
                            .is_err()
                        {
                            continue;
                        }
                        match source.root().symlink_metadata(path) {
                            Ok(_) => {
                                inputs.insert(path.clone());
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error.into()),
                        }
                        if inputs.len() > policy.limits.max_entries {
                            return Err(TaskWorkspaceError::LimitExceeded.into());
                        }
                    }
                }
                // Explicit human inputs override ignore rules. Editable files
                // get private copies and publication baselines; protected
                // configuration remains source authority and is only attached.
                for path in attachments {
                    if is_stopped() {
                        return Err(TaskExecutionError::Stopped);
                    }
                    if let Ok(relative) = source.contained_path(&path)
                        && source.check_write_path(&path, &relative).is_ok()
                        && source.root().is_file(&relative)
                    {
                        inputs.insert(relative);
                    }
                }
                let workspace = TaskWorkspace::import_until(
                    &source_authority,
                    &policy.directory.path.join(owner.to_string()),
                    &inputs.into_iter().collect::<Vec<_>>(),
                    policy.limits,
                    &is_stopped,
                )?;
                let scope = NativeCommandScope {
                    environment: policy.environment.clone(),
                    workspace: workspace.directory_grant(),
                    read_only: policy.read_only.clone(),
                    network: NativeNetwork::Isolated,
                    resources: policy.resources,
                };
                TaskRecord {
                    source: source_grant.clone(),
                    workspace,
                    scope,
                }
            }
        };
        record.validate_source(&source_grant, &source_authority)?;
        // A persisted tree retains work, not a permanent grant. Reapply the
        // trusted host's current limits and readable roots on each admission.
        record.scope.read_only = policy.read_only;
        record.scope.network = NativeNetwork::Isolated;
        record.scope.resources = policy.resources;
        record.workspace.apply_limits(policy.limits)?;
        let held = Workspace::open(record.workspace.path())?;
        record
            .scope
            .validate_workspace(held.path(), NativeWorkspaceIdentity::held(held.root())?)?;
        crate::storage::check_blocking(&held, 0)?;
        for protected in source.protected_pack_roots() {
            if is_stopped() {
                return Err(TaskExecutionError::Stopped);
            }
            if !protected.as_os_str().is_empty() {
                held.root().create_dir_all(protected)?;
            }
        }
        held.root().create_dir_all(format!("{PRIVATE_HOME}/tmp"))?;
        #[cfg(windows)]
        for directory in ["AppData/Local", "AppData/Roaming"] {
            held.root()
                .create_dir_all(format!("{PRIVATE_HOME}/{directory}"))?;
        }
        let encoded = encode(&record)?;
        Ok::<_, TaskExecutionError>((
            Arc::new(TaskExecution {
                owner,
                publisher,
                source_request,
                scope: record.scope.clone(),
                record: Mutex::new(record),
            }),
            encoded,
        ))
    })
    .await??;
    if cancellation.is_cancelled() {
        return Err(TaskExecutionError::Stopped);
    }
    save(&inner.store, format!("{RECORD_PREFIX}{owner}"), prepared.1).await?;
    save(
        &inner.store,
        format!("{RUN_PREFIX}{}", claimed.identity.run_id),
        owner.to_string(),
    )
    .await?;
    if let Some(key) = goal_key {
        save(&inner.store, key, owner.to_string()).await?;
    }
    Ok(Some(prepared.0))
}

/// Prepare only after the verifier has acquired its workspace exclusion, so it
/// checks the published artifact rather than a snapshot taken while work runs.
pub(super) async fn prepare_check(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    cancellation: &RunCancellation,
    deadline: tokio::time::Instant,
    resources: &mut execution::RunResources,
) -> Result<(), TaskExecutionError> {
    if inner.task_workspaces.is_none() {
        return Ok(());
    }
    let progress = RuntimeLoadProgress::default();
    let mut load = inner.loader.load_with_progress(
        RuntimeLoadRequest {
            execution_workspace: None,
            workspace: claimed.workspace.clone(),
            model: claimed.model.clone(),
            profile: claimed.profile.clone(),
            reasoning_effort: claimed.reasoning_effort,
        },
        progress.clone(),
    );
    let loaded = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            progress.cancel(); let _ = load.await;
            return Err(TaskExecutionError::Invalid("check preparation was cancelled"));
        }
        () = tokio::time::sleep_until(deadline) => {
            cancellation.expire(); progress.cancel(); let _ = load.await;
            return Err(TaskExecutionError::Invalid("check preparation exceeded its deadline"));
        }
        result = &mut load => result?,
    };
    if loaded.plan.workspace_path() != Path::new(&claimed.workspace) {
        return Err(TaskExecutionError::Invalid(
            "check source plan names another project",
        ));
    }
    if let Some(task) = prepare(inner, claimed, loaded.plan, cancellation, Some(deadline)).await? {
        resources.bind_task(task)?;
    }
    Ok(())
}

#[cfg(test)]
fn files(
    workspace: &Workspace,
    limits: TaskWorkspaceLimits,
    inputs: bool,
) -> Result<Vec<PathBuf>, TaskExecutionError> {
    files_until(workspace, limits, inputs, &|| false)
}

fn files_until(
    workspace: &Workspace,
    limits: TaskWorkspaceLimits,
    inputs: bool,
    stopped: &dyn Fn() -> bool,
) -> Result<Vec<PathBuf>, TaskExecutionError> {
    use crate::tools::walk::{
        EntryKind, GENERATED_DIRECTORIES, IgnoreStack, ListingLimits, list_children_bounded,
    };
    if stopped() {
        return Err(TaskExecutionError::Stopped);
    }
    let mut ignores = IgnoreStack::for_task_files(workspace, inputs);
    let mut pending = vec![Some(".".to_owned())];
    let mut visited = 0usize;
    let mut path_bytes = 0usize;
    let mut selected = Vec::new();
    while let Some(directory) = pending.pop() {
        if stopped() {
            return Err(TaskExecutionError::Stopped);
        }
        let Some(directory) = directory else {
            ignores.leave();
            continue;
        };
        let mut unreadable = 0;
        let children = list_children_bounded(
            workspace,
            &directory,
            &mut ignores,
            &mut unreadable,
            Some(ListingLimits {
                max_entries: limits.max_entries.saturating_sub(visited),
                max_bytes: MAX_RECORD_BYTES.saturating_sub(path_bytes),
                deadline: None,
                cancelled: Some(stopped),
                #[cfg(test)]
                observe: None,
            }),
        )
        .map_err(|error| {
            if stopped() {
                TaskExecutionError::Stopped
            } else {
                TaskExecutionError::Traverse(error)
            }
        })?;
        if unreadable != 0 {
            return Err(TaskExecutionError::Invalid(
                "task filenames must be Unicode",
            ));
        }
        visited += children.len();
        pending.push(None);
        for child in children.into_iter().rev() {
            if stopped() {
                return Err(TaskExecutionError::Stopped);
            }
            path_bytes = path_bytes.saturating_add(child.path.len());
            if path_bytes > MAX_RECORD_BYTES {
                return Err(TaskExecutionError::Invalid(
                    "task path selection exceeds its byte limit",
                ));
            }
            let relative = PathBuf::from(&child.path);
            if child.ignored
                || relative.starts_with(PRIVATE_HOME)
                || (!inputs
                    && child.is_dir()
                    && GENERATED_DIRECTORIES.contains(&child.name.as_str()))
                || (inputs
                    && relative != Path::new(".qq")
                    && workspace.check_write_path(&child.path, &relative).is_err())
            {
                continue;
            }
            match child.kind {
                EntryKind::Dir => pending.push(Some(child.path)),
                EntryKind::File { .. } => selected.push(relative),
                EntryKind::Symlink | EntryKind::Other if !inputs => {
                    return Err(TaskWorkspaceError::UnsupportedFile(relative).into());
                }
                _ => {}
            }
        }
    }
    Ok(selected)
}

fn encode(record: &TaskRecord) -> Result<String, TaskExecutionError> {
    let value = serde_json::to_string(record)?;
    if value.len() > MAX_RECORD_BYTES {
        return Err(TaskExecutionError::Invalid(
            "task record exceeds its byte limit",
        ));
    }
    Ok(value)
}

pub(super) fn is_bound(connection: &Connection, run: RunId) -> Result<bool, rusqlite::Error> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
        [format!("{RUN_PREFIX}{run}")],
        |row| row.get(0),
    )
}

/// Resolve live inspection authority from trusted run records, never from the
/// model or the source project. No active task means ordinary source inspection.
pub(super) async fn side_scope(
    inner: &SessionRuntimeInner,
    session: SessionId,
    source_path: &str,
) -> Result<Option<NativeCommandScope>, SessionRuntimeError> {
    let (active, parent, mode): (Option<String>, Option<String>, String) = inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            Ok(connection.query_row(
                "SELECT COALESCE(active_run_id, preparing_run_id), owner_run_id, approval_mode FROM sessions WHERE id=?1",
                [session.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .await?;
    let invalid = |reason: &str| SessionRuntimeError::InvalidNativeScope {
        reason: reason.to_owned(),
    };
    for run in [active.as_ref(), parent.as_ref()].into_iter().flatten() {
        let run = run.parse().map_err(|_| invalid("invalid task run"))?;
        if let Some(scope) = scope_for_run(&inner.store, run, Path::new(source_path)).await? {
            return Ok(Some(scope));
        }
    }
    // A question may race the main run's task preparation. Do not inspect the
    // broader source project while an Auto task's scope is still being admitted.
    if active.is_some()
        && inner.task_workspaces.is_some()
        && parse_approval_mode(&mode)? == ApprovalMode::Auto
    {
        return Err(SessionRuntimeError::SideQuestionBusy);
    }
    Ok(None)
}

/// Restore only the authority already bound to this run. Missing or changed
/// bound records are errors; they must never fall back to the source project.
pub(super) async fn scope_for_run(
    store: &Store,
    run: RunId,
    source_path: &Path,
) -> Result<Option<NativeCommandScope>, SessionRuntimeError> {
    let invalid = |reason: &str| SessionRuntimeError::InvalidNativeScope {
        reason: reason.to_owned(),
    };
    let Some(owner) = read(store, format!("{RUN_PREFIX}{run}")).await? else {
        return Ok(None);
    };
    let owner: RunId = owner.parse().map_err(|_| invalid("invalid task owner"))?;
    let saved = read(store, format!("{RECORD_PREFIX}{owner}"))
        .await?
        .ok_or_else(|| invalid("bound task record is missing"))?;
    let record: TaskRecord = serde_json::from_str(&saved)?;
    let grant = record.workspace.directory_grant();
    if record.source.path != source_path
        || grant.path != record.scope.workspace.path
        || grant.identity != record.scope.workspace.identity
    {
        return Err(invalid("bound task does not match the source project"));
    }
    let permit = crate::workspace::blocking_permits()
        .acquire_owned()
        .await
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    let scope = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if NativeWorkspaceIdentity::capture(&record.source.path)? != record.source.identity {
            return Err(io::Error::other("task source directory changed"));
        }
        record
            .scope
            .validate_workspace(&grant.path, NativeWorkspaceIdentity::capture(&grant.path)?)?;
        Ok(record.scope)
    })
    .await
    .map_err(|_| SessionRuntimeError::Unavailable)?
    .map_err(|error: io::Error| invalid(&error.to_string()))?;
    Ok(Some(scope))
}

async fn read(store: &Store, key: String) -> Result<Option<String>, SessionRuntimeError> {
    store.call(Priority::AwaitControl, move |connection| {
        let saved: Option<Option<String>> = connection.query_row(
            "SELECT CASE WHEN length(CAST(value AS BLOB))<=?2 THEN value ELSE NULL END FROM metadata WHERE key=?1",
            params![key, MAX_RECORD_BYTES], |row| row.get(0),
        ).optional()?;
        saved.map(|value| value.ok_or(SessionRuntimeError::CODEC)).transpose()
    }).await
}

async fn save(store: &Store, key: String, value: String) -> Result<(), SessionRuntimeError> {
    store.call_write(Priority::AwaitControl, move |connection| {
        connection.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value])?;
        Ok(())
    }).await
}

#[cfg(test)]
mod tests;
