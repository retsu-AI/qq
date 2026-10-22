use super::*;
use std::fs;

struct Fixture {
    _root: tempfile::TempDir,
    source: NativeDirectoryGrant,
    destination: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        fs::create_dir(path.join("source")).unwrap();
        let source = NativeDirectoryGrant::capture(&path.join("source")).unwrap();
        Self {
            _root: root,
            source,
            destination: path.join("task"),
        }
    }

    fn import(&self, inputs: &[&str], bytes: u64) -> Result<TaskWorkspace, TaskWorkspaceError> {
        TaskWorkspace::import(
            &self.source,
            &self.destination,
            &inputs.iter().map(PathBuf::from).collect::<Vec<_>>(),
            TaskWorkspaceLimits {
                max_entries: 16,
                max_bytes: bytes,
            },
        )
    }
}

#[test]
fn detached_task_workspace_preserves_selected_bytes_without_host_aliases() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.source.path.join("src")).unwrap();
    fs::write(fixture.source.path.join("src/dirty.rs"), "uncommitted").unwrap();
    fs::write(fixture.source.path.join("untracked"), "new").unwrap();
    fs::write(fixture.source.path.join("secret"), "not selected").unwrap();
    let imported = fixture.import(&["src/dirty.rs", "untracked"], 14).unwrap();
    assert_eq!(
        fs::read_to_string(imported.path().join("src/dirty.rs")).unwrap(),
        "uncommitted"
    );
    assert_eq!(
        fs::read_to_string(imported.path().join("untracked")).unwrap(),
        "new"
    );
    assert!(!imported.path().join("secret").exists());
    fs::write(imported.path().join("src/dirty.rs"), "agent change").unwrap();
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("src/dirty.rs")).unwrap(),
        "uncommitted"
    );
    let grant = imported.directory_grant();
    assert_eq!(
        grant.identity,
        NativeWorkspaceIdentity::capture(imported.path()).unwrap()
    );
    drop(imported);
    assert_eq!(
        fs::read_to_string(fixture.destination.join("src/dirty.rs")).unwrap(),
        "agent change"
    );
}

#[test]
fn publication_preserves_other_source_edits_and_can_resume_from_a_saved_record() {
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "original").unwrap();
    fs::write(fixture.source.path.join("b"), "original").unwrap();
    let task = fixture.import(&["a", "b"], 1024).unwrap();
    let saved = serde_json::to_vec(&task).unwrap();
    fs::write(task.path().join("a"), "agent change").unwrap();
    fs::write(task.path().join("new"), "created").unwrap();
    fs::write(fixture.source.path.join("b"), "human change").unwrap();
    let mut reopened: TaskWorkspace = serde_json::from_slice(&saved).unwrap();
    let paths = ["a", "b", "new"].map(PathBuf::from);
    assert_eq!(
        reopened.publish(&paths).unwrap(),
        [PathBuf::from("a"), PathBuf::from("new")]
    );
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "agent change"
    );
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("b")).unwrap(),
        "human change"
    );
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("new")).unwrap(),
        "created"
    );
    // A crash can leave installed bytes with the earlier saved baseline.
    let mut retried: TaskWorkspace = serde_json::from_slice(&saved).unwrap();
    assert_eq!(
        retried.publish(&paths).unwrap(),
        [PathBuf::from("a"), PathBuf::from("new")]
    );
    assert!(retried.publish(&paths).unwrap().is_empty());
}

