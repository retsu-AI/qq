use super::*;
use crate::command_owner::{
    CommandResult, CommandSnapshot, CommandStatus, NativeCommand, NativeCommandFuture,
    NativeCommandScope, NativeCommandService, NativeDirectoryGrant, NativeNetwork,
    NativeResourceLimits, NativeResources,
};

#[derive(Clone, Default)]
struct Recording(Arc<StdMutex<Vec<NativeCommand>>>);
impl NativeCommandService for Recording {
    fn identity(&self) -> NativeCommandFuture<StoreId> {
        Box::pin(async { Ok(StoreId::from_bytes([55; 16])) })
    }
    fn submit(&self, command: NativeCommand) -> NativeCommandFuture<()> {
        self.0.lock().unwrap().push(command);
        Box::pin(async { Ok(()) })
    }
    fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
        Box::pin(async { Ok(()) })
    }
    fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
        Box::pin(async { Ok(()) })
    }
    fn wait(&self, id: ToolCallId, _: u64) -> NativeCommandFuture<CommandSnapshot> {
        Box::pin(async move {
            Ok(CommandSnapshot {
                id,
                version: 1,
                status: CommandStatus::Finished,
                process: None,
                output: String::new(),
                result: Some(
                    serde_json::from_value::<CommandResult>(
                        serde_json::to_value(crate::tools::dispatch::bounded_result(
                            "scoped-call".into(),
                            false,
                        ))
                        .unwrap(),
                    )
                    .unwrap(),
                ),
            })
        })
    }
}

fn scope(path: &Path) -> NativeCommandScope {
    NativeCommandScope {
        sdk_environment: None,
        executable_directories: Vec::new(),
        workspace: NativeDirectoryGrant::capture(path).unwrap(),
        read_only: vec![],
        network: NativeNetwork::Isolated,
        resources: NativeResources::Limited {
            limits: NativeResourceLimits {
                memory_bytes: 64 * 1024 * 1024,
                max_processes: 16,
                cpu_millicores: 500,
            },
        },
    }
}

fn loader() -> Arc<ScriptedRunsLoader> {
    Arc::new(ScriptedRunsLoader {
        requests: Arc::default(),
        runs: vec![vec![("shell", r#"{"command":"echo scoped"}"#.into())]; 4],
        loads: StdMutex::new(0),
    })
}

fn options(database: &Path) -> SessionRuntimeOptions {
    let mut options = SessionRuntimeOptions::new(database.to_owned());
    options.grant_authority = Some(ScriptedGrantAuthority::new(
        WorkspaceGrantSeed {
            tools: vec!["shell".into()],
            ..Default::default()
        },
        WorkspaceGrantOutcome::AlreadyPresent {
            path: "host policy".into(),
        },
    ));
    options
}

async fn run(
    runtime: &SessionRuntime,
    workspace: &Path,
    parent: Option<SessionId>,
) -> (SessionId, Vec<SessionEventEnvelope>) {
    let (workspace_id, _) = resolve_workspace(runtime, workspace).await;
    let created = create_session_with_mode(runtime, workspace_id, parent, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session not created")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    submit_prompt_to(runtime, session_id, "run scoped command").await;
    let observed = collect_through_finished(&mut events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    (session_id, observed)
}

#[tokio::test]
async fn native_scope_survives_restart_and_applies_to_child_sessions() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    let database = path.join("sessions.sqlite3");
    let service = Recording::default();
    let mut initial = options(&database);
    initial.native_scopes = vec![scope(&workspace)];
    initial.native_commands = Some(Arc::new(service.clone()));
    let runtime = SessionRuntime::open(initial, loader()).await.unwrap();
    let (parent, first) = run(&runtime, &workspace, None).await;
    assert!(first.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));
    runtime.shutdown().await.unwrap();
    drop(runtime);
    // Deliberately omit scope registration on restart. Stored authority wins.
    let mut resumed = options(&database);
    resumed.native_commands = Some(Arc::new(service.clone()));
    let runtime = SessionRuntime::open(resumed, loader()).await.unwrap();
    let (_, second) = run(&runtime, &workspace, Some(parent)).await;
    assert!(second.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));
    runtime.shutdown().await.unwrap();
    let requests = service.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.workspace, workspace);
        assert_eq!(request.network, NativeNetwork::Isolated);
        assert!(
            matches!(request.resources, NativeResources::Limited { limits } if limits.cpu_millicores==500 && limits.max_processes==16 && limits.memory_bytes==64*1024*1024)
        );
        let crate::command_owner::NativeFilesystem::Restricted {
            read_only,
            read_write,
        } = &request.filesystem
        else {
            panic!("command escaped to host filesystem")
        };
        assert!(read_only.is_empty());
        assert_eq!(read_write.len(), 1);
        assert_eq!(read_write[0].path, workspace);
    }
}

