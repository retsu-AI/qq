//! Exercise kernel file restrictions through the actual independent worker.
use super::{CommandStatus, NativeCommand, NativeCommandService, NativeEnvironment, client};
use qq_core::command_owner::{NativeDirectoryGrant, NativeFilesystem, NativeWorkspaceIdentity};
use qq_protocol::ToolCallId;
use std::path::Path;

fn request(workspace: &Path, tool: &str, script: &str) -> NativeCommand {
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

async fn finish(
    client: &client::CommandWorker,
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

fn worker(path: &Path) -> client::CommandWorker {
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
