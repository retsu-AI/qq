//! Root-task Git actions. Model input never selects host programs or paths.
use super::{
    claim::{ClaimedRun, RunIdentity, RunKind},
    runtime::SessionRuntimeInner,
    store::Priority,
    *,
};
use crate::{
    command_owner::{
        NativeCommand, NativeEnvironment, NativeFilesystem, NativeNetwork, NativeTaskGitCall,
    },
    tools::{ToolOutput, ToolTasks, task_git::TaskGitRequest},
};

/// Server-selected programs; neither project configuration nor a tool argument
/// may replace these. The Git image is admitted again against the task roots.
#[derive(Debug, Clone)]
pub struct TaskGitPolicy {
    pub helper: crate::task_git_job::TaskGitHelperImage,
    pub git: PathBuf,
}

impl TaskGitPolicy {
    /// Call on a blocking worker during server composition.
    pub fn from_process_env(
        control: crate::command_owner::NativeDirectoryGrant,
    ) -> Result<Option<Self>, crate::task_git_job::HelperImageError> {
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        let Some(path) = std::env::var_os("PATH") else {
            return Ok(None);
        };
        let Some(git) = std::env::split_paths(&path)
            .filter(|path| path.is_absolute())
            .map(|path| path.join(name))
            .find_map(|path| path.is_file().then(|| path.canonicalize().ok()).flatten())
        else {
            return Ok(None);
        };
        let helper = crate::task_git_job::TaskGitHelperImage::capture(
            &std::env::current_exe()?.canonicalize()?,
            control,
            std::time::Instant::now() + Duration::from_secs(30),
        )?;
        Ok(Some(Self { helper, git }))
    }
}

pub(crate) struct TaskGitRunInput {
    inner: Arc<SessionRuntimeInner>,
    identity: RunIdentity,
    task: Arc<super::task_workspaces::TaskExecution>,
    tools: ToolTasks,
}

