//! Private, credential-free commit jobs launched by the durable command worker.
//! These records are trusted host input, never authority supplied by a model.
pub use crate::task_git::{TaskGitCommit, TaskGitError, TaskGitLimits};
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
    pub message: String,
    pub not_after_ms: u64,
}

#[derive(Debug, Clone)]
pub struct TaskGitJobFiles {
    pub request: PathBuf,
    pub receipt: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskGitJobResult {
    Committed { commit: TaskGitCommit },
    Refused { reason: String },
    Stopped,
    CleanupUnconfirmed,
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

    fn validate(&self) -> Result<(), TaskGitJobError> {
        self.digest()?;
        task_git::oid(self.base.clone())?;
        if self.paths.len() > self.limits.max_entries
            || self.protected_paths.len() > self.limits.max_entries
            || self.message.is_empty()
            || self.message.len() > 16 * 1024
            || self.message.contains('\0')
        {
            return Err(TaskGitJobError::Invalid("invalid commit bounds"));
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
    let files = job.files();
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
    Ok(receipt)
}

/// Single-shot hidden CLI entry. The worker owns cancellation and descendant custody.
/// Successful return means a durable typed receipt exists, including refusals.
pub fn execute_job(request: &Path) -> Result<TaskGitJobReceipt, TaskGitJobError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let now_ms = u64::try_from(now.as_millis()).map_err(io::Error::other)?;
    execute_job_at(request, now_ms)
}

fn execute_job_at(request: &Path, now_ms: u64) -> Result<TaskGitJobReceipt, TaskGitJobError> {
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
    let result = runtime.block_on(commit(&job, now_ms, restored));
    let result = match result {
        Ok(commit) => TaskGitJobResult::Committed { commit },
        Err(TaskGitError::Stopped) => TaskGitJobResult::Stopped,
        Err(TaskGitError::CleanupUnconfirmed) => TaskGitJobResult::CleanupUnconfirmed,
        Err(error) => TaskGitJobResult::Refused {
            reason: error.to_string(),
        },
    };
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
            job.message.clone(),
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

    fn fixture() -> (TempDir, TaskGitJob) {
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
            message: "task commit".into(),
            not_after_ms: 30_000,
        };
        (root, job)
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
        changed.message = "different call".into();
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
        job.message = "x".repeat(MAX_RECORD_BYTES + 1);
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
