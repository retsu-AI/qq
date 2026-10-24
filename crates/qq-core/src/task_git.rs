//! Trusted task commits. No transport, credentials, hooks or workspace Git index.
use crate::{
    RunCancellation,
    command_owner::{NativeDirectoryGrant, NativeWorkspaceIdentity},
    workspace::Workspace,
};
use cap_std::fs::OpenOptions;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

#[path = "task_git_commit.rs"]
mod commit;
#[path = "task_git_program.rs"]
mod program;
use program::GitProgram;
#[path = "task_git_objects.rs"]
mod objects;
#[path = "task_git_process.rs"]
mod process;
use commit::CommitPlan;
#[cfg(test)]
#[path = "task_git_tests.rs"]
mod tests;

pub(crate) struct TaskGitRoots {
    pub source: NativeDirectoryGrant,
    pub control: NativeDirectoryGrant,
    pub task: NativeDirectoryGrant,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskGitError {
    #[error("task Git I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("task Git operation stopped")]
    Stopped,
    #[error("task Git operation is already active")]
    Busy,
    #[error("task Git cleanup was not confirmed")]
    CleanupUnconfirmed,
    #[error("task Git {command} failed with exit code {code:?}")]
    GitFailed { command: String, code: Option<i32> },
    #[error("invalid task Git state: {0}")]
    Invalid(&'static str),
    #[error("task Git metadata failed: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("task Git blocking worker failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub(crate) struct Operation {
    pub cancellation: RunCancellation,
    pub deadline: Instant,
    pub worker_owned: bool,
}
impl Operation {
    async fn permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, TaskGitError> {
        self.check()?;
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(TaskGitError::Stopped),
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(self.deadline)) => Err(TaskGitError::Stopped),
            permit = crate::workspace::blocking_permits().acquire_owned() => permit.map_err(|_| TaskGitError::Stopped),
        }
    }
    fn check(&self) -> Result<(), TaskGitError> {
        if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
            Err(TaskGitError::Stopped)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub max_pack_bytes: usize,
}
impl TaskGitLimits {
    fn valid(self) -> bool {
        self.max_entries != 0
            && self.max_entries.checked_mul(80).is_some()
            && self.max_bytes != 0
            && self.max_bytes.checked_add(1).is_some()
            && self.max_pack_bytes != 0
            && self.max_pack_bytes.checked_add(1).is_some()
    }
}
/// Source-write authority: restore only from trusted host storage outside task grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskGit {
    program: GitProgram,
    repository: NativeDirectoryGrant,
    source: NativeDirectoryGrant,
    task: NativeDirectoryGrant,
    branch: String,
    base: String,
    prefix: PathBuf,
    limits: TaskGitLimits,
    #[serde(skip)]
    worker_owned: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preparation {
    git: TaskGit,
    objects: NativeDirectoryGrant,
    format: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitCommit {
    pub oid: String,
    pub tree: String,
    pub branch: String,
    pub base: String,
}

#[derive(Debug)]
pub(crate) enum TaskGitPrepared {
    Complete(TaskGitCommit),
    Pending(TaskGitCommit),
}

impl TaskGit {
    /// Repeatable read performed before importing task bytes or running a model.
    pub(crate) fn capture_base(
        program: &Path,
        source: &NativeDirectoryGrant,
        control: &NativeDirectoryGrant,
        operation: &Operation,
    ) -> Result<Option<String>, TaskGitError> {
        operation.check()?;
        let roots = TaskGitRoots {
            source: source.clone(),
            control: control.clone(),
            task: control.clone(),
        };
        let program = match GitProgram::capture(program, &roots) {
            Ok(program) => program,
            Err(TaskGitError::Invalid("workspace has no Git checkout marker")) => return Ok(None),
            Err(error) => return Err(error),
        };
        let git = Self {
            program,
            repository: control.clone(),
            source: source.clone(),
            task: control.clone(),
            branch: String::new(),
            base: String::new(),
            prefix: PathBuf::new(),
            limits: TaskGitLimits {
                max_entries: 1,
                max_bytes: 1,
                max_pack_bytes: 1,
            },
            worker_owned: operation.worker_owned,
        };
        git.text(
            &source.path,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            operation,
        )
        .and_then(oid)
        .map(Some)
    }

    pub(crate) async fn prepare(
        program: PathBuf,
        roots: TaskGitRoots,
        task: qq_protocol::RunId,
        base: String,
        limits: TaskGitLimits,
        operation: Operation,
    ) -> Result<Self, TaskGitError> {
        let permit = operation.permit().await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::prepare_blocking(program, roots, task, base, limits, operation)
        })
        .await?
    }

