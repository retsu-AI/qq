//! Workspace snapshots and `qq undo`.
//!
//! Before a run first changes anything, the runtime records the workspace's
//! working tree, untracked files included, as a commit under
//! `refs/qq/snapshots/`. Restoring a snapshot first records the current state
//! the same way, so an undo can itself be undone.
//!
//! The user's index, stash, branches and HEAD are never touched: snapshots
//! are written through a temporary index file, and restores only rewrite
//! files under the workspace. Ignored files and anything outside the
//! workspace are not covered. A workspace outside a Git work tree gets no
//! snapshot.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

/// Snapshot refs live here. Shared by every worktree of the repository so
/// their objects survive `git gc`; the workspace path in each commit keeps
/// one worktree's snapshots apart from another's.
pub const SNAPSHOT_REF_PREFIX: &str = "refs/qq/snapshots/";

/// Snapshots kept per repository; older ones are deleted after each new one.
pub const MAX_SNAPSHOTS: usize = 50;

/// Untracked files larger than this are left out of snapshots, and restores
/// leave them alone.
pub const MAX_UNTRACKED_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// One Git step may take this long before the snapshot is abandoned.
const GIT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

const WORKSPACE_TRAILER: &str = "QQ-Workspace: ";
const RUN_TRAILER: &str = "QQ-Run: ";
const HEAD_TRAILER: &str = "QQ-Head: ";
const KIND_TRAILER: &str = "QQ-Kind: ";
const CREATED_TRAILER: &str = "QQ-Created-Ms: ";

/// Why a snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotKind {
    /// Before a run's first change.
    Run(qq_protocol::RunId),
    /// Before `qq undo` restored an older snapshot.
    BeforeUndo,
}

/// A recorded snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The ref name below [`SNAPSHOT_REF_PREFIX`]: the run id, or
    /// `undo-<unix ms>` for a snapshot taken by an undo.
    pub name: String,
    pub commit: String,
    pub kind: SnapshotKind,
    /// The workspace directory the snapshot covers.
    pub workspace: PathBuf,
    /// HEAD when the snapshot was taken; `None` before the first commit.
    pub head: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Untracked files over [`MAX_UNTRACKED_FILE_BYTES`] that were left out,
    /// relative to the workspace. Empty for listed snapshots.
    pub skipped: Vec<PathBuf>,
}

/// What a restore changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The state before the restore, which `qq undo <name>` brings back.
    pub before: Snapshot,
    /// `git diff --name-status` lines, relative to the workspace.
    pub changes: Vec<String>,
    /// HEAD moved since the snapshot was taken: (then, now). The restore
    /// changed files only.
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
}

/// Where the workspace sits in its repository.
struct Repository {
    workspace: PathBuf,
    index: PathBuf,
    git_dir: PathBuf,
}

/// Records the workspace's working tree. `Ok(None)` when the workspace is
/// not in a Git work tree.
pub async fn snapshot(workspace: &Path, kind: SnapshotKind) -> Result<Option<Snapshot>, UndoError> {
    let Some(repository) = discover(workspace).await? else {
        return Ok(None);
    };
    let snapshot = record(&repository, kind).await?;
    prune(&repository.workspace, MAX_SNAPSHOTS).await?;
    Ok(Some(snapshot))
}

/// Snapshots that cover `directory`: those whose workspace is `directory`
/// or its nearest ancestor that has any. Newest first.
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

