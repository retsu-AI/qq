use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

pub const SNAPSHOT_REF_PREFIX: &str = "refs/qq/snapshots/";

pub const MAX_SNAPSHOTS: usize = 50;

pub const MAX_UNTRACKED_FILE_BYTES: u64 = 16 * 1024 * 1024;

const GIT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

const WORKSPACE_TRAILER: &str = "QQ-Workspace: ";
const RUN_TRAILER: &str = "QQ-Run: ";
const HEAD_TRAILER: &str = "QQ-Head: ";
const KIND_TRAILER: &str = "QQ-Kind: ";
const CREATED_TRAILER: &str = "QQ-Created-Ms: ";
const PROTECTED_TRAILER: &str = "QQ-Protected: ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotKind {
    Run(qq_protocol::RunId),
    BeforeUndo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    pub commit: String,
    pub kind: SnapshotKind,
    pub workspace: PathBuf,
    pub head: Option<String>,
    pub created_at_ms: u64,
    pub skipped: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub before: Snapshot,
    pub changes: Vec<String>,
    pub head_moved: Option<(Option<String>, Option<String>)>,
}

#[derive(Debug, Error)]
pub enum UndoError {
    #[error("{0} is not inside a Git work tree; snapshots need Git")]
    NotGit(PathBuf),
    #[error("could not run git: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("git {step} timed out after {seconds} s")]
    Timeout { step: &'static str, seconds: u64 },
    #[error("git {step} failed: {stderr}")]
    Git { step: &'static str, stderr: String },
    #[error("could not prepare the snapshot index: {0}")]
    Index(#[source] std::io::Error),
    #[error("the workspace path cannot be recorded in a snapshot: {0}")]
    UnrecordablePath(PathBuf),
    #[error("no snapshot named {0}; qq undo --list shows them")]
    UnknownSnapshot(String),
    #[error("{0} matches more than one snapshot; give more of the name")]
    AmbiguousSnapshot(String),
    #[error(
        "no run has been snapshotted in {0}; snapshots are taken before a run first changes files in a Git work tree"
    )]
    NoRunSnapshot(PathBuf),
}

#[derive(Debug, Clone, Error)]
#[error(transparent)]
pub struct UndoFailure(std::sync::Arc<UndoError>);

impl UndoFailure {
    #[must_use]
    pub fn cause(&self) -> &UndoError {
        &self.0
    }
}

impl From<UndoError> for UndoFailure {
    fn from(error: UndoError) -> Self {
        Self(std::sync::Arc::new(error))
    }
}

impl PartialEq for UndoFailure {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0) || self.0.to_string() == other.0.to_string()
    }
}

impl Eq for UndoFailure {}

pub fn select<'a>(
    snapshots: &'a [Snapshot],
    directory: &Path,
    name: Option<&str>,
) -> Result<&'a Snapshot, UndoError> {
    match name {
        Some(name) => {
            let mut matching = snapshots
                .iter()
                .filter(|snapshot| snapshot.name.starts_with(name));
            match (matching.next(), matching.next()) {
                (Some(one), None) => Ok(one),
                (None, _) => Err(UndoError::UnknownSnapshot(name.to_owned())),
                (Some(_), Some(_)) => Err(UndoError::AmbiguousSnapshot(name.to_owned())),
            }
        }
        None => snapshots
            .iter()
            .find(|snapshot| matches!(snapshot.kind, SnapshotKind::Run(_)))
            .ok_or_else(|| UndoError::NoRunSnapshot(directory.to_owned())),
    }
}

struct Repository {
    workspace: PathBuf,
    index: PathBuf,
    git_dir: PathBuf,
}

pub async fn snapshot(
    workspace: &Path,
    kind: SnapshotKind,
    retention: Option<Duration>,
) -> Result<Option<Snapshot>, UndoError> {
    let Some(repository) = discover(workspace).await? else {
        return Ok(None);
    };
    let snapshot = record(&repository, kind).await?;
    let cutoff_ms = retention.map(|retention| {
        snapshot
            .created_at_ms
            .saturating_sub(u64::try_from(retention.as_millis()).unwrap_or(u64::MAX))
    });
    prune(&repository.workspace, MAX_SNAPSHOTS, cutoff_ms).await?;
    Ok(Some(snapshot))
}

pub async fn covers(directory: &Path) -> Result<bool, UndoError> {
    Ok(discover(directory).await?.is_some())
}

pub async fn list(directory: &Path) -> Result<Vec<Snapshot>, UndoError> {
    if discover(directory).await?.is_none() {
        return Err(UndoError::NotGit(directory.to_owned()));
    }
    let mut snapshots = all_snapshots(directory).await?;
    let nearest = snapshots
        .iter()
        .filter(|snapshot| directory.starts_with(&snapshot.workspace))
        .map(|snapshot| snapshot.workspace.components().count())
        .max();
    snapshots.retain(|snapshot| {
        directory.starts_with(&snapshot.workspace)
            && Some(snapshot.workspace.components().count()) == nearest
    });
    Ok(snapshots)
}