    pub(crate) fn restore_job(
        program: &Path,
        roots: &TaskGitRoots,
        task: qq_protocol::RunId,
        base: String,
        limits: TaskGitLimits,
    ) -> Result<Option<Self>, TaskGitError> {
        held(&roots.control)?;
        let path = roots.control.path.join(format!("task-git-{task}"));
        if !path.try_exists()? {
            return Ok(None);
        }
        let repository = NativeDirectoryGrant::capture(&path)?;
        let private = held(&repository)?;
        let Some(saved) = metadata::<Self>(&private, "task-state.json")? else {
            return Ok(None);
        };
        let expected = Self {
            program: GitProgram::capture(program, roots)?,
            repository,
            source: roots.source.clone(),
            task: roots.task.clone(),
            branch: format!("refs/heads/qq/task/{task}"),
            base: oid(base)?,
            prefix: PathBuf::new(),
            limits,
            worker_owned: true,
        };
        expected.validate_saved(&saved)?;
        Ok(Some(saved))
    }

    pub(crate) fn inherit_worker_custody(&mut self) {
        self.worker_owned = true;
    }

    fn prepare_blocking(
        program: PathBuf,
        roots: TaskGitRoots,
        task: qq_protocol::RunId,
        base: String,
        limits: TaskGitLimits,
        operation: Operation,
    ) -> Result<Self, TaskGitError> {
        operation.check()?;
        if !limits.valid() {
            return Err(TaskGitError::Invalid(
                "invalid server-supplied program or limits",
            ));
        }
        let program = GitProgram::capture(&program, &roots)?;
        let TaskGitRoots {
            source,
            control,
            task: execution,
        } = roots;
        let task_root = held(&execution)?;
        let root = held(&control)?;
        let source_root = held(&source)?;
        if contains_directory(root.root(), source.identity)?
            || contains_directory(source_root.root(), control.identity)?
            || contains_directory(root.root(), execution.identity)?
            || contains_directory(source_root.root(), execution.identity)?
            || contains_directory(task_root.root(), source.identity)?
        {
            return Err(TaskGitError::Invalid("source and private control overlap"));
        }
        let name = format!("task-git-{task}");
        match root.root().create_dir(&name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let path = control.path.join(name);
        let repository = NativeDirectoryGrant::capture(&path)?;
        let mut git = Self {
            program,
            repository,
            source,
            task: execution,
            branch: format!("refs/heads/qq/task/{task}"),
            base: oid(base)?,
            prefix: PathBuf::new(),
            limits,
            worker_owned: operation.worker_owned,
        };
        let private = held(&git.repository)?;
        if contains_directory(private.root(), git.task.identity)? {
            return Err(TaskGitError::Invalid("private Git state is task-writable"));
        }
        let _lock = private_lock(&private)?;
        if let Some(saved) = metadata::<Self>(&private, "task-state.json")? {
            git.validate_saved(&saved)?;
            saved
                .text(&path, &["rev-parse", "--verify", &saved.branch], &operation)
                .and_then(oid)?;
            return Ok(saved);
        }
        let preparation = match metadata::<Preparation>(&private, "task-owner.json")? {
            Some(owner) => {
                git.validate_saved(&owner.git)?;
                owner
            }
            None => {
                if private.root().symlink_metadata("HEAD").is_ok()
                    || private.root().symlink_metadata("refs").is_ok()
                    || private.root().symlink_metadata("objects").is_ok()
                {
                    return Err(TaskGitError::Invalid(
                        "existing Git state has no matching task owner",
                    ));
                }
                for name in ["empty-hooks", "empty-template"] {
                    match private.root().create_dir(name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                    let directory = cap_primitives::fs::open_dir_nofollow(
                        &private.root().try_clone()?.into_std_file(),
                        Path::new(name),
                    )?;
                    if cap_std::fs::Dir::from_std_file(directory)
                        .entries()?
                        .next()
                        .is_some()
                    {
                        return Err(TaskGitError::Invalid(
                            "task Git template or hooks directory is not empty",
                        ));
                    }
                }
                let mut empty_options = OpenOptions::new();
                empty_options.write(true).create_new(true);
                match private.root().open_with("empty-config", &empty_options) {
                    Ok(file) => file.sync_all()?,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let bytes = read_file(&private, Path::new("empty-config"), 0)?;
                        if bytes.is_none() {
                            return Err(TaskGitError::Invalid("missing empty Git configuration"));
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
                let format = git.text(
                    &git.source.path,
                    &["rev-parse", "--show-object-format"],
                    &operation,
                )?;
                if !matches!(format.as_str(), "sha1" | "sha256") {
                    return Err(TaskGitError::Invalid("unsupported Git object format"));
                }
                let top = git.text(
                    &git.source.path,
                    &["rev-parse", "--show-toplevel"],
                    &operation,
                )?;
                let top = std::fs::canonicalize(top)?;
                let selected = std::fs::canonicalize(&git.source.path)?;
                git.prefix = selected
                    .strip_prefix(&top)
                    .map_err(|_| TaskGitError::Invalid("workspace is outside its Git top-level"))?
                    .to_path_buf();
                validate_prefix(&git.prefix)?;
                let common = PathBuf::from(git.text(
                    &git.source.path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    &operation,
                )?);
                let objects = NativeDirectoryGrant::capture(&common.join("objects"))?;
                let owner = Preparation {
                    git,
                    objects,
                    format,
                };
                save_metadata(&private, "task-owner.json", &owner)?;
                owner
            }
        };
        let Preparation {
            git,
            objects,
            format,
        } = preparation;
        if private.root().symlink_metadata("HEAD").is_ok() {
            match git.text(&path, &["rev-parse", "--verify", &git.branch], &operation) {
                Ok(head) => {
                    let head = oid(head)?;
                    git.text(
                        &path,
                        &["merge-base", "--is-ancestor", &git.base, &head],
                        &operation,
                    )?;
                    save_metadata(&private, "task-state.json", &git)?;
                    return Ok(git);
                }
                Err(TaskGitError::GitFailed { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        git.text(
            &path,
            &[
                "init",
                "--bare",
                &format!("--object-format={format}"),
                "--template=empty-template",
                ".",
            ],
            &operation,
        )?;
        let args = ["pack-objects", "--stdout", "--revs"];
        objects::validate(&objects, limits.max_pack_bytes, &operation)?;
        let pack = git.run(
            &path,
            &args,
            format!("{}\n", git.base).into_bytes(),
            Some(&objects),
            &operation,
            limits.max_pack_bytes,
        )?;
        git.run(
            &path,
            &["index-pack", "--stdin"],
            pack,
            None,
            &operation,
            1024,
        )?;
        git.text(
            &path,
            &[
                "update-ref",
                &git.branch,
                &git.base,
                &"0".repeat(git.base.len()),
            ],
            &operation,
        )?;
        held(&git.source)?;
        save_metadata(&private, "task-state.json", &git)?;
        Ok(git)
    }

    pub(crate) async fn freeze(
        &self,
        snapshot: NativeDirectoryGrant,
        paths: Vec<PathBuf>,
        message: String,
        invocation: qq_protocol::ToolCallId,
        operation: Operation,
    ) -> Result<TaskGitPrepared, TaskGitError> {
        if snapshot.path != self.task.path || snapshot.identity != self.task.identity {
            return Err(TaskGitError::Invalid("snapshot belongs to another task"));
        }
        if let Some(observed) = self.observe(invocation, message.clone()).await? {
            if matches!(observed, TaskGitPrepared::Pending(_)) {
                operation.check()?;
            }
            return Ok(observed);
        }
        let permit = operation.permit().await?;
        let git = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            git.freeze_blocking(snapshot, paths, message, invocation, operation)
        })
        .await?
    }

    /// Call only after the session owner has freshly authorized this exact
    /// frozen invocation. Completed observations require no new authority.
    pub(crate) async fn install(
        &self,
        invocation: qq_protocol::ToolCallId,
        operation: Operation,
    ) -> Result<TaskGitCommit, TaskGitError> {
        if let Some(TaskGitPrepared::Complete(result)) =
            self.observe_stored(invocation, None).await?
        {
            return Ok(result);
        }
        let permit = operation.permit().await?;
        let git = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let root = held(&git.repository)?;
            let _lock = private_lock(&root)?;
            let plan = git
                .load_plan(invocation)?
                .ok_or(TaskGitError::Invalid("missing frozen commit plan"))?;
            git.install_plan(invocation, plan, &operation)
        })
        .await?
    }

    /// Bounded historical lookup, independent of the original effect deadline.
    pub(crate) async fn observe(
        &self,
        invocation: qq_protocol::ToolCallId,
        message: String,
    ) -> Result<Option<TaskGitPrepared>, TaskGitError> {
        self.observe_stored(invocation, Some(message)).await
    }

    async fn observe_stored(
        &self,
        invocation: qq_protocol::ToolCallId,
        message: Option<String>,
    ) -> Result<Option<TaskGitPrepared>, TaskGitError> {
        let git = self.clone();
        tokio::time::timeout(Duration::from_secs(2), async move {
            let permit = crate::workspace::blocking_permits()
                .acquire_owned()
                .await
                .map_err(|_| TaskGitError::Stopped)?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let root = held(&git.repository)?;
                let _lock = private_lock(&root)?;
                let Some(plan) = git.load_plan(invocation)? else {
                    return Ok(None);
                };
                if message.is_some_and(|message| message != plan.message) {
                    return Err(TaskGitError::Invalid(
                        "invocation conflicts with its frozen commit message",
                    ));
                }
                git.observe_plan(invocation, plan).map(Some)
            })
            .await?
        })
        .await
        .map_err(|_| TaskGitError::Stopped)?
    }

    #[cfg(test)]
    async fn commit(
        &self,
        snapshot: NativeDirectoryGrant,
        paths: Vec<PathBuf>,
        message: String,
        invocation: qq_protocol::ToolCallId,
        operation: Operation,
    ) -> Result<TaskGitCommit, TaskGitError> {
        match self
            .freeze(snapshot, paths, message, invocation, operation.clone())
            .await?
        {
            TaskGitPrepared::Complete(result) => Ok(result),
            TaskGitPrepared::Pending(expected) => {
                let actual = self.install(invocation, operation).await?;
                assert_eq!(actual, expected);
                Ok(actual)
            }
        }
    }

    fn freeze_blocking(
        &self,
        snapshot: NativeDirectoryGrant,
        paths: Vec<PathBuf>,
        message: String,
        invocation: qq_protocol::ToolCallId,
        operation: Operation,
    ) -> Result<TaskGitPrepared, TaskGitError> {
        let private = held(&self.repository)?;
        if snapshot.path != self.task.path || snapshot.identity != self.task.identity {
            return Err(TaskGitError::Invalid("snapshot belongs to another task"));
        }
        let _lock = private_lock(&private)?;
        if let Some(plan) = self.saved_plan(invocation, &snapshot, &message)? {
            return self.observe_plan(invocation, plan);
        }
        operation.check()?;
        held(&self.source)?;
        let snapshot_grant = snapshot;
        let snapshot = held(&snapshot_grant)?;
        if !self.limits.valid()
            || paths.len() > self.limits.max_entries
            || message.is_empty()
            || message.len() > 16 * 1024
            || message.contains('\0')
        {
            return Err(TaskGitError::Invalid("invalid task commit bounds"));
        }
        let old = oid(self.text(
            &self.repository.path,
            &["rev-parse", "--verify", &self.branch],
            &operation,
        )?)?;
        // The manifest is cumulative against immutable import bytes. Rebuild
        // from the task-start base so reverted edits disappear without
        // accidentally committing unrelated pre-existing dirty source bytes.
        self.text(
            &self.repository.path,
            &["read-tree", &self.base],
            &operation,
        )?;
        let mut selected = BTreeSet::new();
        let mut updates = Vec::new();
        let mut blobs = Vec::new();
        let mut next_mark = 1;
        let mut total = 0;
        for path in paths {
            operation.check()?;
            validate_path(&path)?;
            let path_text = path
                .to_str()
                .ok_or(TaskGitError::Invalid("non-UTF8 commit path"))?;
            snapshot
                .check_write_path(path_text, &path)
                .map_err(|_| TaskGitError::Invalid("protected commit path"))?;
            if !selected.insert(path.clone()) {
                return Err(TaskGitError::Invalid("duplicate commit path"));
            }
            let indexed_path = self.prefix.join(&path);
            validate_path(&indexed_path)?;
            let indexed_path = git_path(&indexed_path)?;
            match read_file(
                &snapshot,
                &path,
                self.limits.max_bytes.saturating_sub(total),
            )? {
                Some((bytes, executable)) => {
                    total += bytes.len();
                    blobs.extend_from_slice(
                        format!("blob\nmark :{next_mark}\ndata {}\n", bytes.len()).as_bytes(),
                    );
                    blobs.extend_from_slice(&bytes);
                    blobs.push(b'\n');
                    updates.push((indexed_path.to_owned(), Some((next_mark, executable))));
                    next_mark += 1;
                }
                None => updates.push((indexed_path.to_owned(), None)),
            }
        }
        held(&snapshot_grant)?;
        let marks = self.import_blobs(blobs, next_mark - 1, &operation)?;
        let mut index = Vec::new();
        for (path, update) in updates {
            match update {
                Some((mark, executable)) => {
                    let hash = marks
                        .get(&mark)
                        .ok_or(TaskGitError::Invalid("missing imported blob"))?;
                    index.extend_from_slice(
                        format!("{} {hash}\t", if executable { "100755" } else { "100644" })
                            .as_bytes(),
                    );
                }
                None => {
                    index.extend_from_slice(format!("0 {}\t", "0".repeat(old.len())).as_bytes())
                }
            }
            index.extend_from_slice(path.as_bytes());
            index.push(0);
        }
        self.run(
            &self.repository.path,
            &["update-index", "-z", "--index-info"],
            index,
            None,
            &operation,
            1024,
        )?;
        let tree = oid(self.text(&self.repository.path, &["write-tree"], &operation)?)?;
        let previous_tree = oid(self.text(
            &self.repository.path,
            &["rev-parse", &format!("{old}^{{tree}}")],
            &operation,
        )?)?;
        let new = if previous_tree == tree {
            old.clone()
        } else {
            oid(String::from_utf8(self.run(
                &self.repository.path,
                &["commit-tree", &tree, "-p", &old],
                message.as_bytes().to_vec(),
                None,
                &operation,
                1024,
            )?)
            .map_err(|_| TaskGitError::Invalid("invalid commit identity"))?
            .trim()
            .to_owned())?
        };
        held(&self.repository)?;
        held(&snapshot_grant)?;
        let plan = CommitPlan {
            snapshot: snapshot_grant,
            message,
            previous: old,
            result: TaskGitCommit {
                oid: new,
                tree,
                branch: self.branch.clone(),
                base: self.base.clone(),
            },
            completed: false,
        };
        self.save_plan(invocation, &plan)?;
        self.observe_plan(invocation, plan)
    }

    fn validate_saved(&self, saved: &Self) -> Result<(), TaskGitError> {
        if self.program != saved.program
            || self.branch != saved.branch
            || self.base != saved.base
            || self.limits != saved.limits
            || self.task.path != saved.task.path
            || self.task.identity != saved.task.identity
            || self.source.path != saved.source.path
            || self.source.identity != saved.source.identity
            || self.repository.path != saved.repository.path
            || self.repository.identity != saved.repository.identity
        {
            return Err(TaskGitError::Invalid("saved Git task authority differs"));
        }
        validate_prefix(&saved.prefix)?;
        held(&saved.source)?;
        held(&saved.repository)?;
        oid(saved.base.clone())?;
        Ok(())
    }

    fn import_blobs(
        &self,
        mut blobs: Vec<u8>,
        count: usize,
        operation: &Operation,
    ) -> Result<BTreeMap<usize, String>, TaskGitError> {
        if count == 0 {
            return Ok(BTreeMap::new());
        }
        blobs.extend_from_slice(b"done\n");
        self.run(
            &self.repository.path,
            &[
                "fast-import",
                "--done",
                "--quiet",
                "--export-marks=task-marks",
            ],
            blobs,
            None,
            operation,
            1024,
        )?;
        let root = held(&self.repository)?;
        let mut file = root.root().open("task-marks")?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((count * 80 + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > count * 80 {
            return Err(TaskGitError::Invalid("oversized imported blob marks"));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| TaskGitError::Invalid("invalid blob marks"))?;
        let mut marks = BTreeMap::new();
        for line in text.lines() {
            let (mark, hash) = line
                .split_once(' ')
                .ok_or(TaskGitError::Invalid("invalid blob mark"))?;
            let mark: usize = mark
                .strip_prefix(':')
                .and_then(|mark| mark.parse().ok())
                .ok_or(TaskGitError::Invalid("invalid blob number"))?;
            if mark == 0 || mark > count || marks.insert(mark, oid(hash.to_owned())?).is_some() {
                return Err(TaskGitError::Invalid("duplicate or out of range blob mark"));
            }
        }
        if marks.len() != count {
            return Err(TaskGitError::Invalid("incomplete blob marks"));
        }
        Ok(marks)
    }

    fn text(
        &self,
        cwd: &Path,
        args: &[&str],
        operation: &Operation,
    ) -> Result<String, TaskGitError> {
        String::from_utf8(self.run(cwd, args, Vec::new(), None, operation, 16 * 1024)?)
            .map(|value| value.trim().to_owned())
            .map_err(|_| TaskGitError::Invalid("invalid Git metadata"))
    }
}

pub(crate) fn held(grant: &NativeDirectoryGrant) -> Result<Workspace, TaskGitError> {
    let directory = Workspace::open(&grant.path)?;
    if NativeWorkspaceIdentity::held(directory.root())? != grant.identity {
        return Err(TaskGitError::Invalid("directory identity changed"));
    }
    Ok(directory)
}
pub(crate) fn oid(value: String) -> Result<String, TaskGitError> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(value)
    } else {
        Err(TaskGitError::Invalid("invalid Git object identity"))
    }
}
pub(crate) fn validate_path(path: &Path) -> Result<(), TaskGitError> {
    #[cfg(not(windows))]
    if path.to_string_lossy().contains('\\') {
        return Err(TaskGitError::Invalid("ambiguous cross-platform Git path"));
    }
    #[cfg(windows)]
    if path
        .components()
        .any(|part| part.as_os_str().to_string_lossy().ends_with(['.', ' ']))
    {
        return Err(TaskGitError::Invalid("ambiguous Windows commit path"));
    }
    if path.as_os_str().is_empty()
        || path.to_string_lossy().contains(':')
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        Err(TaskGitError::Invalid("commit path escapes task"))
    } else {
        Ok(())
    }
}
pub(crate) fn read_file(
    root: &Workspace,
    path: &Path,
    limit: usize,
) -> Result<Option<(Vec<u8>, bool)>, TaskGitError> {
    let metadata = match root.root().symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    check_file(&metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    options._cap_fs_ext_nonblock(true);
    let mut file = root.root().open_with(path, &options)?;
    same_file(&metadata, &file.metadata()?)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    same_file(&metadata, &file.metadata()?)?;
    same_file(&metadata, &root.root().symlink_metadata(path)?)?;
    if bytes.len() > limit {
        return Err(TaskGitError::Invalid("task commit byte limit exceeded"));
    }
    #[cfg(unix)]
    let executable = {
        use cap_std::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some((bytes, executable)))
}

pub(crate) fn check_file(metadata: &cap_std::fs::Metadata) -> Result<(), TaskGitError> {
    #[cfg(unix)]
    let independent = {
        use cap_std::fs::MetadataExt as _;
        metadata.nlink() == 1
    };
    #[cfg(windows)]
    let independent = {
        use cap_primitives::fs::_WindowsByHandle as _;
        metadata.number_of_links() == Some(1) && metadata.file_attributes() & 0x400 == 0
    };
    #[cfg(not(any(unix, windows)))]
    let independent = false;
    if !metadata.is_file() || metadata.is_symlink() || !independent {
        return Err(TaskGitError::Invalid("linked or non-regular commit input"));
    }
    Ok(())
}
fn same_file(
    before: &cap_std::fs::Metadata,
    after: &cap_std::fs::Metadata,
) -> Result<(), TaskGitError> {
    check_file(after)?;
    #[cfg(unix)]
    let same = {
        use cap_std::fs::MetadataExt as _;
        before.dev() == after.dev() && before.ino() == after.ino()
    };
    #[cfg(windows)]
    let same = {
        use cap_primitives::fs::_WindowsByHandle as _;
        before.volume_serial_number() == after.volume_serial_number()
            && before.file_index() == after.file_index()
    };
    #[cfg(not(any(unix, windows)))]
    let same = false;
    if !same || before.len() != after.len() || before.modified()? != after.modified()? {
        return Err(TaskGitError::Invalid("commit input changed while reading"));
    }
    Ok(())
}

fn private_lock(root: &Workspace) -> Result<std::fs::File, TaskGitError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(false)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    let file = root.root().open_with("task.lock", &options)?;
    check_file(&file.metadata()?)?;
    let file = file.into_std();
    file.try_lock().map_err(|_| TaskGitError::Busy)?;
    Ok(file)
}
pub(crate) fn metadata<T: serde::de::DeserializeOwned>(
    root: &Workspace,
    name: &str,
) -> Result<Option<T>, TaskGitError> {
    read_file(root, Path::new(name), 256 * 1024)?
        .map(|(bytes, _)| serde_json::from_slice(&bytes).map_err(TaskGitError::from))
        .transpose()
}
pub(crate) fn save_metadata(
    root: &Workspace,
    name: &str,
    value: &impl Serialize,
) -> Result<(), TaskGitError> {
    let temporary = format!("{name}.tmp");
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(false)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    let mut file = root.root().open_with(&temporary, &options)?;
    check_file(&file.metadata()?)?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    root.root().rename(&temporary, root.root(), name)?;
    #[cfg(unix)]
    root.root().open(".")?.sync_all()?;
    Ok(())
}

fn contains_directory(
    directory: &cap_std::fs::Dir,
    identity: NativeWorkspaceIdentity,
) -> io::Result<bool> {
    let mut current = directory.try_clone()?;
    for _ in 0..4096 {
        let here = NativeWorkspaceIdentity::held(&current)?;
        if here == identity {
            return Ok(true);
        }
        let parent = current.open_parent_dir(cap_std::ambient_authority())?;
        if NativeWorkspaceIdentity::held(&parent)? == here {
            return Ok(false);
        }
        current = parent;
    }
    Err(io::Error::other("task Git directory ancestry is too deep"))
}

fn validate_prefix(prefix: &Path) -> Result<(), TaskGitError> {
    if prefix.as_os_str().is_empty() {
        Ok(())
    } else {
        validate_path(prefix)
    }
}

fn git_path(path: &Path) -> Result<String, TaskGitError> {
    validate_path(path)?;
    path.components()
        .map(|part| {
            part.as_os_str()
                .to_str()
                .ok_or(TaskGitError::Invalid("non-UTF8 repository path"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}
