use super::*;

struct PublicationLoader {
    protect_pack: bool,
}
impl RuntimeLoader for PublicationLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let protect_pack = self.protect_pack;
        Box::pin(async move {
            assert!(request.execution_workspace.is_none());
            let plan = tokio::task::spawn_blocking(move || {
                let runtime = crate::Runtime::new(
                    qq_provider::test_support::ScriptedProvider::new(vec![]),
                    "demo",
                    256,
                )
                .unwrap();
                let source = PathBuf::from(request.workspace);
                let roots = if protect_pack {
                    vec![source.join("vendor")]
                } else {
                    vec![]
                };
                crate::plan::CompiledAgentPlan::compile_blocking(
                    crate::plan::AgentProfile::embedded(&runtime, source)
                        .with_protected_pack_roots(roots),
                )
                .unwrap()
            })
            .await
            .unwrap();
            Ok(LoadedRuntime { plan })
        })
    }
}

fn publication_fixture() -> (tempfile::TempDir, PathBuf, Arc<TaskExecution>) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let source = path.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(source.join("vendor")).unwrap();
    std::fs::write(source.join("vendor/policy.md"), "original").unwrap();
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
        &["vendor/policy.md".into()],
        TaskWorkspaceLimits {
            max_entries: 16,
            max_bytes: 1024,
        },
    )
    .unwrap();
    std::fs::write(workspace.path().join("vendor/policy.md"), "agent edit").unwrap();
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
        source_request: RuntimeLoadRequest {
            execution_workspace: None,
            workspace: source.to_string_lossy().into_owned(),
            model: ModelSelection::default(),
            profile: AgentProfileId::default(),
            reasoning_effort: None,
        },
        scope,
        record: Mutex::new(record),
    });
    (root, source, execution)
}

#[tokio::test]
async fn publication_rechecks_current_permission_before_installing_task_edits() {
    for (mode, protect_pack) in [
        (ApprovalMode::Auto, false),
        (ApprovalMode::ReadOnly, false),
        (ApprovalMode::Ask, false),
        (ApprovalMode::Auto, true),
    ] {
        let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
            .await
            .unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(root.path().join("sessions.sqlite3")),
            Arc::new(PublicationLoader { protect_pack }),
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
        assert_eq!(
            published.is_ok(),
            mode == ApprovalMode::Auto && !protect_pack,
            "mode={mode:?}, protected={protect_pack}: {published:?}"
        );
        let task_path = task.scope.workspace.path.clone();
        tokio::task::spawn_blocking(move || {
            assert_eq!(
                std::fs::read_to_string(source.join("vendor/policy.md")).unwrap(),
                if mode == ApprovalMode::Auto && !protect_pack {
                    "agent edit"
                } else {
                    "original"
                }
            );
            assert_eq!(
                std::fs::read_to_string(task_path.join("vendor/policy.md")).unwrap(),
                "agent edit"
            );
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
    }
}