#[tokio::test]
async fn publication_preserves_a_human_edit_made_while_staging() {
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "original").unwrap();
    let mut task = fixture.import(&["a"], 1024).unwrap();
    fs::write(task.path().join("a"), "agent change").unwrap();
    let (entered, release) = crate::workspace::hold_tool_apply(&fixture.source.path);
    let publication = tokio::task::spawn_blocking(move || task.publish(&[PathBuf::from("a")]));
    entered.await.unwrap();
    fs::write(fixture.source.path.join("a"), "human change").unwrap();
    release.send(()).unwrap();
    let result = publication.await.unwrap();
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "human change"
    );
    assert!(matches!(result, Err(TaskWorkspaceError::Conflict(path)) if path == Path::new("a")));
    assert!(!fs::read_dir(&fixture.source.path).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".qq-apply-")
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn publication_preserves_a_human_permission_change_while_staging() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "original").unwrap();
    fs::set_permissions(
        fixture.source.path.join("a"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut task = fixture.import(&["a"], 1024).unwrap();
    fs::write(task.path().join("a"), "agent change").unwrap();
    let (entered, release) = crate::workspace::hold_tool_apply(&fixture.source.path);
    let publication = tokio::task::spawn_blocking(move || task.publish(&[PathBuf::from("a")]));
    entered.await.unwrap();
    fs::set_permissions(
        fixture.source.path.join("a"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    release.send(()).unwrap();
    let result = publication.await.unwrap();
    assert_eq!(
        fs::metadata(fixture.source.path.join("a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(matches!(result, Err(TaskWorkspaceError::Conflict(path)) if path == Path::new("a")));
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "original"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn publication_stages_private_bytes_in_a_private_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "private original").unwrap();
    fs::set_permissions(
        fixture.source.path.join("a"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut task = fixture.import(&["a"], 1024).unwrap();
    fs::write(task.path().join("a"), "private agent change").unwrap();
    let (entered, release) = crate::workspace::atomic_write::hold_tool_create(&fixture.source.path);
    let publication = tokio::task::spawn_blocking(move || task.publish(&[PathBuf::from("a")]));
    entered.await.unwrap();
    let staged = fs::read_dir(&fixture.source.path)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".qq-apply-")
        })
        .unwrap();
    let mode = staged.metadata().unwrap().permissions().mode() & 0o777;
    release.send(()).unwrap();
    publication.await.unwrap().unwrap();
    assert_eq!(mode & 0o077, 0, "staging exposed private bytes");
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "private agent change"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn publication_preserves_windows_private_access() {
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::{GetSecurityInfo, SetSecurityInfo},
    };
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "private original").unwrap();
    let user = win_custody::current_user_sid_string().unwrap();
    let security: windows_permissions::LocalBox<windows_permissions::SecurityDescriptor> =
        format!("D:P(A;;FA;;;{user})").parse().unwrap();
    use std::os::windows::fs::OpenOptionsExt as _;
    let mut source = fs::OpenOptions::new()
        .access_mode(0x0006_0000)
        .open(fixture.source.path.join("a"))
        .unwrap();
    SetSecurityInfo(
        &mut source,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        security.dacl(),
        None,
    )
    .unwrap();
    drop(source);
    let mut task = fixture.import(&["a"], 1024).unwrap();
    fs::write(task.path().join("a"), "private agent change").unwrap();
    let (entered, release) = crate::workspace::atomic_write::hold_tool_create(&fixture.source.path);
    let publication = tokio::task::spawn_blocking(move || task.publish(&[PathBuf::from("a")]));
    entered.await.unwrap();
    let staged = fs::read_dir(&fixture.source.path)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".qq-apply-")
        })
        .unwrap();
    let exposed = fs::File::open(staged.path());
    release.send(()).unwrap();
    publication.await.unwrap().unwrap();
    assert!(
        exposed.is_err(),
        "staged bytes were openable before securing the DACL"
    );
    let file = fs::File::open(fixture.source.path.join("a")).unwrap();
    let actual = GetSecurityInfo(
        &file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .unwrap();
    assert_eq!(actual.as_sddl().unwrap(), security.as_sddl().unwrap());
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "private agent change"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn publication_preserves_windows_access_change_while_staging() {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::{GetSecurityInfo, SetSecurityInfo},
    };
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "original").unwrap();
    let mut source = fs::OpenOptions::new()
        .access_mode(0x0006_0000)
        .open(fixture.source.path.join("a"))
        .unwrap();
    let original = GetSecurityInfo(
        &source,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .unwrap();
    let user = win_custody::current_user_sid_string().unwrap();
    let edited: windows_permissions::LocalBox<windows_permissions::SecurityDescriptor> =
        format!("D:P(A;;FR;;;{user})").parse().unwrap();
    let mut task = fixture.import(&["a"], 1024).unwrap();
    fs::write(task.path().join("a"), "agent change").unwrap();
    let (entered, release) = crate::workspace::hold_tool_apply(&fixture.source.path);
    let publication = tokio::task::spawn_blocking(move || task.publish(&[PathBuf::from("a")]));
    entered.await.unwrap();
    SetSecurityInfo(
        &mut source,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        edited.dacl(),
        None,
    )
    .unwrap();
    release.send(()).unwrap();
    let result = publication.await.unwrap();
    let actual = GetSecurityInfo(
        &source,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .unwrap();
    // Restore fixture access even when an assertion below fails.
    SetSecurityInfo(
        &mut source,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        original.dacl(),
        None,
    )
    .unwrap();
    drop(source);
    assert_eq!(actual.as_sddl().unwrap(), edited.as_sddl().unwrap());
    assert!(matches!(result, Err(TaskWorkspaceError::Conflict(path)) if path == Path::new("a")));
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "original"
    );
}

#[test]
fn publication_preflights_conflicts_before_installing_any_output() {
    let fixture = Fixture::new();
    for name in ["a", "z"] {
        fs::write(fixture.source.path.join(name), "original").unwrap();
    }
    let mut task = fixture.import(&["a", "z"], 1024).unwrap();
    for name in ["a", "z"] {
        fs::write(task.path().join(name), "agent change").unwrap();
    }
    fs::write(fixture.source.path.join("z"), "human change").unwrap();
    assert!(
        matches!(task.publish(&["a", "z"].map(PathBuf::from)), Err(TaskWorkspaceError::Conflict(path)) if path == Path::new("z"))
    );
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("a")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(fixture.source.path.join("z")).unwrap(),
        "human change"
    );
}

