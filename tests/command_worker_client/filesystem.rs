//! Exercise kernel file restrictions through the actual independent worker.
use super::{CommandStatus, NativeCommand, NativeCommandService, NativeEnvironment, client};
use qq_core::command_owner::{NativeDirectoryGrant, NativeFilesystem, NativeWorkspaceIdentity};
use qq_protocol::ToolCallId;
use std::path::Path;

pub(super) fn request(workspace: &Path, tool: &str, script: &str) -> NativeCommand {
    let workspace = workspace.canonicalize().unwrap();
    let mut roots = Vec::new();
    // Explicit fixture toolchain inputs, not production implicit grants.
    for path in ["/bin", "/usr", "/lib", "/lib64", "/nix/store"] {
        if let Ok(path) = Path::new(path).canonicalize()
            && !roots.contains(&path)
        {
            roots.push(path);
        }
    }
    NativeCommand {
        environment: NativeEnvironment::capture(&[]).unwrap(),
        network: qq_core::command_owner::NativeNetwork::Host,
        resources: qq_core::command_owner::NativeResources::Host,
        filesystem: NativeFilesystem::Restricted {
            read_only: roots
                .iter()
                .map(|path| NativeDirectoryGrant::capture(path).unwrap())
                .collect(),
            read_write: vec![NativeDirectoryGrant::capture(&workspace).unwrap()],
        },
        id: ToolCallId::generate().unwrap(),
        workspace_identity: NativeWorkspaceIdentity::capture(&workspace).unwrap(),
        workspace,
        tool: tool.into(),
        arguments: if tool == "shell" {
            serde_json::json!({"command":script})
        } else {
            serde_json::json!({"program":"sh", "args":["-c",script]})
        }
        .to_string(),
        shell_policy: Default::default(),
        approved_images: Vec::new(),
        not_after_ms: None,
    }
}

pub(super) async fn finish(
    client: &impl NativeCommandService,
    request: NativeCommand,
) -> qq_core::command_owner::CommandSnapshot {
    let id = request.id;
    client.submit(request).await.unwrap();
    let mut after = 0;
    loop {
        let snapshot = client.wait(id, after).await.unwrap();
        if snapshot.status == CommandStatus::Finished {
            return snapshot;
        }
        assert_ne!(snapshot.status, CommandStatus::Lost);
        after = snapshot.version;
    }
}

pub(super) fn worker(path: &Path) -> client::CommandWorker {
    client::CommandWorker::new(
        path.to_owned(),
        env!("CARGO_BIN_EXE_qq").into(),
        concat!(
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("QQ_BUILD_REVISION"),
            ".",
            env!("QQ_BUILD_DATE")
        ),
    )
    .unwrap()
}