impl TaskGitRunInput {
    /// Compaction declares the same schemas as the prompt whose prefix it
    /// reuses, although its own tool gate never permits execution.
    pub(super) fn offered(inner: &SessionRuntimeInner, claimed: &ClaimedRun) -> bool {
        claimed.depth == 0
            && claimed.purpose == SessionPurpose::Task
            && claimed.approval_mode == ApprovalMode::Auto
            && inner.task_git.is_some()
            && inner.task_workspaces.is_some()
            && inner.native_commands.is_some()
    }

    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: &ClaimedRun,
        task: Arc<super::task_workspaces::TaskExecution>,
        tools: ToolTasks,
    ) -> Option<Self> {
        (Self::offered(&inner, claimed) && claimed.identity.kind == RunKind::Prompt).then_some(
            Self {
                inner,
                identity: claimed.identity,
                task,
                tools,
            },
        )
    }

    pub(crate) async fn execute(
        &self,
        call: &crate::RuntimeToolCall,
        cancellation: &RunCancellation,
        deadline: Option<tokio::time::Instant>,
    ) -> ToolOutput {
        let request = match crate::tools::task_git::parse(&call.name, &call.arguments) {
            Ok(request) => request,
            Err(error) => return ToolOutput::refused(error.to_string()),
        };
        let message = match request {
            TaskGitRequest::Commit { message } => message,
            TaskGitRequest::Push | TaskGitRequest::PullRequest { .. } => {
                return ToolOutput::refused(
                    "No authorized hosting destination is installed for this task",
                );
            }
        };
        if cancellation.is_cancelled() {
            return ToolOutput::interrupted("task Git action cancelled before admission");
        }
        let duration = deadline.map_or(Duration::from_secs(120), |deadline| {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_secs(120))
        });
        let not_after_ms =
            now_ms().saturating_add(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        let admission_deadline = tokio::time::Instant::now() + duration;
        let Some(policy) = self.inner.task_git.as_ref() else {
            return ToolOutput::refused("task Git execution is unavailable");
        };
        if let Err(error) = self.admit(call).await {
            return ToolOutput::refused(error.to_string());
        }
        let job = match self
            .task
            .clone()
            .git_job(
                &self.inner,
                call.id,
                policy.git.clone(),
                message,
                not_after_ms,
                cancellation.clone(),
            )
            .await
        {
            Ok(job) => job,
            Err(error) => return ToolOutput::refused(error.to_string()),
        };
        let control = job.control.clone();
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return ToolOutput::interrupted("task Git action cancelled before admission"),
            () = tokio::time::sleep_until(admission_deadline) => return ToolOutput::interrupted("task Git action expired before admission"),
            permit = crate::workspace::blocking_permits().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return ToolOutput::refused("filesystem workers unavailable"),
            },
        };
        let helper_image = policy.helper.clone();
        let helper_deadline = std::time::Instant::now()
            + admission_deadline.saturating_duration_since(tokio::time::Instant::now());
        let mut saved = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let helper = helper_image
                .validate(helper_deadline)
                .map_err(|error| error.to_string())?;
            let files = crate::task_git_job::save_job(&job).map_err(|error| error.to_string())?;
            Ok::<_, String>((files, helper))
        });
        let (files, helper) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                saved.abort();
                return ToolOutput::interrupted("task Git action cancelled before admission");
            }
            () = tokio::time::sleep_until(admission_deadline) => {
                // Only private intent metadata may still complete. No worker
                // submission or user-file effect has been admitted.
                saved.abort();
                return ToolOutput::interrupted("task Git action expired before admission");
            }
            saved = &mut saved => match saved {
                Ok(Ok(files)) => files,
                Ok(Err(error)) => return ToolOutput::refused(error.to_string()),
                Err(error) => return ToolOutput::refused(error.to_string()),
            },
        };
        if let Err(error) = self.admit(call).await {
            return ToolOutput::refused(error.to_string());
        }
        let environment = match NativeEnvironment::capture(&[]) {
            Ok(environment) => environment,
            Err(error) => return ToolOutput::refused(error.to_string()),
        };
        let arguments = serde_json::json!({
            "program":helper,
            "args":["_task-git",files.request],
        })
        .to_string();
        let command = NativeCommand {
            id: call.id,
            environment,
            filesystem: NativeFilesystem::Host,
            network: NativeNetwork::Host,
            // This fixed, pinned QQ operation runs as trusted coordinator code,
            // rather than executing task code. Its parser bounds input/pack size
            // and the durable worker owns its deadline and process cleanup.
            // Task resource policies require a restricted payload filesystem.
            resources: crate::command_owner::NativeResources::Host,
            workspace: control.path,
            workspace_identity: control.identity,
            tool: "exec".to_owned(),
            arguments,
            task_git_call: Some(NativeTaskGitCall {
                job: None,
                tool: call.name.clone(),
                arguments: call.arguments.clone(),
            }),
            shell_policy: crate::ShellPolicy::default(),
            approved_images: Vec::new(),
            not_after_ms: Some(not_after_ms),
        };
        let Some(service) = self.inner.native_commands.clone() else {
            return ToolOutput::refused("durable command worker unavailable");
        };
        let mut lease = self.tools.enter();
        // Preserve the worker's exact result, including typed helper stdout,
        // for identical normal completion and coordinator-restart recovery.
        crate::command_owner::observe(
            service,
            command,
            &crate::tools::ToolCancellation::new(cancellation.clone()),
            None,
            &mut lease,
        )
        .await
    }

    async fn admit(&self, call: &crate::RuntimeToolCall) -> Result<(), SessionRuntimeError> {
        let identity = self.identity;
        let id = call.id;
        let name = call.name.clone();
        let arguments = call.arguments.clone();
        self.inner.store.call_write(Priority::AwaitControl, move |connection| {
            let transaction = super::store::begin_unit(connection)?;
            let valid: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs r JOIN tool_calls t ON t.run_id=r.id
                 WHERE r.id=?1 AND r.session_id=?2 AND r.status='running' AND r.outcome_json IS NULL
                 AND r.cancel_requested=0 AND t.id=?3 AND t.state='running' AND t.name=?4 AND t.arguments_json=?5)",
                params![identity.run_id.to_string(),identity.session_id.to_string(),id.to_string(),name,arguments],
                |row| row.get(0),
            )?;
            let policy = super::tool_calls::load_tool_approval_policy(&transaction, identity.session_id)?;
            if !valid || !matches!(policy.mode, ApprovalMode::Auto | ApprovalMode::Full)
                || policy.ancestors.iter().any(|(mode,_)| !matches!(mode, ApprovalMode::Auto | ApprovalMode::Full))
                || super::native_recovery::workspace_blocked(&transaction, identity.workspace_id)?
            {
                return Err(SessionRuntimeError::InvalidNativeScope { reason: "task Git authority changed or prior execution is unresolved".into() });
            }
            let children: bool = transaction.query_row(
                "WITH RECURSIVE children(id) AS (
                    SELECT id FROM sessions WHERE parent_id=?1
                    UNION ALL SELECT s.id FROM sessions s JOIN children c ON s.parent_id=c.id
                 ) SELECT EXISTS(SELECT 1 FROM runs r JOIN children c ON c.id=r.session_id
                    WHERE r.outcome_json IS NULL AND r.status IN ('queued','running'))",
                [identity.session_id.to_string()], |row| row.get(0),
            )?;
            if children {
                return Err(SessionRuntimeError::InvalidNativeScope { reason: "wait for task subagents before committing their shared files".into() });
            }
            transaction.commit()?;
            Ok(())
        }).await
    }
}