pub async fn restore(target: &Snapshot) -> Result<Restored, UndoError> {
    let Some(repository) = discover(&target.workspace).await? else {
        return Err(UndoError::NotGit(target.workspace.clone()));
    };
    let before = record(&repository, SnapshotKind::BeforeUndo).await?;
    let index = ScratchIndex::copy(&repository).await?;
    let mut excluded = stage_workspace(&repository, &index).await?;
    excluded.extend(ignored(&repository, &index).await?);
    excluded.extend(protected(&repository, &target.commit).await?);
    excluded.extend(gitlinks(&repository, &index).await?);
    let replaced = replaced_ancestors(&repository, &target.commit, &excluded).await?;
    excluded.extend(replaced);
    git_with_input(
        &repository.workspace,
        Some(&index.path),
        "restore",
        &[
            "restore",
            "--no-recurse-submodules",
            "--no-overlay",
            "--source",
            &target.commit,
            "--worktree",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        pathspecs(&excluded),
    )
    .await?;
    index.remove().await;
    let diff = git(
        &repository.workspace,
        None,
        "diff",
        &[
            "diff",
            "--name-status",
            "--no-renames",
            "--relative",
            &before.commit,
            &target.commit,
            "--",
            ".",
        ],
    )
    .await?;
    let changes = diff.lines().map(str::to_owned).collect();
    let head_moved =
        (before.head != target.head).then(|| (target.head.clone(), before.head.clone()));
    prune(&repository.workspace, MAX_SNAPSHOTS, None).await?;
    Ok(Restored {
        before,
        changes,
        head_moved,
    })
}

async fn discover(directory: &Path) -> Result<Option<Repository>, UndoError> {
    let output = command(directory, None)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--is-inside-work-tree",
            "--git-path",
            "index",
            "--absolute-git-dir",
        ])
        .output();
    let output = match tokio::time::timeout(GIT_STEP_TIMEOUT, output).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(Err(error)) => return Err(UndoError::Spawn(error)),
        Err(_) => return Err(timeout("rev-parse")),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some("true"), Some(index), Some(git_dir)) = (lines.next(), lines.next(), lines.next())
    else {
        return Ok(None);
    };
    Ok(Some(Repository {
        workspace: directory.to_owned(),
        index: PathBuf::from(index),
        git_dir: PathBuf::from(git_dir),
    }))
}

async fn record(repository: &Repository, kind: SnapshotKind) -> Result<Snapshot, UndoError> {
    let workspace_text = repository
        .workspace
        .to_str()
        .filter(|text| !text.contains('\n'))
        .ok_or_else(|| UndoError::UnrecordablePath(repository.workspace.clone()))?
        .to_owned();
    let index = ScratchIndex::copy(repository).await?;
    let skipped = stage_workspace(repository, &index).await?;
    let left_out = ignored(repository, &index).await?;
    let tree = git(
        &repository.workspace,
        Some(&index.path),
        "write-tree",
        &["write-tree"],
    )
    .await?;
    index.remove().await;
    let head = git(
        &repository.workspace,
        None,
        "rev-parse",
        &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
    )
    .await
    .ok()
    .map(|head| head.trim().to_owned())
    .filter(|head| !head.is_empty());
    let created_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let (name, kind_line) = match &kind {
        SnapshotKind::Run(run) => (run.to_string(), format!("{RUN_TRAILER}{run}")),
        SnapshotKind::BeforeUndo => (
            format!("undo-{created_ms}"),
            format!("{KIND_TRAILER}before-undo"),
        ),
    };
    let mut message = format!(
        "qq snapshot\n\n{WORKSPACE_TRAILER}{workspace_text}\n{kind_line}\n{CREATED_TRAILER}{created_ms}\n"
    );
    if let Some(head) = &head {
        message.push_str(&format!("{HEAD_TRAILER}{head}\n"));
    }
    for path in skipped.iter().chain(&left_out) {
        message.push_str(PROTECTED_TRAILER);
        for byte in path_bytes(path) {
            message.push_str(&format!("{byte:02x}"));
        }
        message.push('\n');
    }
    let tree = tree.trim();
    let mut arguments = vec!["commit-tree", "--no-gpg-sign", tree, "-F", "-"];
    if let Some(head) = &head {
        arguments.extend(["-p", head.as_str()]);
    }
    let commit = git_with_input(
        &repository.workspace,
        None,
        "commit-tree",
        &arguments,
        message.into_bytes(),
    )
    .await?;
    let commit = commit.trim().to_owned();
    let reference = format!("{SNAPSHOT_REF_PREFIX}{name}");
    git(
        &repository.workspace,
        None,
        "update-ref",
        &["update-ref", "-m", "qq snapshot", &reference, &commit],
    )
    .await?;
    Ok(Snapshot {
        name,
        commit,
        kind,
        workspace: repository.workspace.clone(),
        head,
        created_at_ms: created_ms,
        skipped,
    })
}