#[tokio::test]
async fn native_scope_never_falls_back_without_a_command_service() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    let mut configuration = options(&path.join("sessions.sqlite3"));
    configuration.native_scopes = vec![scope(&workspace)];
    let runtime = SessionRuntime::open(configuration, loader()).await.unwrap();
    let (_, observed) = run(&runtime, &workspace, None).await;
    runtime.shutdown().await.unwrap();
    assert!(observed.iter().any(|event| matches!(&event.event,SessionEvent::ToolCallFinished { tool_call, .. } if tool_call.result.as_deref().is_some_and(|result| result.contains("scoped execution requires the durable native command service")))));
}

#[tokio::test]
async fn native_scope_rejects_unregistered_workspaces_before_provider_execution() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    let other = path.join("other");
    std::fs::create_dir(&other).unwrap();
    let mut configuration = options(&path.join("sessions.sqlite3"));
    configuration.native_scopes = vec![scope(&workspace)];
    let provider = loader();
    let runtime = SessionRuntime::open(configuration, provider.clone())
        .await
        .unwrap();
    let (_, observed) = run(&runtime, &other, None).await;
    runtime.shutdown().await.unwrap();
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        *provider.loads.lock().unwrap(),
        0,
        "an unauthorized root must not load project configuration"
    );
    assert!(observed.iter().any(|event| matches!(&event.event,SessionEvent::RunFinished { outcome:RunOutcome::Failed { failure }, .. } if failure.message.contains("no registered native execution scope"))));
}

#[tokio::test]
async fn native_scope_conflicting_registration_does_not_replace_stored_authority() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let store = Store::open(path.join("sessions.sqlite3")).await.unwrap();
    let original = scope(&path);
    super::super::native_scope::load(&store, vec![original.clone()])
        .await
        .unwrap();
    let mut widened = original;
    widened.network = NativeNetwork::Host;
    assert!(matches!(
        super::super::native_scope::load(&store, vec![widened]).await,
        Err(SessionRuntimeError::InvalidNativeScope { .. })
    ));
    let restored = super::super::native_scope::load(&store, vec![])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        restored.get(&path).unwrap().network,
        NativeNetwork::Isolated
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn native_scope_replaced_task_directory_is_refused_before_provider_execution() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    let mut configuration = options(&path.join("sessions.sqlite3"));
    configuration.native_scopes = vec![scope(&workspace)];
    let provider = loader();
    let runtime = SessionRuntime::open(configuration, provider.clone())
        .await
        .unwrap();
    std::fs::rename(&workspace, path.join("original-task")).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let (_, observed) = run(&runtime, &workspace, None).await;
    runtime.shutdown().await.unwrap();
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        *provider.loads.lock().unwrap(),
        0,
        "an unauthorized root must not load project configuration"
    );
    assert!(observed.iter().any(|event| matches!(&event.event,SessionEvent::RunFinished { outcome:RunOutcome::Failed { failure }, .. } if failure.message.contains("native scope does not match"))));
}

