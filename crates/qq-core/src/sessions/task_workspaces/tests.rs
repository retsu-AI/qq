use super::*;

struct UnusedLoader;
impl RuntimeLoader for UnusedLoader {
    fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async { panic!("publication does not make a model request") })
    }
}

fn publication_fixture() -> (tempfile::TempDir, PathBuf, Arc<TaskExecution>) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let source = path.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("artifact"), "original").unwrap();
    let runtime = crate::Runtime::new(
        qq_provider::test_support::ScriptedProvider::new(vec![]),
        "demo",
        256,
    )
    .unwrap();
    let plan = crate::plan::CompiledAgentPlan::compile_blocking(
        crate::plan::AgentProfile::embedded(&runtime, source.clone()),
    )
    .unwrap();
    let workspace = TaskWorkspace::import(
        &TaskWorkspaceSource::from_plan(&plan).unwrap(),
        &path.join("task"),
        &["artifact".into()],
        TaskWorkspaceLimits {
            max_entries: 16,
            max_bytes: 1024,
        },
    )
    .unwrap();
    std::fs::write(workspace.path().join("artifact"), "agent edit").unwrap();
    let scope = NativeCommandScope {
        workspace: workspace.directory_grant(),
        read_only: vec![],
        network: NativeNetwork::Isolated,
        resources: NativeResources::Host,
    };
    let record = TaskRecord {
        source: NativeDirectoryGrant::capture(&source).unwrap(),
        workspace,
        scope: scope.clone(),
    };
    let execution = Arc::new(TaskExecution {
        owner: RunId::generate().unwrap(),
        publisher: true,
        scope,
        record: Mutex::new(record),
    });
    (root, source, execution)
}

#[tokio::test]
async fn publication_rechecks_current_permission_before_installing_task_edits() {
    for mode in [
        ApprovalMode::Auto,
        ApprovalMode::ReadOnly,
        ApprovalMode::Ask,
    ] {
        let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
            .await
            .unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(root.path().join("sessions.sqlite3")),
            Arc::new(UnusedLoader),
        )
        .await
        .unwrap();
        let resolved = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: source.to_string_lossy().into_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
            panic!("workspace")
        };
        let created = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("embedded/demo".to_owned()),
                        ..ModelSelection::default()
                    },
                    approval_mode: ApprovalMode::Auto,
                    profile: AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetApprovalMode { session_id, mode },
            )
            .await
            .unwrap();
        let published = task.clone().publish(&runtime.inner, session_id).await;
        assert_eq!(published.is_ok(), mode == ApprovalMode::Auto);
        let task_path = task.scope.workspace.path.clone();
        tokio::task::spawn_blocking(move || {
            assert_eq!(
                std::fs::read_to_string(source.join("artifact")).unwrap(),
                if mode == ApprovalMode::Auto {
                    "agent edit"
                } else {
                    "original"
                }
            );
            assert_eq!(
                std::fs::read_to_string(task_path.join("artifact")).unwrap(),
                "agent edit"
            );
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
    }
}
