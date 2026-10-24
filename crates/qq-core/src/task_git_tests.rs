use super::*;
use std::process::Command;
use tempfile::TempDir;

fn git_program() -> PathBuf {
    let executable = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(executable))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap()
}
fn git(directory: &Path, arguments: &[&str]) -> String {
    let global = directory.parent().unwrap().join("fixture-empty-global");
    std::fs::write(&global, b"").unwrap();
    let output = Command::new(git_program())
        .current_dir(directory)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global)
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn operation() -> Operation {
    Operation {
        cancellation: RunCancellation::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    }
}
fn limits() -> TaskGitLimits {
    TaskGitLimits {
        max_entries: 20,
        max_bytes: 1024 * 1024,
        max_pack_bytes: 1024 * 1024,
    }
}
struct Fixture {
    _root: TempDir,
    source: PathBuf,
    snapshot: PathBuf,
    control: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        let snapshot = root.path().join("snapshot");
        let control = root.path().join("private");
        for directory in [&source, &snapshot, &control] {
            std::fs::create_dir(directory).unwrap();
        }
        git(&source, &["init", "--template=", "."]);
        std::fs::write(source.join("file.txt"), "original\n").unwrap();
        std::fs::write(source.join("delete.txt"), "remove me\n").unwrap();
        git(&source, &["add", "--", "file.txt", "delete.txt"]);
        git(
            &source,
            &["-c", "commit.gpgSign=false", "commit", "-m", "baseline"],
        );
        std::fs::write(snapshot.join("file.txt"), "task change\n").unwrap();
        Self {
            _root: root,
            source,
            snapshot,
            control,
        }
    }
    async fn prepare(&self) -> TaskGit {
        TaskGit::prepare(
            git_program(),
            TaskGitRoots {
                control: NativeDirectoryGrant::capture(&self.control).unwrap(),
                source: NativeDirectoryGrant::capture(&self.source).unwrap(),
                task: NativeDirectoryGrant::capture(&self.snapshot).unwrap(),
            },
            qq_protocol::RunId::generate().unwrap(),
            limits(),
            operation(),
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn task_git_commit_is_private_exact_and_repeatable() {
    let fixture = Fixture::new();
    let original = git(&fixture.source, &["rev-parse", "HEAD"]);
    let original_index = std::fs::read(fixture.source.join(".git/index")).unwrap();
    let broker = fixture.prepare().await;
    std::fs::write(fixture.snapshot.join("unselected.txt"), "must not commit").unwrap();
    let paths = vec![PathBuf::from("file.txt"), PathBuf::from("delete.txt")];
    let first = broker
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            paths.clone(),
            "implement task".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    assert_ne!(first.oid, original);
    assert_eq!(first.base, original);
    assert_eq!(
        git(
            &broker.repository.path,
            &["show", &format!("{}:file.txt", first.oid)]
        ),
        "task change"
    );
    assert_eq!(
        git(
            &broker.repository.path,
            &["ls-tree", "--name-only", &first.oid]
        ),
        "file.txt"
    );
    assert_eq!(
        git(
            &broker.repository.path,
            &["rev-parse", &format!("{}^", first.oid)]
        ),
        original
    );
    let encoded = serde_json::to_vec(&broker).unwrap();
    drop(broker);
    let broker: TaskGit = serde_json::from_slice(&encoded).unwrap();
    let second = broker
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            paths,
            "same tree again".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    assert_eq!(
        second, first,
        "repeating an already installed snapshot must not manufacture another commit"
    );
    assert_eq!(git(&fixture.source, &["rev-parse", "HEAD"]), original);
    assert_eq!(
        std::fs::read(fixture.source.join(".git/index")).unwrap(),
        original_index
    );
    assert_eq!(
        std::fs::read_to_string(fixture.source.join("file.txt")).unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn task_git_never_executes_source_hooks_filters_helpers_or_signing() {
    let fixture = Fixture::new();
    let witness = fixture.source.join("executed-untrusted-config");
    let command = format!("echo escaped > '{}'", witness.display());
    for key in [
        "uploadpack.packObjectsHook",
        "core.fsmonitor",
        "core.alternateRefsCommand",
        "filter.hostile.clean",
        "filter.hostile.smudge",
        "credential.helper",
        "core.sshCommand",
        "gpg.program",
    ] {
        git(&fixture.source, &["config", key, &command]);
    }
    git(&fixture.source, &["config", "commit.gpgSign", "true"]);
    git(
        &fixture.source,
        &["config", "core.hooksPath", fixture.source.to_str().unwrap()],
    );
    for name in [
        "pre-commit",
        "commit-msg",
        "reference-transaction",
        "post-commit",
    ] {
        let path = fixture.source.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{command}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    std::fs::write(fixture.source.join(".gitattributes"), "* filter=hostile\n").unwrap();
    std::fs::write(
        fixture.snapshot.join(".gitattributes"),
        "* filter=hostile\n",
    )
    .unwrap();
    let broker = fixture.prepare().await;
    let result = broker
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            vec![PathBuf::from("file.txt"), PathBuf::from(".gitattributes")],
            "safe plumbing".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    assert_eq!(
        git(
            &broker.repository.path,
            &["show", &format!("{}:file.txt", result.oid)]
        ),
        "task change"
    );
    assert!(
        !witness.exists(),
        "repository configuration must never become executable broker authority"
    );
}

#[tokio::test]
async fn task_git_refuses_cancellation_deadlines_protected_paths_and_limits() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    for path in ["../source/file.txt", ".git/config", ".qq/config.ron"] {
        assert!(
            broker
                .commit(
                    NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
                    vec![PathBuf::from(path)],
                    "refuse".into(),
                    qq_protocol::ToolCallId::generate().unwrap(),
                    operation()
                )
                .await
                .is_err()
        );
    }
    let cancelled = operation();
    cancelled.cancellation.cancel();
    assert!(matches!(
        broker
            .commit(
                NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
                vec![PathBuf::from("file.txt")],
                "cancel".into(),
                qq_protocol::ToolCallId::generate().unwrap(),
                cancelled
            )
            .await,
        Err(TaskGitError::Stopped)
    ));
    let mut expired = operation();
    expired.deadline = Instant::now();
    assert!(matches!(
        broker
            .commit(
                NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
                vec![PathBuf::from("file.txt")],
                "expire".into(),
                qq_protocol::ToolCallId::generate().unwrap(),
                expired
            )
            .await,
        Err(TaskGitError::Stopped)
    ));
    let mut bounded = broker.clone();
    bounded.limits.max_bytes = 1;
    assert!(
        bounded
            .commit(
                NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
                vec![PathBuf::from("file.txt")],
                "large".into(),
                qq_protocol::ToolCallId::generate().unwrap(),
                operation()
            )
            .await
            .is_err()
    );
    assert_eq!(
        git(&broker.repository.path, &["rev-parse", &broker.branch]),
        broker.base
    );
    assert!(!fixture.source.join("escaped").exists());
}

#[tokio::test]
async fn task_git_refuses_replaced_snapshot_and_linked_inputs() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    std::fs::hard_link(
        fixture.source.join("file.txt"),
        fixture.snapshot.join("linked.txt"),
    )
    .unwrap();
    assert!(
        broker
            .commit(
                NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
                vec![PathBuf::from("linked.txt")],
                "linked".into(),
                qq_protocol::ToolCallId::generate().unwrap(),
                operation()
            )
            .await
            .is_err()
    );
    let snapshot = NativeDirectoryGrant::capture(&fixture.snapshot).unwrap();
    std::fs::rename(&fixture.snapshot, fixture.snapshot.with_extension("old")).unwrap();
    std::fs::create_dir(&fixture.snapshot).unwrap();
    assert!(
        broker
            .commit(
                snapshot,
                vec![],
                "replaced".into(),
                qq_protocol::ToolCallId::generate().unwrap(),
                operation()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn task_git_blob_data_cannot_inject_commands_and_commits_serialize() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    let bytes = b"\0\ncommit refs/heads/escaped\nmark :999\ndata 0\n\ndone\n";
    std::fs::write(fixture.snapshot.join("file.txt"), bytes).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(broker.repository.path.join("task.lock"))
        .unwrap();
    lock.lock().unwrap();
    assert!(matches!(
        broker
            .clone()
            .commit(
                NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
                vec![PathBuf::from("file.txt")],
                "busy".into(),
                qq_protocol::ToolCallId::generate().unwrap(),
                operation()
            )
            .await,
        Err(TaskGitError::Busy)
    ));
    drop(lock);
    let result = broker
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            vec![PathBuf::from("file.txt")],
            "raw blob".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    let output = Command::new(git_program())
        .current_dir(&broker.repository.path)
        .args(["cat-file", "blob", &format!("{}:file.txt", result.oid)])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, bytes);
    assert_eq!(
        git(
            &broker.repository.path,
            &["for-each-ref", "--format=%(refname)"]
        ),
        broker.branch
    );
}

#[tokio::test]
async fn task_git_accepts_linked_worktrees_without_changing_shared_refs() {
    let fixture = Fixture::new();
    let linked = fixture._root.path().join("linked");
    git(
        &fixture.source,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    let before = git(&fixture.source, &["for-each-ref"]);
    let broker = TaskGit::prepare(
        git_program(),
        TaskGitRoots {
            control: NativeDirectoryGrant::capture(&fixture.control).unwrap(),
            source: NativeDirectoryGrant::capture(&linked).unwrap(),
            task: NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
        },
        qq_protocol::RunId::generate().unwrap(),
        limits(),
        operation(),
    )
    .await
    .unwrap();
    broker
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            vec![PathBuf::from("file.txt")],
            "linked source".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    assert_eq!(git(&fixture.source, &["for-each-ref"]), before);
    assert_eq!(git(&linked, &["rev-parse", "HEAD"]), broker.base);
}

async fn resume(fixture: &Fixture, broker: &TaskGit) -> Result<TaskGit, TaskGitError> {
    TaskGit::prepare(
        git_program(),
        TaskGitRoots {
            control: NativeDirectoryGrant::capture(&fixture.control).unwrap(),
            source: NativeDirectoryGrant::capture(&fixture.source).unwrap(),
            task: NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
        },
        broker.branch.rsplit('/').next().unwrap().parse().unwrap(),
        limits(),
        operation(),
    )
    .await
}

#[tokio::test]
async fn task_git_prepare_resumes_original_owner_without_resetting_commits() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    std::fs::remove_file(broker.repository.path.join("task-state.json")).unwrap();
    git(
        &broker.repository.path,
        &["update-ref", "-d", &broker.branch, &broker.base],
    );
    std::fs::write(fixture.source.join("file.txt"), "new source HEAD\n").unwrap();
    git(&fixture.source, &["add", "file.txt"]);
    git(
        &fixture.source,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "unrelated source change",
        ],
    );
    let resumed = resume(&fixture, &broker).await.unwrap();
    assert_eq!(
        resumed.base, broker.base,
        "partial preparation must retain its captured base rather than current source HEAD"
    );
    let committed = resumed
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            vec![PathBuf::from("file.txt")],
            "completed task".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    std::fs::remove_file(broker.repository.path.join("task-state.json")).unwrap();
    let recovered = resume(&fixture, &broker).await.unwrap();
    assert_eq!(
        git(
            &recovered.repository.path,
            &["rev-parse", &recovered.branch]
        ),
        committed.oid,
        "an existing completed task ref must never be reset during preparation"
    );
    let again = resume(&fixture, &broker).await.unwrap();
    assert_eq!(
        git(&again.repository.path, &["rev-parse", &again.branch]),
        committed.oid
    );
}

#[tokio::test]
async fn task_git_prepare_preserves_unknown_existing_repository() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    std::fs::remove_file(broker.repository.path.join("task-state.json")).unwrap();
    std::fs::remove_file(broker.repository.path.join("task-owner.json")).unwrap();
    assert!(resume(&fixture, &broker).await.is_err());
    assert_eq!(
        git(&broker.repository.path, &["rev-parse", &broker.branch]),
        broker.base
    );
}

#[tokio::test]
async fn task_git_invocation_replays_frozen_commit_across_both_crash_windows() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    let id = qq_protocol::ToolCallId::generate().unwrap();
    let scope = NativeDirectoryGrant::capture(&fixture.snapshot).unwrap();
    let paths = vec![PathBuf::from("file.txt")];
    let TaskGitPrepared::Pending(first) = broker
        .freeze(
            scope.clone(),
            paths.clone(),
            "frozen task".into(),
            id,
            operation(),
        )
        .await
        .unwrap()
    else {
        panic!("changed snapshot must wait for fresh installation authority");
    };
    assert_eq!(
        git(&broker.repository.path, &["rev-parse", &broker.branch]),
        broker.base
    );
    std::fs::write(fixture.snapshot.join("file.txt"), "later live bytes\n").unwrap();
    let mut expired = operation();
    expired.deadline = Instant::now();
    assert!(matches!(
        broker.install(id, expired).await,
        Err(TaskGitError::Stopped)
    ));
    assert_eq!(
        git(&broker.repository.path, &["rev-parse", &broker.branch]),
        broker.base
    );
    let root = held(&broker.repository).unwrap();
    let name = format!("task-call-{id}.json");
    let retry = broker
        .commit(
            scope.clone(),
            paths.clone(),
            "frozen task".into(),
            id,
            operation(),
        )
        .await
        .unwrap();
    assert_eq!(
        retry, first,
        "before-CAS recovery installs the original frozen commit"
    );
    assert_eq!(
        git(
            &broker.repository.path,
            &["show", &format!("{}:file.txt", retry.oid)]
        ),
        "task change"
    );
    let mut plan: CommitPlan = metadata(&root, &name).unwrap().unwrap();
    plan.completed = false;
    save_metadata(&root, &name, &plan).unwrap();
    let expired = operation();
    expired.cancellation.cancel();
    let mut expired = expired;
    expired.deadline = Instant::now();
    assert_eq!(
        broker
            .commit(
                scope.clone(),
                paths.clone(),
                "frozen task".into(),
                id,
                expired
            )
            .await
            .unwrap(),
        first,
        "after-CAS recovery observes completion even after the old effect deadline"
    );
    let mut expired = operation();
    expired.deadline = Instant::now();
    assert_eq!(
        broker
            .commit(
                scope.clone(),
                paths.clone(),
                "frozen task".into(),
                id,
                expired
            )
            .await
            .unwrap(),
        first
    );
    assert!(
        broker
            .commit(
                scope,
                paths.clone(),
                "different request".into(),
                id,
                operation()
            )
            .await
            .is_err()
    );
    let different = fixture._root.path().join("different-task");
    std::fs::create_dir(&different).unwrap();
    assert!(
        broker
            .commit(
                NativeDirectoryGrant::capture(&different).unwrap(),
                paths,
                "frozen task".into(),
                id,
                operation()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn task_git_pending_plan_never_overwrites_an_unexpected_ref() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    let id = qq_protocol::ToolCallId::generate().unwrap();
    let scope = NativeDirectoryGrant::capture(&fixture.snapshot).unwrap();
    let result = broker
        .commit(
            scope.clone(),
            vec![PathBuf::from("file.txt")],
            "task".into(),
            id,
            operation(),
        )
        .await
        .unwrap();
    let name = format!("task-call-{id}.json");
    let root = held(&broker.repository).unwrap();
    let mut plan: CommitPlan = metadata(&root, &name).unwrap().unwrap();
    plan.completed = false;
    save_metadata(&root, &name, &plan).unwrap();
    let other = git(
        &broker.repository.path,
        &["commit-tree", &result.tree, "-m", "unrelated root"],
    );
    git(
        &broker.repository.path,
        &["update-ref", &broker.branch, &other],
    );
    assert!(
        broker
            .commit(
                scope,
                vec![PathBuf::from("file.txt")],
                "task".into(),
                id,
                operation()
            )
            .await
            .is_err()
    );
    assert_eq!(
        git(&broker.repository.path, &["rev-parse", &broker.branch]),
        other
    );
}

#[tokio::test]
async fn task_git_subdirectory_edits_never_overwrite_repository_siblings() {
    let fixture = Fixture::new();
    let selected = fixture.source.join("services/api");
    std::fs::create_dir_all(selected.join("src")).unwrap();
    std::fs::create_dir(fixture.source.join("src")).unwrap();
    std::fs::write(
        fixture.source.join("src/main.rs"),
        "unrelated repository root\n",
    )
    .unwrap();
    std::fs::write(selected.join("src/main.rs"), "selected project baseline\n").unwrap();
    git(
        &fixture.source,
        &["add", "src/main.rs", "services/api/src/main.rs"],
    );
    git(
        &fixture.source,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "monorepo baseline",
        ],
    );
    std::fs::create_dir(fixture.snapshot.join("src")).unwrap();
    std::fs::write(
        fixture.snapshot.join("src/main.rs"),
        "selected project change\n",
    )
    .unwrap();
    let broker = TaskGit::prepare(
        git_program(),
        TaskGitRoots {
            control: NativeDirectoryGrant::capture(&fixture.control).unwrap(),
            source: NativeDirectoryGrant::capture(&selected).unwrap(),
            task: NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
        },
        qq_protocol::RunId::generate().unwrap(),
        limits(),
        operation(),
    )
    .await
    .unwrap();
    assert_eq!(broker.prefix, Path::new("services/api"));
    let receipt = broker
        .commit(
            NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
            vec![PathBuf::from("src/main.rs")],
            "project edit".into(),
            qq_protocol::ToolCallId::generate().unwrap(),
            operation(),
        )
        .await
        .unwrap();
    assert_eq!(
        git(
            &broker.repository.path,
            &["show", &format!("{}:src/main.rs", receipt.oid)]
        ),
        "unrelated repository root"
    );
    assert_eq!(
        git(
            &broker.repository.path,
            &["show", &format!("{}:services/api/src/main.rs", receipt.oid)]
        ),
        "selected project change"
    );
}

#[tokio::test]
async fn task_git_rejects_payload_git_programs_and_image_replacement() {
    let fixture = Fixture::new();
    let roots = || TaskGitRoots {
        source: NativeDirectoryGrant::capture(&fixture.source).unwrap(),
        control: NativeDirectoryGrant::capture(&fixture.control).unwrap(),
        task: NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
    };
    let bytes = std::fs::read(git_program()).unwrap();
    for directory in [&fixture.source, &fixture.snapshot, &fixture.control] {
        let candidate = directory.join("untrusted-git");
        std::fs::write(&candidate, &bytes).unwrap();
        assert!(matches!(
            GitProgram::capture(&candidate, &roots()),
            Err(TaskGitError::Invalid(_))
        ));
    }
    let trusted = fixture._root.path().join("trusted-git-image");
    std::fs::write(&trusted, &bytes).unwrap();
    let image = GitProgram::capture(&trusted, &roots()).unwrap();
    image.validate().unwrap();
    std::fs::write(&trusted, b"changed native image").unwrap();
    assert!(image.validate().is_err());
    #[cfg(unix)]
    {
        let alias = fixture._root.path().join("git-alias");
        std::os::unix::fs::symlink(fixture.snapshot.join("untrusted-git"), &alias).unwrap();
        assert!(GitProgram::capture(&alias, &roots()).is_err());
    }
}

#[tokio::test]
async fn task_git_receipt_queue_is_bounded_when_blocking_permits_are_saturated() {
    let fixture = Fixture::new();
    let broker = fixture.prepare().await;
    let id = qq_protocol::ToolCallId::generate().unwrap();
    let scope = NativeDirectoryGrant::capture(&fixture.snapshot).unwrap();
    let TaskGitPrepared::Pending(_) = broker
        .freeze(
            scope.clone(),
            vec![PathBuf::from("file.txt")],
            "frozen".into(),
            id,
            operation(),
        )
        .await
        .unwrap()
    else {
        panic!("pending task");
    };
    let permits = crate::workspace::blocking_permits();
    let held = permits
        .clone()
        .try_acquire_many_owned(permits.available_permits().try_into().unwrap())
        .unwrap();
    tokio::time::pause();
    // Tokio deadlines round up to the next millisecond timer-wheel tick.
    let start = tokio::time::Instant::now();
    let expired = operation();
    expired.cancellation.cancel();
    assert!(matches!(
        broker.install(id, expired).await,
        Err(TaskGitError::Stopped)
    ));
    assert!(tokio::time::Instant::now() - start <= Duration::from_millis(2001));
    let start = tokio::time::Instant::now();
    let mut expired = operation();
    expired.deadline = Instant::now();
    assert!(matches!(
        broker
            .freeze(
                scope,
                vec![PathBuf::from("file.txt")],
                "frozen".into(),
                id,
                expired
            )
            .await,
        Err(TaskGitError::Stopped)
    ));
    assert!(tokio::time::Instant::now() - start <= Duration::from_millis(2001));
    drop(held);
    tokio::time::resume();
    assert_eq!(
        git(&broker.repository.path, &["rev-parse", &broker.branch]),
        broker.base
    );
    broker.install(id, operation()).await.unwrap();
}

#[tokio::test]
async fn task_git_refuses_source_object_alternates_without_importing() {
    let fixture = Fixture::new();
    let outside = fixture._root.path().join("outside-objects");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("private-canary"), "private").unwrap();
    let objects = fixture.source.join(".git/objects");
    std::fs::write(
        objects.join("info/alternates"),
        format!("{}\n", outside.display()),
    )
    .unwrap();
    let result = TaskGit::prepare(
        git_program(),
        TaskGitRoots {
            control: NativeDirectoryGrant::capture(&fixture.control).unwrap(),
            source: NativeDirectoryGrant::capture(&fixture.source).unwrap(),
            task: NativeDirectoryGrant::capture(&fixture.snapshot).unwrap(),
        },
        qq_protocol::RunId::generate().unwrap(),
        limits(),
        operation(),
    )
    .await;
    assert!(matches!(
        result,
        Err(TaskGitError::Invalid(
            "source object alternates are unsupported"
        ))
    ));
    assert_eq!(
        std::fs::read(outside.join("private-canary")).unwrap(),
        b"private"
    );
}

#[cfg(unix)]
#[test]
fn task_git_refuses_object_aliases_and_even_empty_alternates() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let objects = root.path().join("objects");
    let outside = root.path().join("private");
    std::fs::create_dir(&objects).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(objects.join("info")).unwrap();
    let grant = NativeDirectoryGrant::capture(&objects).unwrap();
    for target in [Some(outside.as_path()), None] {
        let alternates = objects.join("info/alternates");
        match target {
            Some(path) => symlink(path, &alternates).unwrap(),
            None => std::fs::write(&alternates, b"").unwrap(),
        }
        assert!(objects::validate(&grant, 100, &operation()).is_err());
        std::fs::remove_file(alternates).unwrap();
    }
    for alias in ["info", "pack", "ab"] {
        if alias == "info" {
            std::fs::remove_dir(objects.join(alias)).unwrap();
        }
        symlink(&outside, objects.join(alias)).unwrap();
        assert!(objects::validate(&grant, 100, &operation()).is_err());
        std::fs::remove_file(objects.join(alias)).unwrap();
    }
    std::fs::create_dir(objects.join("ab")).unwrap();
    let private = outside.join("secret");
    std::fs::write(&private, "secret").unwrap();
    symlink(&private, objects.join("ab/object")).unwrap();
    assert!(objects::validate(&grant, 100, &operation()).is_err());
    std::fs::remove_file(objects.join("ab/object")).unwrap();
    std::fs::hard_link(&private, objects.join("ab/object")).unwrap();
    assert!(objects::validate(&grant, 100, &operation()).is_err());
}