async fn all_snapshots(directory: &Path) -> Result<Vec<Snapshot>, UndoError> {
    let listing = git(
        directory,
        None,
        "for-each-ref",
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(contents:body)%00",
            SNAPSHOT_REF_PREFIX,
        ],
    )
    .await?;
    let fields: Vec<&str> = listing.split('\0').collect();
    let mut snapshots = Vec::new();
    for record in fields.chunks_exact(3) {
        let [reference, commit, body] = record else {
            continue;
        };
        let reference = reference.trim_start_matches('\n');
        let Some(name) = reference.strip_prefix(SNAPSHOT_REF_PREFIX) else {
            continue;
        };
        let mut workspace = None;
        let mut kind = None;
        let mut head = None;
        let mut created_at_ms = 0;
        for line in body.lines() {
            if let Some(path) = line.strip_prefix(WORKSPACE_TRAILER) {
                workspace = Some(PathBuf::from(path));
            } else if let Some(run) = line.strip_prefix(RUN_TRAILER) {
                kind = run.parse().ok().map(SnapshotKind::Run);
            } else if line == format!("{KIND_TRAILER}before-undo") {
                kind = Some(SnapshotKind::BeforeUndo);
            } else if let Some(commit) = line.strip_prefix(HEAD_TRAILER) {
                head = Some(commit.to_owned());
            } else if let Some(created) = line.strip_prefix(CREATED_TRAILER) {
                created_at_ms = created.parse().unwrap_or(0);
            }
        }
        let (Some(workspace), Some(kind)) = (workspace, kind) else {
            continue;
        };
        snapshots.push(Snapshot {
            name: name.to_owned(),
            commit: (*commit).to_owned(),
            kind,
            workspace,
            head,
            created_at_ms,
            skipped: Vec::new(),
        });
    }
    snapshots.sort_by(|a, b| (b.created_at_ms, &b.name).cmp(&(a.created_at_ms, &a.name)));
    Ok(snapshots)
}

async fn prune(directory: &Path, keep: usize, cutoff_ms: Option<u64>) -> Result<(), UndoError> {
    let snapshots = all_snapshots(directory).await?;
    for (index, stale) in snapshots.into_iter().enumerate() {
        if index < keep && cutoff_ms.is_none_or(|cutoff| stale.created_at_ms >= cutoff) {
            continue;
        }
        let reference = format!("{SNAPSHOT_REF_PREFIX}{}", stale.name);
        git(
            directory,
            None,
            "update-ref",
            &["update-ref", "-d", &reference],
        )
        .await?;
    }
    Ok(())
}

async fn stage_workspace(
    repository: &Repository,
    index: &ScratchIndex,
) -> Result<Vec<PathBuf>, UndoError> {
    let listing = git_bytes(
        &repository.workspace,
        Some(&index.path),
        "ls-files",
        &["ls-files", "-z", "-v", "--", "."],
        Vec::new(),
    )
    .await?;
    let mut assumed = Vec::new();
    let mut skipped_worktree = Vec::new();
    for entry in nul_separated(&listing) {
        let Some(space) = entry.iter().position(|byte| *byte == b' ') else {
            continue;
        };
        let (tag, path) = (&entry[..space], &entry[space + 1..]);
        if tag.iter().all(u8::is_ascii_lowercase) {
            assumed.extend_from_slice(path);
            assumed.push(0);
        }
        if tag.eq_ignore_ascii_case(b"s")
            && tokio::fs::symlink_metadata(repository.workspace.join(path_from_bytes(path)))
                .await
                .is_ok()
        {
            skipped_worktree.extend_from_slice(path);
            skipped_worktree.push(0);
        }
    }
    for (flag, paths) in [
        ("--no-assume-unchanged", assumed),
        ("--no-skip-worktree", skipped_worktree),
    ] {
        if !paths.is_empty() {
            git_with_input(
                &repository.workspace,
                Some(&index.path),
                "update-index",
                &["update-index", flag, "-z", "--stdin"],
                paths,
            )
            .await?;
        }
    }
    let untracked = git_bytes(
        &repository.workspace,
        Some(&index.path),
        "ls-files",
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ],
        Vec::new(),
    )
    .await?;
    let mut skipped = Vec::new();
    for relative in nul_separated(&untracked).map(path_from_bytes) {
        let large = tokio::fs::symlink_metadata(repository.workspace.join(&relative))
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > MAX_UNTRACKED_FILE_BYTES);
        if large {
            skipped.push(relative);
        }
    }
    git_with_input(
        &repository.workspace,
        Some(&index.path),
        "add",
        &["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
        pathspecs(&skipped),
    )
    .await?;
    Ok(skipped)
}