#[test]
fn publication_handles_deletion_without_deleting_changed_source_bytes() {
    for conflict in [false, true] {
        let fixture = Fixture::new();
        fs::write(fixture.source.path.join("a"), "original").unwrap();
        let mut task = fixture.import(&["a"], 1024).unwrap();
        fs::remove_file(task.path().join("a")).unwrap();
        if conflict {
            fs::write(fixture.source.path.join("a"), "human change").unwrap();
        }
        let result = task.publish(&[PathBuf::from("a")]);
        if conflict {
            assert!(matches!(result, Err(TaskWorkspaceError::Conflict(_))));
            assert_eq!(
                fs::read_to_string(fixture.source.path.join("a")).unwrap(),
                "human change"
            );
        } else {
            assert_eq!(result.unwrap(), [PathBuf::from("a")]);
            assert!(!fixture.source.path.join("a").exists());
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn publication_refuses_task_and_source_aliases_before_changing_files() {
    #[cfg(unix)]
    let aliases: &[&str] = &["symlink", "hardlink", "symlink-parent"];
    #[cfg(windows)]
    let aliases: &[&str] = &["hardlink", "junction-parent"];
    for side in ["task", "source"] {
        for &alias in aliases {
            let parent_alias = matches!(alias, "symlink-parent" | "junction-parent");
            let fixture = Fixture::new();
            fs::write(fixture.source.path.join("a"), "original").unwrap();
            let selected = if parent_alias {
                fs::create_dir(fixture.source.path.join("z")).unwrap();
                "z/host"
            } else {
                "z"
            };
            fs::write(fixture.source.path.join(selected), "original").unwrap();
            let mut task = fixture.import(&["a", selected], 1024).unwrap();
            fs::write(task.path().join("a"), "agent change").unwrap();
            fs::write(task.path().join(selected), "agent change").unwrap();
            let outside = fixture.source.path.parent().unwrap().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("host"), "host bytes").unwrap();
            let root = if side == "task" {
                task.path()
            } else {
                &fixture.source.path
            };
            if parent_alias {
                fs::remove_dir_all(root.join("z")).unwrap();
                #[cfg(unix)]
                std::os::unix::fs::symlink(&outside, root.join("z")).unwrap();
                #[cfg(windows)]
                assert!(
                    std::process::Command::new("cmd")
                        .args(["/d", "/c", "mklink", "/J"])
                        .arg(root.join("z"))
                        .arg(&outside)
                        .stdout(std::process::Stdio::null())
                        .status()
                        .unwrap()
                        .success()
                );
            } else {
                fs::remove_file(root.join("z")).unwrap();
                match alias {
                    "hardlink" => fs::hard_link(outside.join("host"), root.join("z")).unwrap(),
                    #[cfg(unix)]
                    "symlink" => {
                        std::os::unix::fs::symlink(outside.join("host"), root.join("z")).unwrap()
                    }
                    _ => unreachable!("unknown alias fixture"),
                }
            }
            assert!(
                task.publish(&[PathBuf::from("a"), PathBuf::from(selected)])
                    .is_err(),
                "{side} {alias}"
            );
            assert_eq!(
                fs::read_to_string(fixture.source.path.join("a")).unwrap(),
                "original",
                "{side} {alias}"
            );
            assert_eq!(
                fs::read_to_string(outside.join("host")).unwrap(),
                "host bytes"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn publication_refuses_aliased_or_special_lock_files() {
    for alias in ["symlink", "hardlink", "fifo"] {
        let fixture = Fixture::new();
        fs::write(fixture.source.path.join("a"), "original").unwrap();
        let mut task = fixture.import(&["a"], 1024).unwrap();
        fs::write(task.path().join("a"), "agent change").unwrap();
        let outside = fixture.source.path.parent().unwrap().join("outside");
        fs::write(&outside, "host bytes").unwrap();
        fs::create_dir(fixture.source.path.join(".qq")).unwrap();
        let lock = fixture.source.path.join(".qq/task-publication.lock");
        match alias {
            "symlink" => std::os::unix::fs::symlink(&outside, &lock).unwrap(),
            "hardlink" => fs::hard_link(&outside, &lock).unwrap(),
            "fifo" => assert!(
                std::process::Command::new("mkfifo")
                    .arg(&lock)
                    .status()
                    .unwrap()
                    .success()
            ),
            _ => unreachable!(),
        }
        assert!(matches!(
            task.publish(&[PathBuf::from("a")]),
            Err(TaskWorkspaceError::UnsupportedFile(_))
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.path.join("a")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "host bytes");
    }
}

#[tokio::test]
async fn publication_reports_partial_failure_and_retries_the_saved_baseline() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.source.path.join("nested")).unwrap();
    for name in ["a", "nested/z"] {
        fs::write(fixture.source.path.join(name), "original").unwrap();
    }
    let mut task = fixture.import(&["a", "nested/z"], 1024).unwrap();
    let saved = serde_json::to_vec(&task).unwrap();
    for name in ["a", "nested/z"] {
        fs::write(task.path().join(name), "agent change").unwrap();
    }
    let paths = ["a", "nested/z"].map(PathBuf::from);
    let pending_paths = paths.clone();
    let (entered, release) = crate::workspace::hold_tool_apply(&fixture.source.path);
    let publishing = tokio::task::spawn_blocking(move || task.publish(&pending_paths));
    tokio::time::timeout(std::time::Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let lock = fixture.source.path.join(".qq/task-publication.lock");
    let probe = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "task_workspace::tests::publication_lock_probe",
                "--nocapture",
            ])
            .env("QQ_TEST_PUBLICATION_LOCK", lock)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        String::from_utf8_lossy(&probe.stdout).contains("1 passed"),
        "the cross-process lock probe must execute"
    );
    // Another writer changes the later target after preflight, while the
    // first atomic apply is staged. This works without permission tricks.
    fs::remove_file(fixture.source.path.join("nested/z")).unwrap();
    fs::create_dir(fixture.source.path.join("nested/z")).unwrap();
    release.send(()).unwrap();
    let failed = publishing.await.unwrap();
    assert!(
        matches!(failed, Err(TaskWorkspaceError::PartialPublication { applied, .. }) if applied == [PathBuf::from("a")])
    );
    fs::remove_dir(fixture.source.path.join("nested/z")).unwrap();
    fs::write(fixture.source.path.join("nested/z"), "original").unwrap();
    let mut reopened: TaskWorkspace = serde_json::from_slice(&saved).unwrap();
    assert_eq!(reopened.publish(&paths).unwrap(), paths);
    for name in ["a", "nested/z"] {
        assert_eq!(
            fs::read_to_string(fixture.source.path.join(name)).unwrap(),
            "agent change"
        );
    }
}

#[test]
fn publication_lock_probe() {
    let Some(path) = std::env::var_os("QQ_TEST_PUBLICATION_LOCK") else {
        return;
    };
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    assert!(
        matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "another process must not publish while the first holds the checkout lock"
    );
}

#[test]
fn publication_cannot_install_policy_or_git_trust_files() {
    for protected in [
        ".git/config",
        ".qq/config.ron",
        ".qq/task-publication.lock",
        "../outside",
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.source.path.join("a"), "original").unwrap();
        let mut task = fixture.import(&["a"], 1024).unwrap();
        fs::write(task.path().join("a"), "agent change").unwrap();
        assert!(matches!(
            task.publish(&[PathBuf::from("a"), PathBuf::from(protected)]),
            Err(TaskWorkspaceError::InvalidInput(_))
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.path.join("a")).unwrap(),
            "original"
        );
    }
}

#[test]
fn publication_bounds_output_bytes_and_retained_entries_before_applying() {
    for limit in ["bytes", "entries"] {
        let fixture = Fixture::new();
        fs::write(fixture.source.path.join("a"), "old").unwrap();
        let mut task = fixture.import(&["a"], 8).unwrap();
        fs::write(task.path().join("a"), "new").unwrap();
        let mut paths = vec![PathBuf::from("a")];
        if limit == "bytes" {
            fs::write(task.path().join("z"), "too large").unwrap();
            paths.push(PathBuf::from("z"));
        } else {
            // Repeated publication must not grow the saved baseline without
            // bound even when each individual request fits the entry budget.
            task.limits.max_entries = 1;
            fs::write(task.path().join("z"), "new").unwrap();
            paths = vec![PathBuf::from("z")];
        }
        assert!(matches!(
            task.publish(&paths),
            Err(TaskWorkspaceError::LimitExceeded)
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.path.join("a")).unwrap(),
            "old"
        );
        assert!(!fixture.source.path.join("z").exists());
    }
}

#[test]
fn publication_refuses_replaced_source_and_task_roots() {
    for side in ["source", "task"] {
        let fixture = Fixture::new();
        fs::write(fixture.source.path.join("a"), "original").unwrap();
        let mut task = fixture.import(&["a"], 1024).unwrap();
        fs::write(task.path().join("a"), "agent change").unwrap();
        let path = if side == "source" {
            &fixture.source.path
        } else {
            task.path()
        };
        let old = path.with_file_name("old");
        fs::rename(path, &old).unwrap();
        fs::create_dir(path).unwrap();
        fs::write(path.join("a"), "replacement").unwrap();
        assert!(matches!(
            task.publish(&[PathBuf::from("a")]),
            Err(TaskWorkspaceError::Changed(_))
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.path.join("a")).unwrap(),
            if side == "source" {
                "replacement"
            } else {
                "original"
            }
        );
        assert_eq!(
            fs::read_to_string(old.join("a")).unwrap(),
            if side == "source" {
                "original"
            } else {
                "agent change"
            }
        );
    }
}

#[test]
fn detached_task_workspace_rejects_hardlinks_and_cleans_partial_import() {
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "ordinary").unwrap();
    let external = fixture.source.path.parent().unwrap().join("external");
    fs::write(&external, "private host bytes").unwrap();
    fs::hard_link(&external, fixture.source.path.join("z")).unwrap();
    assert!(
        matches!(fixture.import(&["a", "z"], 1024), Err(TaskWorkspaceError::UnsupportedFile(path)) if path == Path::new("z"))
    );
    assert!(!fixture.destination.exists());
    assert_eq!(fs::read_to_string(external).unwrap(), "private host bytes");
    // Even selected internal aliases are refused, avoiding case-insensitive
    // duplicate names being mistaken for distinct hardlink names on other OSes.
    fs::hard_link(fixture.source.path.join("a"), fixture.source.path.join("b")).unwrap();
    assert!(matches!(
        fixture.import(&["a", "b"], 1024),
        Err(TaskWorkspaceError::UnsupportedFile(_))
    ));
    assert!(!fixture.destination.exists());
}

#[test]
fn detached_task_workspace_bounds_bytes_files_and_keeps_existing_destination() {
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "1234").unwrap();
    fs::write(fixture.source.path.join("b"), "5678").unwrap();
    assert!(matches!(
        fixture.import(&["a", "b"], 7),
        Err(TaskWorkspaceError::LimitExceeded)
    ));
    assert!(!fixture.destination.exists());
    assert!(matches!(
        TaskWorkspace::import(
            &fixture.source,
            &fixture.destination,
            &[PathBuf::from("a"), PathBuf::from("b")],
            TaskWorkspaceLimits {
                max_entries: 1,
                max_bytes: 8
            }
        ),
        Err(TaskWorkspaceError::LimitExceeded)
    ));
    assert!(matches!(
        TaskWorkspace::import(
            &fixture.source,
            &fixture.destination,
            &[PathBuf::from("a/b/c")],
            TaskWorkspaceLimits {
                max_entries: 2,
                max_bytes: 8
            }
        ),
        Err(TaskWorkspaceError::LimitExceeded)
    ));
    fs::create_dir(&fixture.destination).unwrap();
    fs::write(fixture.destination.join("existing"), "keep").unwrap();
    assert!(fixture.import(&["a"], 4).is_err());
    assert_eq!(
        fs::read_to_string(fixture.destination.join("existing")).unwrap(),
        "keep"
    );
}

