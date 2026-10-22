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