async fn ignored(repository: &Repository, index: &ScratchIndex) -> Result<Vec<PathBuf>, UndoError> {
    let listing = git_bytes(
        &repository.workspace,
        Some(&index.path),
        "ls-files",
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--",
            ".",
        ],
        Vec::new(),
    )
    .await?;
    Ok(nul_separated(&listing)
        .map(|path| path_from_bytes(path.strip_suffix(b"/").unwrap_or(path)))
        .collect())
}

async fn gitlinks(
    repository: &Repository,
    index: &ScratchIndex,
) -> Result<Vec<PathBuf>, UndoError> {
    let listing = git_bytes(
        &repository.workspace,
        Some(&index.path),
        "ls-files",
        &["ls-files", "-z", "--stage", "--", "."],
        Vec::new(),
    )
    .await?;
    Ok(nul_separated(&listing)
        .filter(|entry| entry.starts_with(b"160000 "))
        .filter_map(|entry| {
            let tab = entry.iter().position(|byte| *byte == b'\t')?;
            Some(path_from_bytes(&entry[tab + 1..]))
        })
        .collect())
}

async fn protected(repository: &Repository, commit: &str) -> Result<Vec<PathBuf>, UndoError> {
    let object = git(
        &repository.workspace,
        None,
        "cat-file",
        &["cat-file", "commit", commit],
    )
    .await?;
    Ok(object
        .lines()
        .filter_map(|line| line.strip_prefix(PROTECTED_TRAILER))
        .filter_map(|hex| {
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            (!bytes.is_empty()).then(|| path_from_bytes(&bytes))
        })
        .collect())
}

async fn replaced_ancestors(
    repository: &Repository,
    commit: &str,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, UndoError> {
    let mut ancestors: Vec<&Path> = paths
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect();
    ancestors.sort();
    ancestors.dedup();
    if ancestors.is_empty() {
        return Ok(Vec::new());
    }
    let mut objects = Vec::new();
    for ancestor in &ancestors {
        objects.extend_from_slice(commit.as_bytes());
        objects.extend_from_slice(b":./");
        objects.extend_from_slice(&path_bytes(ancestor));
        objects.push(0);
    }
    let types = git_bytes(
        &repository.workspace,
        None,
        "cat-file",
        &["cat-file", "--batch-check=%(objecttype)", "-Z"],
        objects,
    )
    .await?;
    Ok(ancestors
        .into_iter()
        .zip(types.split(|byte| *byte == 0))
        .filter(|(_, kind)| matches!(*kind, b"blob" | b"commit"))
        .map(|(ancestor, _)| ancestor.to_owned())
        .collect())
}

fn nul_separated(listing: &[u8]) -> impl Iterator<Item = &[u8]> {
    listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

fn pathspecs(excluded: &[PathBuf]) -> Vec<u8> {
    let mut pathspecs = b".\0".to_vec();
    for relative in excluded {
        pathspecs.extend_from_slice(b":(exclude,literal)");
        pathspecs.extend_from_slice(&path_bytes(relative));
        pathspecs.push(0);
    }
    pathspecs
}

struct ScratchIndex {
    path: PathBuf,
    removed: bool,
}

impl ScratchIndex {
    async fn copy(repository: &Repository) -> Result<Self, UndoError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = repository
            .git_dir
            .join(format!("qq-snapshot-{}-{nonce}.index", std::process::id()));
        match tokio::fs::copy(&repository.index, &path).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(UndoError::Index(error)),
        }
        Ok(Self {
            path,
            removed: false,
        })
    }

    async fn remove(mut self) {
        self.removed = true;
        let _ = tokio::fs::remove_file(&self.path).await;
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let path = std::mem::take(&mut self.path);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || std::fs::remove_file(path));
            }
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn command(directory: &Path, index: Option<&Path>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command.current_dir(directory);
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(name);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command.args([
        "-c",
        if cfg!(windows) {
            "core.hooksPath=NUL"
        } else {
            "core.hooksPath=/dev/null"
        },
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.autocrlf=false",
    ]);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "QQ")
        .env("GIT_AUTHOR_EMAIL", "qq@localhost")
        .env("GIT_COMMITTER_NAME", "QQ")
        .env("GIT_COMMITTER_EMAIL", "qq@localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn git(
    directory: &Path,
    index: Option<&Path>,
    step: &'static str,
    arguments: &[&str],
) -> Result<String, UndoError> {
    git_with_input(directory, index, step, arguments, Vec::new()).await
}