#[cfg(windows)]
#[test]
fn detached_task_workspace_refuses_junctions_and_ambiguous_windows_names() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.source.path.join("real")).unwrap();
    fs::write(fixture.source.path.join("real/input"), "data").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(fixture.source.path.join("junction"))
        .arg(fixture.source.path.join("real"))
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "create native junction fixture");
    assert!(fixture.import(&["junction/input"], 1024).is_err());
    assert!(!fixture.destination.exists());
    for path in ["file:stream", "directory./file", "directory /file"] {
        assert!(matches!(
            fixture.import(&[path], 1024),
            Err(TaskWorkspaceError::InvalidInput(_))
        ));
        assert!(!fixture.destination.exists());
    }
}

#[test]
fn detached_task_workspace_rejects_escaping_paths_trust_inputs_and_replaced_roots() {
    for path in [
        "../outside",
        "/absolute",
        "",
        ".git/config",
        "qq.ron",
        ".qq/config.ron",
        ".qq/task-publication.lock",
        "nested/.git/HEAD",
    ] {
        let fixture = Fixture::new();
        assert!(fixture.import(&[path], 1024).is_err(), "{path}");
        assert!(!fixture.destination.exists());
    }
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("a"), "a").unwrap();
    assert!(matches!(
        fixture.import(&["a", "a"], 1024),
        Err(TaskWorkspaceError::InvalidInput(_))
    ));
    fs::rename(
        &fixture.source.path,
        fixture.source.path.with_file_name("old"),
    )
    .unwrap();
    fs::create_dir(&fixture.source.path).unwrap();
    assert!(matches!(
        fixture.import(&[], 1024),
        Err(TaskWorkspaceError::Changed(_))
    ));
    assert!(!fixture.destination.exists());
}

