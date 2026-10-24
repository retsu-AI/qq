//! Private task Git jobs launched by the durable command worker.
//! These records are trusted host input, never authority supplied by a model.
pub use crate::task_git::hosting::{
    TaskGitHostingError, TaskGitHostingTarget, TaskGitPullRequest, TaskGitPullRequestState,
    TaskGitPush,
};
pub use crate::task_git::{TaskGitCommit, TaskGitError, TaskGitLimits};
pub const HOSTING_TOKEN_ENV: &str = "QQ_TASK_GITHUB_TOKEN";
pub use crate::task_git_helper_image::{HelperImageError, TaskGitHelperImage};
use crate::{
    RunCancellation,
    command_owner::NativeDirectoryGrant,
    task_git::{self, Operation, TaskGit, TaskGitPrepared, TaskGitRoots},
};
use qq_protocol::{RunId, ToolCallId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// Same private-record ceiling as sessions/task_workspaces.rs; the worker IPC
// contains only the request pathname, never this potentially large manifest.
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitJob {
    pub call: ToolCallId,
    pub task: RunId,
    pub git_program: PathBuf,
    /// Git HEAD captured before task import/model execution; never recaptured.
    pub base: String,
    pub source: NativeDirectoryGrant,
    pub control: NativeDirectoryGrant,
    pub snapshot: NativeDirectoryGrant,
    pub limits: TaskGitLimits,
    /// All current task changes versus immutable imported bytes.
    pub paths: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
    pub operation: TaskGitJobOperation,
    pub not_after_ms: u64,
}

/// Only title/body/message are model input. Remote destinations are supplied
/// by current host configuration and bound into the private job identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskGitJobOperation {
    Commit {
        message: String,
    },
    Push {
        target: TaskGitHostingTarget,
    },
    PullRequest {
        target: TaskGitHostingTarget,
        title: String,
        body: String,
    },
}

impl TaskGitJobOperation {
    fn matches_call(&self, tool: &str, arguments: &str) -> bool {
        use crate::tools::task_git::{TaskGitRequest, parse};
        match (self, parse(tool, arguments)) {
            (Self::Commit { message }, Ok(TaskGitRequest::Commit { message: expected })) => {
                message == &expected
            }
            (Self::Push { .. }, Ok(TaskGitRequest::Push)) => true,
            (
                Self::PullRequest { title, body, .. },
                Ok(TaskGitRequest::PullRequest {
                    title: expected_title,
                    body: expected_body,
                }),
            ) => title == &expected_title && body == &expected_body,
            _ => false,
        }
    }