async fn git_with_input(
    directory: &Path,
    index: Option<&Path>,
    step: &'static str,
    arguments: &[&str],
    input: Vec<u8>,
) -> Result<String, UndoError> {
    let output = git_bytes(directory, index, step, arguments, input).await?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

async fn git_bytes(
    directory: &Path,
    index: Option<&Path>,
    step: &'static str,
    arguments: &[&str],
    input: Vec<u8>,
) -> Result<Vec<u8>, UndoError> {
    use tokio::io::AsyncWriteExt as _;
    let mut command = command(directory, index);
    command.args(arguments.iter().map(OsStr::new));
    if !input.is_empty() {
        command.stdin(Stdio::piped());
    }
    let run = async {
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let feed = async {
            if let Some(mut stdin) = stdin {
                stdin.write_all(&input).await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (fed, output) = tokio::join!(feed, child.wait_with_output());
        let output = output?;
        fed?;
        Ok(output)
    };
    let output = tokio::time::timeout(GIT_STEP_TIMEOUT, run)
        .await
        .map_err(|_| timeout(step))?
        .map_err(UndoError::Spawn)?;
    if !output.status.success() {
        return Err(UndoError::Git {
            step,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output.stdout)
}

fn timeout(step: &'static str) -> UndoError {
    UndoError::Timeout {
        step,
        seconds: GIT_STEP_TIMEOUT.as_secs(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    pub(crate) fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    pub(crate) fn repository(directory: &Path) {
        git(directory, &["init", "-q", "--template=", "."]);
        git(directory, &["config", "commit.gpgSign", "false"]);
        std::fs::write(directory.join("tracked.txt"), "base\n").unwrap();
        git(directory, &["add", "tracked.txt"]);
        git(directory, &["commit", "-q", "-m", "base"]);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{git, repository};
    use super::*;

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    #[tokio::test]
    async fn restore_keeps_line_endings_byte_for_byte_under_autocrlf() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        git(root, &["config", "core.autocrlf", "true"]);
        std::fs::write(root.join("draft.txt"), "lf\nmixed\r\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        std::fs::remove_file(root.join("draft.txt")).unwrap();
        restore(&taken).await.unwrap();
        assert_eq!(std::fs::read(root.join("tracked.txt")).unwrap(), b"base\n");
        assert_eq!(
            std::fs::read(root.join("draft.txt")).unwrap(),
            b"lf\nmixed\r\n"
        );
    }

    #[tokio::test]
    async fn restore_reverts_edits_deletions_and_new_files_without_touching_the_index() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(root, &["add", "staged.txt"]);
        std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        git(root, &["stash", "list"]);
        let index_before = git(root, &["ls-files", "--stage"]);

        let run = qq_protocol::RunId::generate().unwrap();
        let taken = snapshot(root, SnapshotKind::Run(run), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken.name, run.to_string());
        assert_eq!(git(root, &["ls-files", "--stage"]), index_before);

        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        std::fs::remove_file(root.join("untracked.txt")).unwrap();
        std::fs::write(root.join("created.txt"), "new\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "agent\n").unwrap();
        git(root, &["add", "tracked.txt"]);

        let restored = restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(
            read(&root.join("untracked.txt")).as_deref(),
            Some("untracked\n")
        );
        assert_eq!(read(&root.join("staged.txt")).as_deref(), Some("staged\n"));
        assert_eq!(read(&root.join("created.txt")), None);
        assert_eq!(read(&root.join("ignored.txt")).as_deref(), Some("agent\n"));
        assert!(git(root, &["diff", "--cached", "--name-only"]).contains("tracked.txt"));
        assert!(git(root, &["stash", "list"]).is_empty());
        let mut changes = restored.changes.clone();
        changes.sort();
        assert_eq!(
            changes,
            ["A\tuntracked.txt", "D\tcreated.txt", "M\ttracked.txt"]
        );
        assert_eq!(restored.head_moved, None);

        restore(&restored.before).await.unwrap();
        assert_eq!(
            read(&root.join("tracked.txt")).as_deref(),
            Some("changed\n")
        );
        assert_eq!(read(&root.join("created.txt")).as_deref(), Some("new\n"));
        assert_eq!(read(&root.join("untracked.txt")), None);
    }

    #[tokio::test]
    async fn a_subdirectory_workspace_restores_only_its_own_files() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let workspace = root.join("crate");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("lib.rs"), "one\n").unwrap();

        let taken = snapshot(
            &workspace,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(workspace.join("lib.rs"), "two\n").unwrap();
        std::fs::write(root.join("tracked.txt"), "outside\n").unwrap();

        let listed = list(&workspace).await.unwrap();
        assert_eq!(listed, std::slice::from_ref(&taken));
        assert!(list(root).await.unwrap().is_empty());
        restore(&taken).await.unwrap();
        assert_eq!(read(&workspace.join("lib.rs")).as_deref(), Some("one\n"));
        assert_eq!(
            read(&root.join("tracked.txt")).as_deref(),
            Some("outside\n")
        );
    }

    #[tokio::test]
    async fn a_commit_after_the_snapshot_is_reported_not_reverted() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(root, &["commit", "-q", "-am", "agent"]);
        let head = git(root, &["rev-parse", "HEAD"]).trim().to_owned();

        let restored = restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), head);
        assert_eq!(restored.head_moved, Some((taken.head.clone(), Some(head))));
    }

    #[tokio::test]
    async fn a_repository_without_commits_can_be_snapshotted() {
        let fixture = directory();
        let root = fixture.path();
        git(root, &["init", "-q", "--template=", "."]);
        std::fs::write(root.join("first.txt"), "first\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(taken.head, None);
        std::fs::write(root.join("first.txt"), "second\n").unwrap();
        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("first.txt")).as_deref(), Some("first\n"));
    }

    #[tokio::test]
    async fn outside_git_there_is_no_snapshot() {
        let fixture = directory();
        let plain = fixture.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(
            snapshot(&plain, SnapshotKind::BeforeUndo, None)
                .await
                .unwrap(),
            None
        );
        assert!(matches!(list(&plain).await, Err(UndoError::NotGit(_))));
    }

    #[tokio::test]
    async fn pruning_bounds_the_snapshot_count() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let repository = discover(root).await.unwrap().unwrap();
        for _ in 0..4 {
            let run = qq_protocol::RunId::generate().unwrap();
            record(&repository, SnapshotKind::Run(run)).await.unwrap();
        }
        assert_eq!(all_snapshots(root).await.unwrap().len(), 4);
        prune(root, 2, None).await.unwrap();
        assert_eq!(all_snapshots(root).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn pruning_drops_snapshots_older_than_the_cutoff() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let repository = discover(root).await.unwrap().unwrap();
        let mut taken = Vec::new();
        for _ in 0..2 {
            let run = qq_protocol::RunId::generate().unwrap();
            taken.push(record(&repository, SnapshotKind::Run(run)).await.unwrap());
        }
        let oldest = taken
            .iter()
            .map(|snapshot| snapshot.created_at_ms)
            .min()
            .unwrap();
        prune(root, MAX_SNAPSHOTS, Some(oldest)).await.unwrap();
        assert_eq!(all_snapshots(root).await.unwrap().len(), 2);
        prune(root, MAX_SNAPSHOTS, Some(u64::MAX)).await.unwrap();
        assert!(all_snapshots(root).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn large_untracked_files_are_skipped_and_survive_a_restore() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let large = root.join("large.bin");
        let file = std::fs::File::create(&large).unwrap();
        file.set_len(MAX_UNTRACKED_FILE_BYTES + 1).unwrap();
        drop(file);
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(taken.skipped, [PathBuf::from("large.bin")]);
        assert!(!git(root, &["ls-tree", "-r", "--name-only", &taken.commit]).contains("large.bin"));
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(
            std::fs::metadata(&large).unwrap().len(),
            MAX_UNTRACKED_FILE_BYTES + 1
        );
    }

    #[tokio::test]
    async fn an_untracked_file_that_grew_past_the_limit_survives_a_restore() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let grown = root.join("grown.bin");
        std::fs::write(&grown, "small\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(taken.skipped.is_empty());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&grown)
            .unwrap()
            .set_len(MAX_UNTRACKED_FILE_BYTES + 1)
            .unwrap();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

        let restored = restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(restored.before.skipped, [PathBuf::from("grown.bin")]);
        assert_eq!(
            std::fs::metadata(&grown).unwrap().len(),
            MAX_UNTRACKED_FILE_BYTES + 1
        );
    }

    #[tokio::test]
    async fn assume_unchanged_files_are_recorded_from_the_working_tree() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        git(root, &["update-index", "--assume-unchanged", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "draft\n").unwrap();

        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            git(root, &["show", &format!("{}:tracked.txt", taken.commit)]),
            "draft\n"
        );
        std::fs::write(root.join("tracked.txt"), "agent\n").unwrap();
        let restored = restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("draft\n"));
        restore(&restored.before).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("agent\n"));
        assert!(git(root, &["ls-files", "-v", "tracked.txt"]).starts_with("h "));
    }

    #[tokio::test]
    async fn a_file_ignored_since_the_snapshot_survives_a_restore() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join("notes.txt"), "old\n").unwrap();
        std::fs::create_dir(root.join("cache")).unwrap();
        std::fs::write(root.join("cache/entry"), "old\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join(".gitignore"), "notes.txt\ncache/\n").unwrap();
        std::fs::write(root.join("notes.txt"), "newer\n").unwrap();
        std::fs::write(root.join("cache/entry"), "newer\n").unwrap();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(read(&root.join("notes.txt")).as_deref(), Some("newer\n"));
        assert_eq!(read(&root.join("cache/entry")).as_deref(), Some("newer\n"));
    }

    #[tokio::test]
    async fn reversing_an_undo_keeps_files_the_undo_left_alone() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join("notes"), "old\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join(".gitignore"), "notes\n").unwrap();
        std::fs::write(root.join("notes"), "newer\n").unwrap();

        let restored = restore(&taken).await.unwrap();
        assert_eq!(read(&root.join(".gitignore")), None);
        assert_eq!(read(&root.join("notes")).as_deref(), Some("newer\n"));
        restore(&restored.before).await.unwrap();
        assert_eq!(read(&root.join(".gitignore")).as_deref(), Some("notes\n"));
        assert_eq!(read(&root.join("notes")).as_deref(), Some("newer\n"));
    }

    #[tokio::test]
    async fn a_file_ignored_when_the_snapshot_was_taken_survives_a_restore() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join(".gitignore"), "local.env\n").unwrap();
        std::fs::write(root.join("local.env"), "user\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::remove_file(root.join(".gitignore")).unwrap();
        std::fs::write(root.join("local.env"), "agent\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(
            read(&root.join(".gitignore")).as_deref(),
            Some("local.env\n")
        );
        assert_eq!(read(&root.join("local.env")).as_deref(), Some("agent\n"));
    }

    #[tokio::test]
    async fn an_ignored_directory_is_not_replaced_by_a_snapshotted_file() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join("foo"), "file\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::remove_file(root.join("foo")).unwrap();
        std::fs::create_dir(root.join("foo")).unwrap();
        std::fs::write(root.join("foo/secret"), "secret\n").unwrap();
        std::fs::write(root.join(".gitignore"), "foo/\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("foo/secret")).as_deref(), Some("secret\n"));
    }

    #[tokio::test]
    async fn many_excluded_paths_do_not_stall_the_ancestor_check() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let repository = discover(root).await.unwrap().unwrap();
        let head = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
        let paths: Vec<PathBuf> = (0..5_000)
            .map(|n| PathBuf::from(format!("absent-{n:05}/file")))
            .collect();
        let replaced = tokio::time::timeout(
            Duration::from_secs(10),
            replaced_ancestors(&repository, &head, &paths),
        )
        .await
        .expect("cat-file output is drained while its input is written")
        .unwrap();
        assert!(replaced.is_empty());
    }

    #[tokio::test]
    async fn a_directory_holding_excluded_files_is_not_replaced_by_a_snapshotted_file() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("foo"), "file\n").unwrap();
        std::fs::write(root.join("nested/bar"), "file\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::remove_file(root.join("foo")).unwrap();
        std::fs::create_dir(root.join("foo")).unwrap();
        std::fs::write(root.join("foo/secret"), "secret\n").unwrap();
        std::fs::write(root.join("foo/plain"), "plain\n").unwrap();
        std::fs::write(root.join(".gitignore"), "secret\n").unwrap();
        std::fs::remove_file(root.join("nested/bar")).unwrap();
        std::fs::create_dir(root.join("nested/bar")).unwrap();
        let large = root.join("nested/bar/large.bin");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_UNTRACKED_FILE_BYTES + 1)
            .unwrap();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(read(&root.join("foo/secret")).as_deref(), Some("secret\n"));
        assert_eq!(
            std::fs::metadata(&large).unwrap().len(),
            MAX_UNTRACKED_FILE_BYTES + 1
        );
    }

    #[tokio::test]
    async fn an_ignored_folder_below_a_folder_survives_a_restore() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::create_dir_all(root.join("web/node_modules/left-pad")).unwrap();
        std::fs::write(root.join("web/index.js"), "user\n").unwrap();
        std::fs::write(root.join("web/node_modules/left-pad/index.js"), "dep\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join("web/index.js"), "agent\n").unwrap();
        std::fs::write(root.join("web/node_modules/left-pad/index.js"), "rebuilt\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("web/index.js")).as_deref(), Some("user\n"));
        assert_eq!(
            read(&root.join("web/node_modules/left-pad/index.js")).as_deref(),
            Some("rebuilt\n")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_folder_named_with_a_newline_is_not_replaced_by_a_snapshotted_file() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let odd = root.join("odd\ndir");
        let similar = root.join("odd");
        std::fs::write(&odd, "file\n").unwrap();
        std::fs::write(&similar, "similar\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::remove_file(&odd).unwrap();
        std::fs::create_dir(&odd).unwrap();
        std::fs::write(odd.join("secret"), "secret\n").unwrap();
        std::fs::write(root.join(".gitignore"), "secret\n").unwrap();
        std::fs::write(&similar, "agent\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(read(&odd.join("secret")).as_deref(), Some("secret\n"));
        assert_eq!(read(&similar).as_deref(), Some("similar\n"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_newly_ignored_file_with_a_non_utf8_name_survives_a_restore() {
        use std::os::unix::ffi::OsStrExt as _;
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        let name = OsStr::from_bytes(b"notes-\xff.txt");
        std::fs::write(root.join(name), "old\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join(".gitignore"), "notes-*.txt\n").unwrap();
        std::fs::write(root.join(name), "newer\n").unwrap();

        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join(name)).as_deref(), Some("newer\n"));
    }

    #[tokio::test]
    async fn skip_worktree_files_are_recorded_from_the_working_tree() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::create_dir(root.join("sparse")).unwrap();
        std::fs::write(root.join("sparse/absent.txt"), "absent\n").unwrap();
        git(root, &["add", "sparse/absent.txt"]);
        git(root, &["commit", "-q", "-m", "sparse"]);
        git(root, &["update-index", "--skip-worktree", "tracked.txt"]);
        git(
            root,
            &["update-index", "--skip-worktree", "sparse/absent.txt"],
        );
        std::fs::write(root.join("tracked.txt"), "draft\n").unwrap();
        std::fs::remove_file(root.join("sparse/absent.txt")).unwrap();

        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let show = |path: &str| git(root, &["show", &format!("{}:{path}", taken.commit)]);
        assert_eq!(show("tracked.txt"), "draft\n");
        assert_eq!(show("sparse/absent.txt"), "absent\n");
        std::fs::write(root.join("tracked.txt"), "agent\n").unwrap();
        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("draft\n"));
        assert_eq!(read(&root.join("sparse/absent.txt")), None);
        let listing = git(root, &["ls-files", "-v"]);
        assert!(listing.contains("S tracked.txt"), "{listing}");
        assert!(listing.contains("S sparse/absent.txt"), "{listing}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_hooks_do_not_run() {
        use std::os::unix::fs::PermissionsExt as _;
        let fixture = directory();
        let root = &fixture.path().join("root");
        std::fs::create_dir(root).unwrap();
        repository(root);
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let marker = fixture.path().join("hook-ran");
        let hook = hooks.join("reference-transaction");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(root, &["update-ref", "refs/fixture/probe", "HEAD"]);
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let taken = snapshot(root, SnapshotKind::BeforeUndo, None)
            .await
            .unwrap()
            .unwrap();
        restore(&taken).await.unwrap();
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_restore_leaves_submodule_working_trees_alone() {
        let fixture = directory();
        let upstream = fixture.path().join("upstream");
        let root = fixture.path().join("root");
        for directory in [&upstream, &root] {
            std::fs::create_dir(directory).unwrap();
            repository(directory);
        }
        git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../upstream",
                "sub",
            ],
        );
        git(&root, &["commit", "-q", "-m", "submodule"]);
        git(&root, &["config", "submodule.recurse", "true"]);
        std::fs::write(root.join("sub/tracked.txt"), "dirty\n").unwrap();

        let taken = snapshot(
            &root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        restore(&taken).await.unwrap();
        assert_eq!(read(&root.join("tracked.txt")).as_deref(), Some("base\n"));
        assert_eq!(
            read(&root.join("sub/tracked.txt")).as_deref(),
            Some("dirty\n")
        );
    }

    #[tokio::test]
    async fn a_nested_repository_is_not_replaced_by_a_snapshotted_file() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        std::fs::write(root.join("foo"), "file\n").unwrap();
        let taken = snapshot(
            root,
            SnapshotKind::Run(qq_protocol::RunId::generate().unwrap()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::remove_file(root.join("foo")).unwrap();
        let nested = root.join("foo");
        std::fs::create_dir(&nested).unwrap();
        repository(&nested);
        std::fs::write(nested.join("tracked.txt"), "dirty\n").unwrap();

        restore(&taken).await.unwrap();
        assert!(nested.join(".git").is_dir());
        assert_eq!(
            read(&nested.join("tracked.txt")).as_deref(),
            Some("dirty\n")
        );
    }

    #[tokio::test]
    async fn the_scratch_index_is_removed() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        snapshot(root, SnapshotKind::BeforeUndo, None)
            .await
            .unwrap()
            .unwrap();
        let leftovers = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("qq-snapshot-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }
}