#[tokio::test]
async fn detached_task_workspace_confines_edits_without_modifying_source_inputs() {
    use qq_core::task_workspace::{TaskWorkspace, TaskWorkspaceLimits, TaskWorkspaceSource};
    let (root, source, detached) = tokio::task::spawn_blocking(|| {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let source = path.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("input"), "dirty input").unwrap();
        let source_runtime = qq_core::Runtime::new(
            qq_provider::test_support::ScriptedProvider::new(vec![]),
            "demo",
            256,
        )
        .unwrap();
        let source_plan = qq_core::plan::CompiledAgentPlan::compile_blocking(
            qq_core::plan::AgentProfile::embedded(&source_runtime, source.clone()),
        )
        .unwrap();
        let detached = TaskWorkspace::import(
            &TaskWorkspaceSource::from_plan(&source_plan).unwrap(),
            &path.join("task"),
            &["input".into()],
            TaskWorkspaceLimits {
                max_entries: 16,
                max_bytes: 1024,
            },
        )
        .unwrap();
        (root, source, detached)
    })
    .await
    .unwrap();
    let client = worker(&root.path().join("owner"));
    let script = format!(
        "cat input; printf task-edit > input; cat '{}'; printf escaped > '{}'; printf finished",
        source.join("input").display(),
        source.join("escape").display(),
    );
    let result = finish(&client, request(detached.path(), "shell", &script))
        .await
        .result
        .unwrap();
    client.stop_spawned().await;
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            std::fs::read_to_string(source.join("input")).unwrap(),
            "dirty input"
        );
        assert!(!source.join("escape").exists());
        if result.is_error() {
            assert!(
                result
                    .text()
                    .contains("required Linux filesystem enforcement is unavailable"),
                "{}",
                result.text()
            );
            assert_eq!(
                std::fs::read_to_string(detached.path().join("input")).unwrap(),
                "dirty input"
            );
        } else {
            assert!(result.text().contains("dirty input"), "{}", result.text());
            assert!(result.text().contains("finished"), "{}", result.text());
            assert_eq!(
                std::fs::read_to_string(detached.path().join("input")).unwrap(),
                "task-edit"
            );
        }
        drop(root);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn filesystem_scope_confines_shells_and_descendants_without_restricting_worker() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(root.path().join("host-secret"), "host-only-secret").unwrap();
    std::os::unix::fs::symlink("../host-secret", workspace.join("secret-link")).unwrap();
    let input = root.path().join("read-only-input");
    std::fs::create_dir(&input).unwrap();
    std::fs::write(input.join("value"), "explicit-readable-input").unwrap();
    std::os::unix::fs::symlink(&input, workspace.join("input")).unwrap();
    let client = worker(&root.path().join("owner"));
    let mut observations = Vec::new();
    for tool in ["shell", "exec"] {
        let mut command = request(
            &workspace,
            tool,
            "cat input/value; printf changed > input/value; printf allowed > allowed; printf escaped > ../outside; cat ../host-secret; cat secret-link; sh -c 'printf escaped > ../descendant-outside; cat ../host-secret'; printf finished",
        );
        if let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem {
            read_only.push(NativeDirectoryGrant::capture(&input).unwrap());
        }
        let snapshot = finish(&client, command).await;
        observations.push(snapshot.result.unwrap());
    }
    // Complete worker cleanup before checking containment, including in the
    // deliberately failing negative-control run.
    let confined_outside = root.path().join("outside").exists();
    let confined_descendant = root.path().join("descendant-outside").exists();
    // Applying the rules in the coordinator or worker would break this request.
    let mut unrestricted = request(
        &workspace,
        "shell",
        "cat ../host-secret; printf host > ../outside",
    );
    unrestricted.filesystem = NativeFilesystem::Host;
    let result = finish(&client, unrestricted).await.result.unwrap();
    client.stop_spawned().await;
    assert!(
        !confined_outside,
        "a confined shell wrote outside its grants"
    );
    assert!(
        !confined_descendant,
        "a descendant wrote outside its grants"
    );
    for observed in observations {
        if observed.is_error() {
            assert!(
                observed
                    .text()
                    .contains("required Linux filesystem enforcement is unavailable"),
                "{}",
                observed.text()
            );
            assert!(!workspace.join("allowed").exists());
        } else {
            assert!(
                observed.text().contains("explicit-readable-input"),
                "{}",
                observed.text()
            );
            assert!(observed.text().contains("finished"), "{}", observed.text());
            assert!(
                !observed.text().contains("host-only-secret"),
                "{}",
                observed.text()
            );
            assert_eq!(
                std::fs::read_to_string(workspace.join("allowed")).unwrap(),
                "allowed"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(input.join("value")).unwrap(),
        "explicit-readable-input"
    );
    assert!(!result.is_error(), "{}", result.text());
    assert!(result.text().contains("host-only-secret"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("outside")).unwrap(),
        "host"
    );
}

#[tokio::test]
async fn filesystem_scope_rejects_replaced_grants_and_changed_authority_on_retry() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let input = root.path().join("input");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&input).unwrap();
    let client = worker(&root.path().join("owner"));
    let mut command = request(&workspace, "shell", "printf payload > ran");
    if let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem {
        read_only.push(NativeDirectoryGrant::capture(&input).unwrap());
    }
    std::fs::rename(&input, root.path().join("old-input")).unwrap();
    std::fs::create_dir(&input).unwrap();
    let result = finish(&client, command.clone()).await.result.unwrap();
    let mut missing = serde_json::to_value(&command).unwrap();
    missing.as_object_mut().unwrap().remove("filesystem");
    assert!(
        serde_json::from_value::<NativeCommand>(missing).is_err(),
        "missing authority must never deserialize as unrestricted host access"
    );
    command.filesystem = NativeFilesystem::Host;
    let retry = client.submit(command).await;
    client.stop_spawned().await;
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result
            .text()
            .contains("authorized filesystem directory was replaced"),
        "{}",
        result.text()
    );
    assert!(!workspace.join("ran").exists());
    assert!(matches!(retry, Err(super::CommandOwnerError::Conflict)));
}