#[cfg(unix)]
#[test]
fn detached_task_workspace_refuses_links_special_files_and_preserves_executable_bits() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let fixture = Fixture::new();
    fs::write(fixture.source.path.join("script"), "exit 0\n").unwrap();
    fs::set_permissions(
        fixture.source.path.join("script"),
        fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    symlink("script", fixture.source.path.join("link")).unwrap();
    assert!(matches!(
        fixture.import(&["link"], 1024),
        Err(TaskWorkspaceError::UnsupportedFile(_))
    ));
    fs::create_dir(fixture.source.path.join("real")).unwrap();
    fs::write(fixture.source.path.join("real/input"), "data").unwrap();
    symlink("real", fixture.source.path.join("parent-link")).unwrap();
    assert!(fixture.import(&["parent-link/input"], 1024).is_err());
    assert!(
        std::process::Command::new("mkfifo")
            .arg(fixture.source.path.join("pipe"))
            .status()
            .unwrap()
            .success()
    );
    assert!(matches!(
        fixture.import(&["pipe"], 1024),
        Err(TaskWorkspaceError::UnsupportedFile(_))
    ));
    let workspace = fixture.import(&["script"], 1024).unwrap();
    assert_eq!(
        fs::metadata(workspace.path().join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o711
    );
    assert_eq!(
        fs::metadata(workspace.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[tokio::test]
async fn detached_task_workspace_is_shared_by_builtin_tools_and_native_commands() {
    use crate::{
        RunCancellation,
        command_owner::{
            CommandOwner, CommandStatus, NativeCommand, NativeEnvironment, NativeFilesystem,
            NativeNetwork, NativeResources,
        },
        tools::dispatch::{ToolTasks, execute},
        workspace::FileState,
    };
    use std::sync::Arc;
    let fixture = tokio::task::spawn_blocking(Fixture::new).await.unwrap();
    let source = fixture.source.clone();
    let destination = fixture.destination.clone();
    let detached = tokio::task::spawn_blocking(move || {
        fs::write(source.path.join("input"), "local edit").unwrap();
        TaskWorkspace::import(
            &source,
            &destination,
            &["input".into()],
            TaskWorkspaceLimits {
                max_entries: 16,
                max_bytes: 1024,
            },
        )
        .unwrap()
    })
    .await
    .unwrap();
    let path = detached.path().to_owned();
    let workspace = tokio::task::spawn_blocking(move || Workspace::open(&path).unwrap())
        .await
        .unwrap();
    let state = Arc::new(FileState::default());
    let tasks = ToolTasks::default();
    let builtin = |name: &str, arguments: &str| {
        execute(
            workspace.clone(),
            state.clone(),
            name.into(),
            arguments.into(),
            RunCancellation::new(),
            None,
            tasks.clone(),
            Arc::default(),
            Arc::default(),
        )
    };
    let written = builtin(
        "write_file",
        r#"{"path":"hello.txt","content":"builtin","create_only":true}"#,
    )
    .await;
    assert!(!written.is_error, "{}", written.model_text);
    let owner = CommandOwner::open(
        fixture.destination.with_file_name("commands"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    let grant = detached.directory_grant();
    let id = qq_protocol::ToolCallId::generate().unwrap();
    owner
        .submit(NativeCommand {
            environment: NativeEnvironment::capture(&[]).unwrap(),
            id,
            workspace: grant.path,
            workspace_identity: grant.identity,
            tool: "shell".into(),
            arguments: r#"{"command":"echo shell>>hello.txt"}"#.into(),
            shell_policy: Default::default(),
            approved_images: vec![],
            filesystem: NativeFilesystem::Host,
            network: NativeNetwork::Host,
            resources: NativeResources::Host,
            not_after_ms: None,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut after = 0;
        loop {
            let snapshot = owner.wait(id, after).await.unwrap();
            if snapshot.status == CommandStatus::Finished {
                break snapshot.result.unwrap();
            }
            assert_ne!(snapshot.status, CommandStatus::Lost);
            after = snapshot.version;
        }
    })
    .await
    .unwrap();
    owner.shutdown().await.unwrap();
    assert!(!result.is_error(), "{}", result.text());
    let read = builtin("read_file", r#"{"path":"hello.txt"}"#).await;
    assert!(!read.is_error, "{}", read.model_text);
    assert!(
        read.model_text.contains("builtinshell"),
        "{}",
        read.model_text
    );
    tasks.drain().await.unwrap();
    tokio::task::spawn_blocking(move || {
        assert!(!fixture.source.path.join("hello.txt").exists());
        assert_eq!(
            fs::read_to_string(fixture.source.path.join("input")).unwrap(),
            "local edit"
        );
        drop(fixture);
    })
    .await
    .unwrap();
}

#[test]
fn detached_task_workspace_rejects_filesystem_ancestor_aliases() {
    let fixture = Fixture::new();
    let nested = fixture.source.path.join("nested");
    fs::create_dir(&nested).unwrap();
    let held = Workspace::open(&nested).unwrap();
    assert!(contains_directory(held.root(), fixture.source.identity).unwrap());
    let sibling = Workspace::open(fixture.source.path.parent().unwrap()).unwrap();
    assert!(!contains_directory(sibling.root(), fixture.source.identity).unwrap());

    // Default macOS and Windows volumes resolve this alternate spelling to
    // the same source. Case-sensitive volumes exercise held ancestry above.
    let alternate = fixture.source.path.with_file_name("SOURCE");
    if !alternate.exists() {
        return;
    }
    let destination = alternate.join("nested/task");
    let result = TaskWorkspace::import(
        &fixture.source,
        &destination,
        &[],
        TaskWorkspaceLimits {
            max_entries: 16,
            max_bytes: 1024,
        },
    );
    assert!(matches!(
        result,
        Err(TaskWorkspaceError::InvalidInput(
            "source and destination overlap"
        ))
    ));
    assert!(!destination.exists());
    assert_eq!(fs::read_dir(&nested).unwrap().count(), 0);
}