#[tokio::test]
async fn native_scope_corrupt_or_empty_persisted_policy_never_restores_host_authority() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let store = Store::open(path.join("sessions.sqlite3")).await.unwrap();
    for policy in ["[]", "{malformed"] {
        store.call_write(Priority::Control, move |connection| {
            let unit=store::begin_unit(connection)?;
            unit.execute("INSERT INTO metadata(key,value) VALUES('native_command_scopes',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[policy])?;
            unit.commit()?;
            Ok(())
        }).await.unwrap();
        assert!(
            super::super::native_scope::load(&store, vec![])
                .await
                .is_err()
        );
    }
    store.close().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_scope_enforces_auto_session_commands_through_the_real_owner() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("input"), "initial").unwrap();
    let outside = path.join("outside");
    let command = format!(
        "printf task > input; printf escaped > '{}'; cat input",
        outside.display()
    );
    let owner = crate::command_owner::CommandOwner::open(
        path.join("commands"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    let mut policy = scope(&workspace);
    // This fixture qualifies filesystem/network dispatch; delegated resource
    // qualification is covered by its explicit native cgroup fixture.
    policy.resources = NativeResources::Host;
    for root in ["/bin", "/usr", "/lib", "/lib64", "/nix/store"] {
        if let Ok(root) = Path::new(root).canonicalize()
            && !policy.read_only.iter().any(|grant| grant.path == root)
        {
            policy
                .read_only
                .push(NativeDirectoryGrant::capture(&root).unwrap());
        }
    }
    let mut configuration = options(&path.join("sessions.sqlite3"));
    configuration.native_scopes = vec![policy];
    configuration.native_commands = Some(Arc::new(owner.clone()));
    let provider = Arc::new(ScriptedRunsLoader {
        requests: Arc::default(),
        runs: vec![vec![(
            "shell",
            serde_json::json!({"command":command}).to_string(),
        )]],
        loads: StdMutex::new(0),
    });
    let runtime = SessionRuntime::open(configuration, provider).await.unwrap();
    let (_, observed) = run(&runtime, &workspace, None).await;
    runtime.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();
    assert!(!outside.exists(), "Auto session escaped its task workspace");
    let result = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call, .. } => tool_call.result.as_deref(),
            _ => None,
        })
        .expect("native result");
    if result.contains("required Linux filesystem enforcement is unavailable")
        || result.contains("required Linux network enforcement is unavailable")
    {
        assert_eq!(
            std::fs::read_to_string(workspace.join("input")).unwrap(),
            "initial"
        );
    } else {
        assert_eq!(
            std::fs::read_to_string(workspace.join("input")).unwrap(),
            "task",
            "{result}"
        );
        assert!(result.contains("task"), "{result}");
    }
}

#[tokio::test]
async fn native_scope_goal_check_never_falls_back_to_host_execution() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    let marker = workspace.join("host-execution");
    let mut configuration = options(&path.join("sessions.sqlite3"));
    configuration.native_scopes = vec![scope(&workspace)];
    let runtime = SessionRuntime::open(configuration, loader()).await.unwrap();
    let resources =
        execution::RunResources::with_native_commands(None, runtime.inner.native_scopes.clone());
    let result = crate::sessions::goal_check::run_process(
        workspace,
        "echo escaped > host-execution".into(),
        tokio::time::Instant::now() + Duration::from_secs(10),
        RunCancellation::new(),
        Arc::new(crate::ProcessProducer::new(env!("CARGO_PKG_VERSION"))),
        resources.clone(),
    )
    .await;
    assert!(
        result.is_err(),
        "a scoped check must not use the host process launcher"
    );
    assert!(!marker.exists(), "the host command executed before refusal");
    resources.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_scope_tool_search_roots_require_read_authority_and_survive_restart() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    for name in ["work", "sdk", "sdk-lib", "sdk-include"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    let sdk = NativeDirectoryGrant::capture(&root.join("sdk")).unwrap();
    let mut admitted = scope(&root.join("work"));
    admitted.executable_directories = vec![sdk.path.clone()];
    assert!(super::super::native_scope::NativeScopes::single(admitted.clone()).is_err());
    admitted.read_only.push(sdk.clone());
    assert!(super::super::native_scope::NativeScopes::single(admitted.clone()).is_ok());
    let mut oversized = admitted.clone();
    oversized.executable_directories = vec![sdk.path.clone(); 17];
    assert!(super::super::native_scope::NativeScopes::single(oversized).is_err());
    admitted.sdk_environment = Some(crate::command_owner::NativeSdkEnvironment {
        linker: sdk.path.join("link.exe"),
        libraries: vec![root.join("sdk-lib")],
        includes: vec![root.join("sdk-include")],
        target: "aarch64-pc-windows-msvc".into(),
    });
    assert!(super::super::native_scope::NativeScopes::single(admitted.clone()).is_err());
    admitted.read_only.extend(
        ["sdk-lib", "sdk-include"]
            .map(|name| NativeDirectoryGrant::capture(&root.join(name)).unwrap()),
    );
    let expected_environment = serde_json::to_value(&admitted.sdk_environment).unwrap();
    let database = root.join("sessions.sqlite3");
    let store = Store::open(database.clone()).await.unwrap();
    super::super::native_scope::load(&store, vec![admitted])
        .await
        .unwrap();
    store.close().await.unwrap();
    let store = Store::open(database).await.unwrap();
    let restored = super::super::native_scope::load(&store, vec![])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        restored
            .get(&root.join("work"))
            .unwrap()
            .executable_directories,
        [sdk.path]
    );
    assert_eq!(
        serde_json::to_value(&restored.get(&root.join("work")).unwrap().sdk_environment).unwrap(),
        expected_environment
    );
    store.close().await.unwrap();
}