/// Restores `target`'s workspace to the snapshot, after recording the
/// current state as a [`SnapshotKind::BeforeUndo`] snapshot.
pub async fn restore(target: &Snapshot) -> Result<Restored, UndoError> {
    let Some(repository) = discover(&target.workspace).await? else {
        return Err(UndoError::NotGit(target.workspace.clone()));
    };
    let before = record(&repository, SnapshotKind::BeforeUndo).await?;
    // The scratch index lists every current file, untracked ones included,
    // so the restore deletes files the snapshot does not have.
    let index = ScratchIndex::copy(&repository).await?;
    let skipped = stage_workspace(&repository, &index).await?;
    // The backup above could not hold the skipped files, so the restore
    // leaves them alone even where the snapshot has an older version.
    git_with_input(
        &repository.workspace,
        Some(&index.path),
        "restore",
        &[
            "restore",
            "--no-overlay",
            "--source",
            &target.commit,
            "--worktree",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        pathspecs(&skipped),
    )
    .await?;
    drop(index);
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
    prune(&repository.workspace, MAX_SNAPSHOTS).await?;
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
    let tree = git(
        &repository.workspace,
        Some(&index.path),
        "write-tree",
        &["write-tree"],
    )
    .await?;
    drop(index);
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
    let tree = tree.trim();
    let mut arguments = vec!["commit-tree", "--no-gpg-sign", tree, "-m", message.as_str()];
    if let Some(head) = &head {
        arguments.extend(["-p", head.as_str()]);
    }
    let commit = git(&repository.workspace, None, "commit-tree", &arguments).await?;
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
    // Newest first; the name breaks ties within a millisecond.
    snapshots.sort_by(|a, b| (b.created_at_ms, &b.name).cmp(&(a.created_at_ms, &a.name)));
    Ok(snapshots)
}

async fn prune(directory: &Path, keep: usize) -> Result<(), UndoError> {
    for stale in all_snapshots(directory).await?.into_iter().skip(keep) {
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

/// Stages every file under the workspace into the scratch index, except
/// untracked files over [`MAX_UNTRACKED_FILE_BYTES`], which it returns.
/// Leaving them out of the index also keeps a `--no-overlay` restore from
/// deleting them.
async fn stage_workspace(
    repository: &Repository,
    index: &ScratchIndex,
) -> Result<Vec<PathBuf>, UndoError> {
    // `assume-unchanged` makes `add` keep the indexed content instead of the
    // file's. Clear it in the copy only; the user's index keeps the bit.
    let listing = git(
        &repository.workspace,
        Some(&index.path),
        "ls-files",
        &["ls-files", "-z", "-v", "--", "."],
    )
    .await?;
    let mut assumed = Vec::new();
    for entry in listing.split('\0') {
        // A lowercase tag marks an assume-unchanged entry.
        if let Some((tag, path)) = entry.split_once(' ')
            && tag.chars().all(|tag| tag.is_ascii_lowercase())
        {
            assumed.extend_from_slice(path.as_bytes());
            assumed.push(0);
        }
    }
    if !assumed.is_empty() {
        git_with_input(
            &repository.workspace,
            Some(&index.path),
            "update-index",
            &["update-index", "--no-assume-unchanged", "-z", "--stdin"],
            assumed,
        )
        .await?;
    }
    let untracked = git(
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
    )
    .await?;
    let mut skipped = Vec::new();
    for relative in untracked.split('\0').filter(|path| !path.is_empty()) {
        let path = repository.workspace.join(relative);
        let large = tokio::fs::symlink_metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > MAX_UNTRACKED_FILE_BYTES);
        if large {
            skipped.push(PathBuf::from(relative));
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

/// The workspace minus `excluded`, as NUL-separated pathspecs.
fn pathspecs(excluded: &[PathBuf]) -> Vec<u8> {
    let mut pathspecs = String::from(".\0");
    for relative in excluded {
        pathspecs.push_str(":(exclude,literal)");
        pathspecs.push_str(&relative.to_string_lossy());
        pathspecs.push('\0');
    }
    pathspecs.into_bytes()
}

/// A copy of the repository's index under the Git directory, removed on
/// drop. Copying keeps Git's stat cache, so recording a large tree costs
/// about as much as `git status`.
struct ScratchIndex {
    path: PathBuf,
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
        Ok(Self { path })
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn command(directory: &Path, index: Option<&Path>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command.current_dir(directory);
    // Inherited repository overrides (a hook's GIT_DIR, GIT_INDEX_FILE)
    // would aim every step at some other repository or index.
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
    use tokio::io::AsyncWriteExt as _;
    let mut command = command(directory, index);
    command.args(arguments.iter().map(OsStr::new));
    if !input.is_empty() {
        command.stdin(Stdio::piped());
    }
    let run = async {
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&input).await?;
        }
        child.wait_with_output().await
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

    /// Runs git in `directory` with a fixture identity and no user config.
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

    /// A repository with one commit holding `tracked.txt`.
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
        let taken = snapshot(root, SnapshotKind::Run(run))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken.name, run.to_string());
        assert_eq!(git(root, &["ls-files", "--stage"]), index_before);

        // What an agent might do.
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
        // Ignored files are outside the snapshot and stay as they are.
        assert_eq!(read(&root.join("ignored.txt")).as_deref(), Some("agent\n"));
        // Restores rewrite files only: the agent's `git add` stays staged.
        assert!(git(root, &["diff", "--cached", "--name-only"]).contains("tracked.txt"));
        assert!(git(root, &["stash", "list"]).is_empty());
        let mut changes = restored.changes.clone();
        changes.sort();
        assert_eq!(
            changes,
            ["A\tuntracked.txt", "D\tcreated.txt", "M\ttracked.txt"]
        );
        assert_eq!(restored.head_moved, None);

        // The undo is itself undoable.
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
            snapshot(&plain, SnapshotKind::BeforeUndo).await.unwrap(),
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
        prune(root, 2).await.unwrap();
        assert_eq!(all_snapshots(root).await.unwrap().len(), 2);
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
        // The before-undo snapshot could not hold it, so the restore must
        // not overwrite it.
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
        // The user's index keeps its bit.
        assert!(git(root, &["ls-files", "-v", "tracked.txt"]).starts_with("h "));
    }

    #[tokio::test]
    async fn the_scratch_index_is_removed() {
        let fixture = directory();
        let root = fixture.path();
        repository(root);
        snapshot(root, SnapshotKind::BeforeUndo)
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