    fn logical_call(&self) -> (&'static str, serde_json::Value) {
        match self {
            Self::Commit { message } => ("task_commit", serde_json::json!({"message":message})),
            Self::Push { .. } => ("task_push", serde_json::json!({})),
            Self::PullRequest { title, body, .. } => {
                ("task_pr", serde_json::json!({"title":title,"body":body}))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskGitJobFiles {
    pub request: PathBuf,
    pub receipt: PathBuf,
    /// Present only after the original private intent has been durably saved.
    pub reference: Option<TaskGitJobReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitJobReference {
    call: ToolCallId,
    task: RunId,
    control: NativeDirectoryGrant,
    request_digest: [u8; 32],
}

impl PartialEq for TaskGitJobReference {
    fn eq(&self, other: &Self) -> bool {
        self.call == other.call
            && self.task == other.task
            && self.control.path == other.control.path
            && self.control.identity == other.control.identity
            && self.request_digest == other.request_digest
    }
}
impl Eq for TaskGitJobReference {}

impl TaskGitJobReference {
    fn request(&self) -> PathBuf {
        self.control
            .path
            .join(format!("task-git-job-{}.request.json", self.call))
    }

    pub(crate) fn matches_launch(
        &self,
        id: ToolCallId,
        workspace: &Path,
        workspace_identity: crate::command_owner::NativeWorkspaceIdentity,
        tool: &str,
        arguments: &str,
        logical_tool: &str,
    ) -> bool {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CommitLaunch {
            program: PathBuf,
            args: [String; 2],
            #[serde(default)]
            env: Vec<String>,
        }
        let Ok(args) = serde_json::from_str::<CommitLaunch>(arguments) else {
            return false;
        };
        id == self.call
            && tool == "exec"
            && workspace == self.control.path
            && workspace_identity == self.control.identity
            && args.program.is_absolute()
            && args.args[0] == "_task-git"
            && Path::new(&args.args[1]) == self.request()
            && if logical_tool == "task_commit" {
                args.env.is_empty()
            } else {
                matches!(logical_tool, "task_push" | "task_pr") && args.env == [HOSTING_TOKEN_ENV]
            }
    }

    /// Historical observation only: no Git command, new deadline or ref update.
    pub(crate) fn successful_receipt(
        &self,
        tool: &str,
        arguments: &str,
    ) -> Result<Option<TaskGitJobReceipt>, TaskGitJobError> {
        let root = task_git::held(&self.control)?;
        let request = self.request();
        let Some(job) = read_record::<TaskGitJob>(
            &root,
            Path::new(
                request
                    .file_name()
                    .ok_or(TaskGitJobError::Invalid("request name"))?,
            ),
        )?
        else {
            return Ok(None);
        };
        if job.call != self.call
            || job.task != self.task
            || (job.control.path != self.control.path
                || job.control.identity != self.control.identity)
            || job.digest()? != self.request_digest
            || !job.operation.matches_call(tool, arguments)
        {
            return Err(TaskGitJobError::Invalid("job reference mismatch"));
        }
        let receipt = read_receipt(&job)?;
        Ok(receipt.filter(|receipt| receipt.result.is_success()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskGitJobResult {
    Committed { commit: TaskGitCommit },
    Pushed { push: TaskGitPush },
    PullRequest { pull_request: TaskGitPullRequest },
    Refused { reason: String },
    Stopped,
    CleanupUnconfirmed,
}

impl TaskGitJobResult {
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(
            self,
            Self::Committed { .. } | Self::Pushed { .. } | Self::PullRequest { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskGitJobReceipt {
    pub call: ToolCallId,
    pub task: RunId,
    pub request_digest: [u8; 32],
    pub result: TaskGitJobResult,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskGitJobError {
    #[error(transparent)]
    Git(#[from] TaskGitError),
    #[error("task Git job I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("task Git job metadata failed: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("invalid task Git job: {0}")]
    Invalid(&'static str),
}

impl TaskGitJob {
    pub fn files(&self) -> TaskGitJobFiles {
        let stem = format!("task-git-job-{}", self.call);
        TaskGitJobFiles {
            request: self.control.path.join(format!("{stem}.request.json")),
            receipt: self.control.path.join(format!("{stem}.receipt.json")),
            reference: None,
        }
    }

    fn digest(&self) -> Result<[u8; 32], TaskGitJobError> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(TaskGitJobError::Invalid("oversized job"));
        }
        Ok(Sha256::digest(bytes).into())
    }

    fn roots(&self) -> TaskGitRoots {
        TaskGitRoots {
            source: self.source.clone(),
            control: self.control.clone(),
            task: self.snapshot.clone(),
        }
    }

    fn validate_result(&self, result: &TaskGitJobResult) -> Result<(), TaskGitJobError> {
        let branch = format!("refs/heads/qq/task/{}", self.task);
        let invalid = || TaskGitJobError::Invalid("receipt operation or destination mismatch");
        match (&self.operation, result) {
            (TaskGitJobOperation::Commit { .. }, TaskGitJobResult::Committed { commit }) => {
                if commit.branch != branch || commit.base != self.base {
                    return Err(invalid());
                }
                task_git::oid(commit.oid.clone())?;
                task_git::oid(commit.tree.clone())?;
            }
            (TaskGitJobOperation::Push { .. }, TaskGitJobResult::Pushed { push }) => {
                if push.branch != branch || push.repository_id == 0 {
                    return Err(invalid());
                }
                task_git::oid(push.oid.clone())?;
            }
            (
                TaskGitJobOperation::PullRequest { target, .. },
                TaskGitJobResult::PullRequest { pull_request },
            ) => {
                if pull_request.repository_id == 0
                    || pull_request.number == 0
                    || pull_request.head
                        != branch.strip_prefix("refs/heads/").ok_or_else(invalid)?
                    || pull_request.base != target.base_branch()
                    || pull_request.url
                        != format!(
                            "https://github.com/{}/{}/pull/{}",
                            target.owner(),
                            target.repository(),
                            pull_request.number
                        )
                {
                    return Err(invalid());
                }
                task_git::oid(pull_request.oid.clone())?;
            }
            (
                _,
                TaskGitJobResult::Refused { .. }
                | TaskGitJobResult::Stopped
                | TaskGitJobResult::CleanupUnconfirmed,
            ) => {}
            _ => return Err(invalid()),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), TaskGitJobError> {
        self.digest()?;
        task_git::oid(self.base.clone())?;
        if self.paths.len() > self.limits.max_entries
            || self.protected_paths.len() > self.limits.max_entries
        {
            return Err(TaskGitJobError::Invalid("invalid commit bounds"));
        }
        let (tool, arguments) = self.operation.logical_call();
        if !self.operation.matches_call(tool, &arguments.to_string()) {
            return Err(TaskGitJobError::Invalid("invalid task Git operation"));
        }
        match &self.operation {
            TaskGitJobOperation::Commit { .. } => {}
            TaskGitJobOperation::Push { target }
            | TaskGitJobOperation::PullRequest { target, .. } => {
                TaskGitHostingTarget::new(
                    target.owner().to_owned(),
                    target.repository().to_owned(),
                    target.base_branch().to_owned(),
                )
                .map_err(|_| TaskGitJobError::Invalid("invalid hosting target"))?;
                if !self.paths.is_empty() {
                    return Err(TaskGitJobError::Invalid("remote jobs cannot select files"));
                }
            }
        }
        for path in self.paths.iter().chain(&self.protected_paths) {
            task_git::validate_path(path)?;
        }
        if self.paths.iter().any(|path| {
            self.protected_paths
                .iter()
                .any(|protected| path.starts_with(protected) || protected.starts_with(path))
        }) {
            return Err(TaskGitJobError::Invalid("protected commit path"));
        }
        Ok(())
    }
}

/// Call on the host's bounded blocking executor before worker submission.
/// The private control root must not be writable by the task payload.
pub fn save_job(job: &TaskGitJob) -> Result<TaskGitJobFiles, TaskGitJobError> {
    job.validate()?;
    let root = task_git::held(&job.control)?;
    let _lock = lock_job(&root, job.call)?;
    let mut files = job.files();
    let name = files
        .request
        .file_name()
        .ok_or(TaskGitJobError::Invalid("request name"))?;
    if let Some(existing) = read_record::<TaskGitJob>(&root, Path::new(name))? {
        if existing.digest()? != job.digest()? {
            return Err(TaskGitJobError::Invalid(
                "call ID already binds another request",
            ));
        }
    } else {
        task_git::save_metadata(
            &root,
            name.to_str()
                .ok_or(TaskGitJobError::Invalid("request encoding"))?,
            job,
        )?;
    }
    files.reference = Some(TaskGitJobReference {
        call: job.call,
        task: job.task,
        control: job.control.clone(),
        request_digest: job.digest()?,
    });
    Ok(files)
}

/// Read a historical receipt without launching or renewing an effect.
pub fn read_receipt(job: &TaskGitJob) -> Result<Option<TaskGitJobReceipt>, TaskGitJobError> {
    let root = task_git::held(&job.control)?;
    let files = job.files();
    let receipt: Option<TaskGitJobReceipt> = read_record(
        &root,
        Path::new(
            files
                .receipt
                .file_name()
                .ok_or(TaskGitJobError::Invalid("receipt name"))?,
        ),
    )?;
    if let Some(receipt) = &receipt
        && (receipt.call != job.call
            || receipt.task != job.task
            || receipt.request_digest != job.digest()?)
    {
        return Err(TaskGitJobError::Invalid(
            "receipt belongs to another request",
        ));
    }
    if let Some(receipt) = &receipt {
        job.validate_result(&receipt.result)?;
    }
    Ok(receipt)
}

/// Single-shot hidden CLI entry. The worker owns cancellation and descendant custody.
/// Successful return means a durable typed receipt exists, including refusals.
pub fn execute_job(request: &Path) -> Result<TaskGitJobReceipt, TaskGitJobError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let now_ms = u64::try_from(now.as_millis()).map_err(io::Error::other)?;
    execute_job_at_with_token(request, now_ms, None)
}

/// The caller supplies this credential in memory only, after trusted host policy
/// resolution. Neither the private request nor a saved result contains it.
pub fn execute_job_with_token(
    request: &Path,
    credential: Option<qq_provider::SecretLiteral>,
) -> Result<TaskGitJobReceipt, TaskGitJobError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let now_ms = u64::try_from(now.as_millis()).map_err(io::Error::other)?;
    execute_job_at_with_token(request, now_ms, credential)
}

#[cfg(test)]
fn execute_job_at(request: &Path, now_ms: u64) -> Result<TaskGitJobReceipt, TaskGitJobError> {
    execute_job_at_with_token(request, now_ms, None)
}

fn execute_job_at_with_token(
    request: &Path,
    now_ms: u64,
    credential: Option<qq_provider::SecretLiteral>,
) -> Result<TaskGitJobReceipt, TaskGitJobError> {
    let parent = request
        .parent()
        .ok_or(TaskGitJobError::Invalid("request parent"))?;
    let root = crate::workspace::Workspace::open(parent).map_err(TaskGitError::from)?;
    let job: TaskGitJob = read_record(
        &root,
        Path::new(
            request
                .file_name()
                .ok_or(TaskGitJobError::Invalid("request name"))?,
        ),
    )?
    .ok_or(TaskGitJobError::Invalid("missing request"))?;
    job.validate()?;
    if job.files().request != request {
        return Err(TaskGitJobError::Invalid(
            "request is outside its private control root",
        ));
    }
    let _lock = lock_job(&task_git::held(&job.control)?, job.call)?;
    if let Some(receipt) = read_receipt(&job)? {
        return Ok(receipt);
    }
    let restored = TaskGit::restore_job(
        &job.git_program,
        &job.roots(),
        job.task,
        job.base.clone(),
        job.limits,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let result = match &job.operation {
        TaskGitJobOperation::Commit { message } => {
            match runtime.block_on(commit(&job, message.clone(), now_ms, restored)) {
                Ok(commit) => TaskGitJobResult::Committed { commit },
                Err(TaskGitError::Stopped) => TaskGitJobResult::Stopped,
                Err(TaskGitError::CleanupUnconfirmed) => TaskGitJobResult::CleanupUnconfirmed,
                Err(error) => TaskGitJobResult::Refused {
                    reason: error.to_string(),
                },
            }
        }
        operation => match (restored, credential) {
            (Ok(Some(mut git)), Some(credential)) => {
                git.inherit_worker_custody();
                let deadline = Instant::now()
                    .checked_add(Duration::from_millis(
                        job.not_after_ms.saturating_sub(now_ms),
                    ))
                    .ok_or(TaskGitJobError::Invalid("invalid job deadline"))?;
                let bounds = Operation {
                    cancellation: RunCancellation::new(),
                    deadline,
                    worker_owned: true,
                };
                match operation {
                    TaskGitJobOperation::Push { target } => runtime
                        .block_on(git.push(target.clone(), credential, job.call, bounds))
                        .map(|push| TaskGitJobResult::Pushed { push }),
                    TaskGitJobOperation::PullRequest {
                        target,
                        title,
                        body,
                    } => runtime
                        .block_on(git.pull_request(
                            target.clone(),
                            credential,
                            job.call,
                            title.clone(),
                            body.clone(),
                            bounds,
                        ))
                        .map(|pull_request| TaskGitJobResult::PullRequest { pull_request }),
                    TaskGitJobOperation::Commit { .. } => unreachable!("commit handled above"),
                }
                .unwrap_or_else(|error| TaskGitJobResult::Refused {
                    reason: error.to_string(),
                })
            }
            (Err(error), _) => TaskGitJobResult::Refused {
                reason: error.to_string(),
            },
            (_, None) => TaskGitJobResult::Refused {
                reason: "host credential unavailable".into(),
            },
            (Ok(None), _) => TaskGitJobResult::Refused {
                reason: "commit task changes before publishing".into(),
            },
        },
    };
    job.validate_result(&result)?;
    let receipt = TaskGitJobReceipt {
        call: job.call,
        task: job.task,
        request_digest: job.digest()?,
        result,
    };
    let files = job.files();
    task_git::save_metadata(
        &task_git::held(&job.control)?,
        files
            .receipt
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(TaskGitJobError::Invalid("receipt encoding"))?,
        &receipt,
    )?;
    Ok(receipt)
}

async fn commit(
    job: &TaskGitJob,
    message: String,
    now_ms: u64,
    restored: Result<Option<TaskGit>, TaskGitError>,
) -> Result<TaskGitCommit, TaskGitError> {
    let operation = Operation {
        cancellation: RunCancellation::new(),
        deadline: Instant::now()
            .checked_add(Duration::from_millis(
                job.not_after_ms.saturating_sub(now_ms),
            ))
            .ok_or(TaskGitError::Invalid("invalid job deadline"))?,
        worker_owned: true,
    };
    let mut git = match restored? {
        Some(git) => git,
        None => {
            TaskGit::prepare(
                job.git_program.clone(),
                job.roots(),
                job.task,
                job.base.clone(),
                job.limits,
                operation.clone(),
            )
            .await?
        }
    };
    git.inherit_worker_custody();
    match git
        .freeze(
            job.snapshot.clone(),
            job.paths.clone(),
            message,
            job.call,
            operation.clone(),
        )
        .await?
    {
        TaskGitPrepared::Complete(commit) => Ok(commit),
        TaskGitPrepared::Pending(expected) => {
            let actual = git.install(job.call, operation).await?;
            if actual != expected {
                return Err(TaskGitError::Invalid(
                    "installed commit differs from frozen receipt",
                ));
            }
            Ok(actual)
        }
    }
}

fn read_record<T: serde::de::DeserializeOwned>(
    root: &crate::workspace::Workspace,
    name: &Path,
) -> Result<Option<T>, TaskGitJobError> {
    let metadata = match root.root().symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    task_git::check_file(&metadata)?;
    if metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(TaskGitJobError::Invalid("oversized record"));
    }
    // Check the opened file identity as well as the no-follow metadata above.
    task_git::read_file(root, name, MAX_RECORD_BYTES)?
        .map(|(bytes, _)| serde_json::from_slice(&bytes).map_err(Into::into))
        .transpose()
}

fn lock_job(
    root: &crate::workspace::Workspace,
    call: ToolCallId,
) -> Result<std::fs::File, TaskGitJobError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(false)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    let file = root
        .root()
        .open_with(format!("task-git-job-{call}.lock"), &options)?;
    task_git::check_file(&file.metadata()?)?;
    let file = file.into_std();
    file.try_lock().map_err(|_| TaskGitError::Busy)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    pub(super) fn fixture() -> (TempDir, TaskGitJob) {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        let control = root.path().join("control");
        let snapshot = root.path().join("snapshot");
        for path in [&source, &control, &snapshot] {
            std::fs::create_dir(path).unwrap();
        }
        let program = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join(if cfg!(windows) { "git.exe" } else { "git" }))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        let run = |args: &[&str]| {
            let output = Command::new(&program)
                .args(args)
                .current_dir(&source)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", root.path().join("empty"))
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        std::fs::write(root.path().join("empty"), "").unwrap();
        run(&["init", "--template=", "."]);
        std::fs::write(source.join("file"), "base").unwrap();
        run(&["add", "file"]);
        run(&[
            "-c",
            "core.hooksPath=",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "base",
        ]);
        std::fs::write(snapshot.join("file"), "task").unwrap();
        let job = TaskGitJob {
            call: ToolCallId::generate().unwrap(),
            task: RunId::generate().unwrap(),
            base: TaskGit::capture_base(
                &program,
                &NativeDirectoryGrant::capture(&source).unwrap(),
                &NativeDirectoryGrant::capture(&control).unwrap(),
                &Operation {
                    cancellation: RunCancellation::new(),
                    deadline: Instant::now() + Duration::from_secs(30),
                    worker_owned: false,
                },
            )
            .unwrap()
            .unwrap(),
            git_program: program,
            source: NativeDirectoryGrant::capture(&source).unwrap(),
            control: NativeDirectoryGrant::capture(&control).unwrap(),
            snapshot: NativeDirectoryGrant::capture(&snapshot).unwrap(),
            limits: TaskGitLimits {
                max_entries: 100,
                max_bytes: 1024 * 1024,
                max_pack_bytes: 1024 * 1024,
            },
            paths: vec![PathBuf::from("file")],
            protected_paths: vec![PathBuf::from("private")],
            operation: TaskGitJobOperation::Commit {
                message: "task commit".into(),
            },
            not_after_ms: 30_000,
        };
        (root, job)
    }

    #[test]
    fn task_git_job_rejects_success_receipts_for_another_effect() {
        let (_root, job) = fixture();
        let files = save_job(&job).unwrap();
        let receipt = execute_job_at(&files.request, 0).unwrap();
        let TaskGitJobResult::Committed { commit } = &receipt.result else {
            panic!("commit failed")
        };
        let mut foreign = commit.clone();
        foreign.branch = "refs/heads/unrelated".into();
        let mismatches = [
            TaskGitJobResult::Committed { commit: foreign },
            TaskGitJobResult::Pushed {
                push: TaskGitPush {
                    repository_id: 1,
                    oid: commit.oid.clone(),
                    branch: commit.branch.clone(),
                },
            },
        ];
        let root = task_git::held(&job.control).unwrap();
        let name = files.receipt.file_name().unwrap().to_str().unwrap();
        for result in mismatches {
            let mut invalid = receipt.clone();
            invalid.result = result;
            task_git::save_metadata(&root, name, &invalid).unwrap();
            assert!(read_receipt(&job).is_err());
        }
        task_git::save_metadata(&root, name, &receipt).unwrap();
        assert_eq!(read_receipt(&job).unwrap(), Some(receipt));
    }

    #[test]
    fn task_git_job_replays_durable_commit_after_expiry_and_lost_outer_receipt() {
        let (_root, job) = fixture();
        let files = save_job(&job).unwrap();
        let locked = lock_job(&task_git::held(&job.control).unwrap(), job.call).unwrap();
        assert!(matches!(
            save_job(&job),
            Err(TaskGitJobError::Git(TaskGitError::Busy))
        ));
        drop(locked);
        let receipt = execute_job_at(&files.request, 0).unwrap();
        assert!(matches!(receipt.result, TaskGitJobResult::Committed { .. }));
        std::fs::write(job.snapshot.path.join("file"), "changed after commit").unwrap();
        assert_eq!(execute_job_at(&files.request, 40_000).unwrap(), receipt);
        std::fs::remove_file(&files.receipt).unwrap();
        assert_eq!(execute_job_at(&files.request, 40_000).unwrap(), receipt);
        assert_eq!(read_receipt(&job).unwrap(), Some(receipt));
        let mut changed = job.clone();
        changed.operation = TaskGitJobOperation::Commit {
            message: "different call".into(),
        };
        assert!(save_job(&changed).is_err());
        assert!(read_receipt(&changed).is_err());
    }

    #[test]
    fn task_git_job_preserves_large_bounded_manifests_outside_worker_ipc() {
        let (_root, mut job) = fixture();
        job.limits.max_entries = 10_000;
        job.paths = (0..6_000)
            .map(|index| {
                PathBuf::from(format!(
                    "source/subdirectory/component_{index:05}/long_file_name.rs"
                ))
            })
            .collect();
        assert!(serde_json::to_vec(&job).unwrap().len() > 256 * 1024);
        let files = save_job(&job).unwrap();
        let root = task_git::held(&job.control).unwrap();
        let stored: TaskGitJob = read_record(&root, Path::new(files.request.file_name().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(stored.digest().unwrap(), job.digest().unwrap());
        job.operation = TaskGitJobOperation::Commit {
            message: "x".repeat(MAX_RECORD_BYTES + 1),
        };
        assert!(save_job(&job).is_err());
    }

    #[test]
    fn task_git_job_refuses_protected_paths_and_expired_new_effects() {
        let (_root, mut job) = fixture();
        job.paths = vec![PathBuf::from("private/credential")];
        assert!(save_job(&job).is_err());
        job.paths = vec![PathBuf::from("file")];
        let files = save_job(&job).unwrap();
        let receipt = execute_job_at(&files.request, 40_000).unwrap();
        assert_eq!(receipt.result, TaskGitJobResult::Stopped);
        assert!(
            !job.control
                .path
                .join(format!("task-git-{}", job.task))
                .exists()
        );
    }
}

#[cfg(all(test, unix))]
mod reconciliation_tests {
    use super::*;
    use crate::command_owner::{
        CommandOwner, CommandOwnerError, CommandStatus, NativeCommand, NativeEnvironment,
        NativeFilesystem, NativeNetwork, NativeResources, NativeTaskGitCall,
    };

    async fn finished(
        owner: &CommandOwner,
        id: ToolCallId,
    ) -> crate::command_owner::CommandSnapshot {
        let mut version = 0;
        loop {
            let snapshot = owner.wait(id, version).await.unwrap();
            version = snapshot.version;
            if snapshot.status == CommandStatus::Finished {
                return snapshot;
            }
        }
    }

    #[tokio::test]
    async fn task_git_reconciles_persisted_commit_after_helper_loses_stdout() {
        let (_root, job, files, receipt) = tokio::task::spawn_blocking(|| {
            let (root, job) = super::tests::fixture();
            let files = save_job(&job).unwrap();
            // This is the precise state after durable effect/receipt persistence,
            // before the helper has printed stdout. No effect may be repeated.
            let receipt = execute_job_at(&files.request, 0).unwrap();
            (root, job, files, receipt)
        })
        .await
        .unwrap();
        let TaskGitJobResult::Committed { commit } = &receipt.result else {
            panic!("fixture must commit")
        };
        let reference = files.reference.clone().unwrap();
        let failing_helper = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|root| root.join("false"))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        let owner = CommandOwner::open(
            job.control.path.join("worker"),
            "unknown",
            crate::command_owner::tests::launcher(),
        )
        .await
        .unwrap();
        let request = NativeCommand {
            id: job.call,
            workspace: job.control.path.clone(),
            workspace_identity: job.control.identity,
            tool: "exec".into(),
            arguments:
                serde_json::json!({"program":failing_helper,"args":["_task-git",files.request]})
                    .to_string(),
            task_git_call: Some(NativeTaskGitCall {
                tool: "task_commit".into(),
                arguments: job.operation.logical_call().1.to_string(),
                job: Some(reference.clone()),
            }),
            environment: NativeEnvironment::capture(&[]).unwrap(),
            filesystem: NativeFilesystem::Host,
            network: NativeNetwork::Host,
            resources: NativeResources::Host,
            shell_policy: crate::ShellPolicy::default(),
            approved_images: Vec::new(),
            not_after_ms: None,
        };
        owner.submit(request.clone()).await.unwrap();
        let snapshot = finished(&owner, job.call).await;
        let output = snapshot.result.unwrap().into_output();
        assert!(!output.is_error, "{}", output.model_text);
        assert_eq!(
            serde_json::from_str::<TaskGitJobReceipt>(&output.model_text).unwrap(),
            receipt
        );
        assert_eq!(
            output.native_outcome,
            Some(crate::command_owner::NativeCommandOutcome::Exited { code: Some(1) })
        );
        assert!(output.execution_context.as_ref().unwrap().bound_to(
            job.call,
            "task_commit",
            &request.task_git_call.as_ref().unwrap().arguments
        ));
        assert_eq!(
            snapshot.process.unwrap().into_context(),
            output.execution_context.unwrap()
        );
        // Remove the receipt: a failed helper must remain failed, and must not
        // regenerate it by replaying Git or guessing from arbitrary stdout.
        std::fs::remove_file(&files.receipt).unwrap();
        let mut absent = request.clone();
        let mut next_job = job.clone();
        next_job.call = ToolCallId::generate().unwrap();
        let next = save_job(&next_job).unwrap();
        absent.id = next_job.call;
        absent.arguments =
            serde_json::json!({"program":failing_helper,"args":["_task-git",next.request]})
                .to_string();
        absent.task_git_call.as_mut().unwrap().job = next.reference;
        owner.submit(absent.clone()).await.unwrap();
        assert!(
            finished(&owner, absent.id)
                .await
                .result
                .unwrap()
                .into_output()
                .is_error
        );
        let mut mismatched_job = job.clone();
        mismatched_job.call = ToolCallId::generate().unwrap();
        let mismatched_files = save_job(&mismatched_job).unwrap();
        let mut mismatched_receipt = receipt.clone();
        mismatched_receipt.call = mismatched_job.call;
        mismatched_receipt.request_digest = [7; 32];
        task_git::save_metadata(
            &task_git::held(&job.control).unwrap(),
            mismatched_files
                .receipt
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            &mismatched_receipt,
        )
        .unwrap();
        let mut mismatch = request.clone();
        mismatch.id = mismatched_job.call;
        mismatch.arguments = serde_json::json!({"program":failing_helper,"args":["_task-git",mismatched_files.request]}).to_string();
        mismatch.task_git_call.as_mut().unwrap().job = mismatched_files.reference;
        owner.submit(mismatch.clone()).await.unwrap();
        assert!(
            finished(&owner, mismatch.id)
                .await
                .result
                .unwrap()
                .into_output()
                .is_error
        );
        // A reference must be bound to both the original private request and
        // native launch, even when the logical message otherwise matches.
        let mut invalid = request;
        invalid.id = ToolCallId::generate().unwrap();
        assert!(matches!(
            owner.submit(invalid).await,
            Err(CommandOwnerError::Invalid)
        ));
        owner.shutdown().await.unwrap();
        assert_eq!(
            reference
                .successful_receipt("task_commit", &job.operation.logical_call().1.to_string())
                .unwrap(),
            None
        );
        let task_ref = job
            .control
            .path
            .join(format!("task-git-{}/{}", job.task, commit.branch));
        assert_eq!(
            std::fs::read_to_string(task_ref).unwrap().trim(),
            commit.oid
        );
    }
}
