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

/// Authority supplied by the server, independently of project/model output.
/// `directory` is a private control root outside every project and task grant.
#[derive(Debug, Clone)]
pub struct TaskWorkspacePolicy {
    pub directory: NativeDirectoryGrant,
    pub read_only: Vec<NativeDirectoryGrant>,
    pub resources: NativeResources,
    pub limits: TaskWorkspaceLimits,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum TaskExecutionError {
    #[error("invalid task execution state: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Storage(#[from] crate::storage::Pressure),
    #[error("task workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Workspace(#[from] TaskWorkspaceError),
    #[error("task workspace traversal failed: {0}")]
    Traverse(#[from] ignore::Error),
    #[error("task record could not be encoded or decoded: {0}")]
    Codec(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] SessionRuntimeError),
    #[error("task source configuration failed: {0}")]
    Runtime(#[from] RuntimeLoadError),
    #[error("task workspace preparation stopped: {0}")]
    Join(#[from] tokio::task::JoinError),
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
    ) -> Result<(), TaskExecutionError> {
        if !self.publisher {
            return Ok(());
        }
        // Configuration is trusted source authority. Reload it after payloads
        // drain so newly protected pack files cannot be installed as output.
        let loaded = inner.loader.load(self.source_request.clone()).await?;
        let policy = inner.store.tool_approval_policy(session).await?;
        if !matches!(policy.mode, ApprovalMode::Auto | ApprovalMode::Full)
            || policy
                .ancestors
                .iter()
                .any(|(mode, _)| !matches!(mode, ApprovalMode::Auto | ApprovalMode::Full))
        {
            return Err(TaskExecutionError::Invalid(
                "source publication permission changed",
            ));
        }
        let task = self.clone();
        let permit = crate::workspace::blocking_permits()
            .acquire_owned()
            .await
            .map_err(|_| TaskExecutionError::Invalid("filesystem workers unavailable"))?;
        let encoded = tokio::task::spawn_blocking(move || {
            let _permit = permit;
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
            outputs.extend(files(&root, record.workspace_limits(), false)?);
            if outputs.len() > record.workspace_limits().max_entries {
                return Err(TaskExecutionError::Invalid("too many task outputs"));
            }
            record
                .workspace
                .publish(&outputs.into_iter().collect::<Vec<_>>())?;
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

impl TaskRecord {
    fn workspace_limits(&self) -> TaskWorkspaceLimits {
        self.workspace.limits()
    }
}

pub(super) async fn prepare(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    source_plan: Arc<crate::plan::CompiledAgentPlan>,
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
    let source_request = RuntimeLoadRequest {
        execution_workspace: None,
        workspace: claimed.workspace.clone(),
        model: claimed.model.clone(),
        profile: claimed.profile.clone(),
        reasoning_effort: claimed.reasoning_effort,
    };
    let permit = crate::workspace::blocking_permits()
        .acquire_owned()
        .await
        .map_err(|_| TaskExecutionError::Invalid("filesystem workers unavailable"))?;
    let prepared = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let source = source_plan.workspace_handle();
        let source_grant = NativeDirectoryGrant {
            path: source.path().to_owned(),
            identity: NativeWorkspaceIdentity::held(source.root())?,
        };
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
                let inputs = files(&source, policy.limits, true)?;
                let workspace = TaskWorkspace::import(
                    &TaskWorkspaceSource::from_plan(&source_plan)?,
                    &policy.directory.path.join(owner.to_string()),
                    &inputs,
                    policy.limits,
                )?;
                let scope = NativeCommandScope {
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
        if record.source.path != source_grant.path
            || record.source.identity != source_grant.identity
            || record.workspace.path() != record.scope.workspace.path
            || record.workspace.directory_grant().identity != record.scope.workspace.identity
        {
            return Err(TaskExecutionError::Invalid(
                "task record does not match this project",
            ));
        }
        // A persisted tree retains work, not a permanent grant. Reapply the
        // trusted host's current limits and readable roots on each admission.
        record.scope.read_only = policy.read_only;
        record.scope.network = NativeNetwork::Isolated;
        record.scope.resources = policy.resources;
        record
            .workspace
            .protect_source(&TaskWorkspaceSource::from_plan(&source_plan)?)?;
        let held = Workspace::open(record.workspace.path())?;
        record
            .scope
            .validate_workspace(held.path(), NativeWorkspaceIdentity::held(held.root())?)?;
        crate::storage::check_blocking(&held, 0)?;
        for protected in source.protected_pack_roots() {
            if !protected.as_os_str().is_empty() {
                held.root().create_dir_all(protected)?;
            }
        }
        held.root().create_dir_all(format!("{PRIVATE_HOME}/tmp"))?;
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
    if let Some(task) = prepare(inner, claimed, loaded.plan).await? {
        resources.bind_task(task)?;
    }
    Ok(())
}

fn files(
    workspace: &Workspace,
    limits: TaskWorkspaceLimits,
    inputs: bool,
) -> Result<Vec<PathBuf>, TaskExecutionError> {
    let root = workspace.path().to_owned();
    let guard = workspace.clone();
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let Ok(relative) = entry.path().strip_prefix(guard.path()) else {
                return false;
            };
            if relative.as_os_str().is_empty() || relative == Path::new(".qq") {
                return true;
            }
            if relative.starts_with(PRIVATE_HOME) {
                return false;
            }
            !inputs
                || guard
                    .check_write_path(&relative.to_string_lossy(), relative)
                    .is_ok()
        });
    let mut selected = Vec::new();
    for (index, entry) in builder.build().enumerate() {
        if index > limits.max_entries {
            return Err(TaskExecutionError::Invalid(
                "task file selection exceeds its entry limit",
            ));
        }
        let entry = entry?;
        let Some(kind) = entry.file_type() else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(&root)
            .map_err(|_| TaskExecutionError::Invalid("task traversal escaped its root"))?;
        if kind.is_file() {
            selected.push(relative.to_owned());
        } else if !inputs && !kind.is_dir() {
            return Err(TaskWorkspaceError::UnsupportedFile(relative.to_owned()).into());
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
        let Some(owner) = read(&inner.store, format!("{RUN_PREFIX}{run}")).await? else {
            continue;
        };
        let owner: RunId = owner.parse().map_err(|_| invalid("invalid task owner"))?;
        let saved = read(&inner.store, format!("{RECORD_PREFIX}{owner}"))
            .await?
            .ok_or_else(|| invalid("active task record is missing"))?;
        let record: TaskRecord = serde_json::from_str(&saved)?;
        let grant = record.workspace.directory_grant();
        if record.source.path != Path::new(source_path)
            || grant.path != record.scope.workspace.path
            || grant.identity != record.scope.workspace.identity
        {
            return Err(invalid("active task does not match the source project"));
        }
        let permit = crate::workspace::blocking_permits()
            .acquire_owned()
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let identity = NativeWorkspaceIdentity::capture(&record.source.path)?;
            if identity != record.source.identity {
                return Err(io::Error::other("task source directory changed"));
            }
            Ok(())
        })
        .await
        .map_err(|_| SessionRuntimeError::Unavailable)?
        .map_err(|error: io::Error| invalid(&error.to_string()))?;
        return Ok(Some(record.scope));
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
