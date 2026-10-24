use super::*;
use crate::command_owner::{
    NativeCommandScope, NativeDirectoryGrant, NativeNetwork, NativeReadAccess, NativeResources,
};
use serde_json::json;

async fn run(
    workspace: &Workspace,
    state: &Arc<FileState>,
    access: NativeReadAccess,
    name: &str,
    arguments: String,
) -> ToolOutput {
    let scopes = crate::sessions::native_scope::NativeScopes::single(NativeCommandScope {
        workspace: NativeDirectoryGrant::capture(workspace.path()).unwrap(),
        read_access: access,
        read_only: vec![],
        network: NativeNetwork::Host,
        resources: NativeResources::Host,
    })
    .unwrap();
    execute_with_images(
        None,
        workspace.clone(),
        Arc::clone(state),
        name.to_owned(),
        arguments,
        RunCancellation::new(),
        None,
        ToolTasks::default().with_native_scopes(Some(scopes)),
        Arc::default(),
        Arc::default(),
        Arc::default(),
        Arc::default(),
        None,
    )
    .await
}

#[tokio::test]
async fn host_read_dispatch_reads_external_files_but_never_writes_them() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let inside = root.join("workspace");
    let outside = root.join("outside");
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("message.txt"), "external needle\n").unwrap();
    let workspace = Workspace::open(&inside).unwrap();
    let state = Arc::new(FileState::default());
    for access in [NativeReadAccess::Host, NativeReadAccess::Granted] {
        for path in [
            outside.join("message.txt").to_string_lossy().into_owned(),
            "../outside/message.txt".to_owned(),
        ] {
            let result = run(
                &workspace,
                &state,
                access,
                "read_file",
                json!({"path":path}).to_string(),
            )
            .await;
            assert_eq!(
                result.is_error,
                access == NativeReadAccess::Granted,
                "{}",
                result.model_text
            );
            if access == NativeReadAccess::Host {
                assert!(result.model_text.contains("external needle"));
                assert!(result.file_states.is_empty());
            }
        }
        for (name, args) in [
            ("search", json!({"path":outside,"query":"needle"})),
            ("tree", json!({"path":outside})),
        ] {
            let result = run(&workspace, &state, access, name, args.to_string()).await;
            assert_eq!(
                result.is_error,
                access == NativeReadAccess::Granted,
                "{}",
                result.model_text
            );
            if access == NativeReadAccess::Host {
                assert!(result.model_text.contains("message.txt"));
            }
        }
        let result = run(
            &workspace,
            &state,
            access,
            "write_file",
            json!({"path":"../outside/message.txt","content":"changed"}).to_string(),
        )
        .await;
        assert!(result.is_error);
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("message.txt")).unwrap(),
        "external needle\n"
    );
    assert!(state.recorded("message.txt").is_none());
}

#[tokio::test]
async fn host_external_reads_preserve_workspace_edit_hashes_and_argument_validation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let inside = root.join("workspace");
    std::fs::create_dir(&inside).unwrap();
    std::fs::write(inside.join("same.txt"), "inside\n").unwrap();
    std::fs::write(root.join("same.txt"), "outside\n").unwrap();
    let workspace = Workspace::open(&inside).unwrap();
    let state = Arc::new(FileState::default());
    for path in [
        "same.txt".to_owned(),
        inside.join("same.txt").to_string_lossy().into_owned(),
    ] {
        let read = run(
            &workspace,
            &state,
            NativeReadAccess::Host,
            "read_file",
            json!({"path":path}).to_string(),
        )
        .await;
        assert!(!read.is_error, "{}", read.model_text);
        assert!(!read.file_states.is_empty());
    }
    let saved = state.recorded("same.txt").unwrap();
    let read = run(
        &workspace,
        &state,
        NativeReadAccess::Host,
        "read_file",
        json!({"path":"../same.txt"}).to_string(),
    )
    .await;
    assert!(!read.is_error, "{}", read.model_text);
    assert!(read.file_states.is_empty());
    assert_eq!(state.recorded("same.txt").unwrap(), saved);
    for arguments in [
        r#"{"path":"../same.txt","path":"same.txt"}"#,
        r#"{"path":"../same.txt","unknown":true}"#,
        r#"{"path":"../same.txt","ranges":false}"#,
    ] {
        assert!(
            run(
                &workspace,
                &state,
                NativeReadAccess::Host,
                "read_file",
                arguments.to_owned()
            )
            .await
            .is_error
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn host_read_dispatch_resolves_external_symlinks_without_granting_writes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let inside = root.join("workspace");
    std::fs::create_dir(&inside).unwrap();
    let external = root.join("outside.txt");
    std::fs::write(&external, "symlink target\n").unwrap();
    std::os::unix::fs::symlink(&external, inside.join("alias.txt")).unwrap();
    let workspace = Workspace::open(&inside).unwrap();
    let state = Arc::new(FileState::default());
    for access in [NativeReadAccess::Host, NativeReadAccess::Granted] {
        let result = run(
            &workspace,
            &state,
            access,
            "read_file",
            json!({"path":"alias.txt"}).to_string(),
        )
        .await;
        assert_eq!(
            result.is_error,
            access == NativeReadAccess::Granted,
            "{}",
            result.model_text
        );
    }
    let write = run(
        &workspace,
        &state,
        NativeReadAccess::Host,
        "write_file",
        json!({"path":"alias.txt","content":"changed"}).to_string(),
    )
    .await;
    assert!(write.is_error);
    assert_eq!(
        std::fs::read_to_string(&external).unwrap(),
        "symlink target\n"
    );
}