#[tokio::test]
async fn filesystem_scope_denies_metadata_changes_outside_writable_grants() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let input = root.path().join("read-only-input");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&input).unwrap();
    for path in [root.path().join("outside"), input.join("value")] {
        std::fs::write(&path, "retained").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::os::unix::fs::symlink("../outside", workspace.join("outside-link")).unwrap();
    std::os::unix::fs::symlink(&input, workspace.join("input")).unwrap();
    let client = worker(&root.path().join("owner"));
    let mut observations = Vec::new();
    for tool in ["shell", "exec"] {
        let mut command = request(
            &workspace,
            tool,
            "chmod 644 ../outside; chmod 644 outside-link; chmod 644 input/value; printf allowed > allowed; chmod 640 allowed; printf metadata-probe-finished",
        );
        if let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem {
            read_only.push(NativeDirectoryGrant::capture(&input).unwrap());
        }
        observations.push(finish(&client, command).await.result.unwrap());
    }
    client.stop_spawned().await;
    for result in observations {
        if result.is_error() {
            assert!(
                result
                    .text()
                    .contains("required Linux filesystem enforcement is unavailable"),
                "{}",
                result.text()
            );
        } else {
            assert!(
                result.text().contains("metadata-probe-finished"),
                "{}",
                result.text()
            );
            assert_eq!(
                std::fs::metadata(workspace.join("allowed"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
    }
    assert_eq!(
        std::fs::metadata(root.path().join("outside"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "a confined command changed host metadata"
    );
    assert_eq!(
        std::fs::metadata(input.join("value"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "a confined command changed read-only input metadata"
    );
}

#[test]
fn capabilities_probe() {
    if !std::env::args().any(|argument| argument == "qq-fs-capabilities") {
        return;
    }
    let caps = rustix::thread::capabilities(None).unwrap();
    assert!(caps.effective.is_empty());
    assert!(caps.permitted.is_empty());
    assert!(caps.inheritable.is_empty());
    assert!(rustix::thread::no_new_privs().unwrap());
    println!("payload-caps-empty");
    let nested = std::process::Command::new("unshare")
        .args(["--user", "--map-current-user", "--", "true"])
        .output()
        .unwrap();
    println!("nested-userns={}", nested.status.success());
}

#[tokio::test]
async fn filesystem_scope_drops_setup_capabilities_and_prevents_namespace_reentry() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut command = request(&workspace, "exec", "unused");
    command.arguments = serde_json::json!({"program": executable, "args":["--exact", "filesystem::capabilities_probe", "--nocapture", "--skip", "qq-fs-capabilities"]}).to_string();
    if let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem {
        read_only.push(NativeDirectoryGrant::capture(executable.parent().unwrap()).unwrap());
    }
    let client = worker(&root.path().join("owner"));
    let scoped = finish(&client, command.clone()).await.result.unwrap();
    command.id = ToolCallId::generate().unwrap();
    command.filesystem = NativeFilesystem::Host;
    let control = finish(&client, command).await.result.unwrap();
    client.stop_spawned().await;
    assert!(!control.is_error(), "{}", control.text());
    assert!(
        control.text().contains("payload-caps-empty"),
        "{}",
        control.text()
    );
    assert!(
        control.text().contains("nested-userns=true"),
        "{}",
        control.text()
    );
    if scoped.is_error() {
        assert!(
            scoped
                .text()
                .contains("required Linux filesystem enforcement is unavailable"),
            "{}",
            scoped.text()
        );
    } else {
        assert!(
            scoped.text().contains("payload-caps-empty"),
            "{}",
            scoped.text()
        );
        assert!(
            scoped.text().contains("nested-userns=false"),
            "{}",
            scoped.text()
        );
    }
}

#[tokio::test]
async fn filesystem_scope_reports_helper_setup_refusal_without_running_payload() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut command = request(&workspace, "shell", "printf forbidden > ran");
    // The helper has a fresh procfs for its PID namespace. A host-procfs grant
    // must not silently bind that host filesystem into the command's root.
    if let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem {
        read_only.push(NativeDirectoryGrant::capture(Path::new("/proc")).unwrap());
    }
    let client = worker(&root.path().join("owner"));
    let result = finish(&client, command).await.result.unwrap();
    client.stop_spawned().await;
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("private filesystem setup failed:")
            || result
                .text()
                .contains("required Linux filesystem enforcement is unavailable"),
        "{}",
        result.text()
    );
    assert!(!workspace.join("ran").exists());
}

#[tokio::test]
async fn filesystem_scope_preserves_writable_children_and_nested_working_directories() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let inputs = root.path().join("inputs");
    let workspace = inputs.join("workspace");
    std::fs::create_dir_all(workspace.join("nested")).unwrap();
    std::fs::write(inputs.join("value"), "read-only-input").unwrap();
    std::fs::set_permissions(inputs.join("value"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut command = request(&workspace, "shell", "unused");
    command.arguments = serde_json::json!({"command":"cat ../../value; printf forbidden > ../../value; chmod 644 ../../value; printf permitted > artifact; printf nested-finished", "cwd":"nested"}).to_string();
    if let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem {
        read_only.push(NativeDirectoryGrant::capture(&inputs).unwrap());
    }
    let client = worker(&root.path().join("owner"));
    let result = finish(&client, command).await.result.unwrap();
    client.stop_spawned().await;
    if result.is_error() {
        assert!(
            result
                .text()
                .contains("required Linux filesystem enforcement is unavailable"),
            "{}",
            result.text()
        );
    } else {
        assert!(
            result.text().contains("read-only-input"),
            "{}",
            result.text()
        );
        assert!(
            result.text().contains("nested-finished"),
            "{}",
            result.text()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("nested/artifact")).unwrap(),
            "permitted"
        );
    }
    assert_eq!(
        std::fs::read_to_string(inputs.join("value")).unwrap(),
        "read-only-input"
    );
    assert_eq!(
        std::fs::metadata(inputs.join("value"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn filesystem_scope_preserves_frozen_path_aliases_without_granting_their_parent() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let tools = workspace.join("tools");
    let alias = root.path().join("tool-profile");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(
        tools.join("private-probe"),
        "#!/bin/sh\nprintf path-worked > artifact\nprintf alias-finished\n",
    )
    .unwrap();
    std::fs::set_permissions(
        tools.join("private-probe"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink(&tools, &alias).unwrap();
    let mut command = request(&workspace, "exec", "unused");
    command.environment =
        NativeEnvironment::from_values([("PATH".to_owned(), alias.into_os_string())]).unwrap();
    command.arguments = serde_json::json!({"program":"private-probe", "args":[]}).to_string();
    let client = worker(&root.path().join("owner"));
    let result = finish(&client, command).await.result.unwrap();
    client.stop_spawned().await;
    if result.is_error() {
        assert!(
            result
                .text()
                .contains("required Linux filesystem enforcement is unavailable"),
            "{}",
            result.text()
        );
    } else {
        assert!(
            result.text().contains("alias-finished"),
            "{}",
            result.text()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("artifact")).unwrap(),
            "path-worked"
        );
    }
}

#[tokio::test]
async fn resource_scope_without_delegation_refuses_before_payload_and_cannot_be_widened() {
    use qq_core::command_owner::{NativeResourceLimits, NativeResources};
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut command = request(&workspace, "shell", "printf forbidden > ran");
    command.resources = NativeResources::Limited {
        limits: NativeResourceLimits {
            memory_bytes: 64 * 1024 * 1024,
            max_processes: 8,
            cpu_millicores: 250,
        },
    };
    let mut missing = serde_json::to_value(&command).unwrap();
    missing.as_object_mut().unwrap().remove("resources");
    assert!(serde_json::from_value::<NativeCommand>(missing).is_err());
    let client = worker(&root.path().join("owner"));
    let result = finish(&client, command.clone()).await.result.unwrap();
    command.resources = NativeResources::Host;
    let retry = client.submit(command).await;
    client.stop_spawned().await;
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("resource enforcement"),
        "{}",
        result.text()
    );
    assert!(!workspace.join("ran").exists());
    assert!(matches!(retry, Err(super::CommandOwnerError::Conflict)));
}
