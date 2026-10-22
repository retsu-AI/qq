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

async fn open_session(
    root: &Path,
    source: &Path,
    loader: Arc<dyn RuntimeLoader>,
) -> (SessionRuntime, SessionId) {
    let control = root.join("control");
    let policy = tokio::task::spawn_blocking(move || {
        std::fs::create_dir(&control).unwrap();
        TaskWorkspacePolicy {
            directory: NativeDirectoryGrant::capture(&control).unwrap(),
            read_only: vec![],
            resources: NativeResources::Host,
            limits: TaskWorkspaceLimits {
                max_entries: 16,
                max_bytes: 1024,
            },
        }
    })
    .await
    .unwrap();
    let mut options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
    options.task_workspaces = Some(policy);
    let runtime = SessionRuntime::open(options, loader).await.unwrap();
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
    (runtime, session_id)
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
        let (runtime, session_id) = open_session(
            root.path(),
            &source,
            Arc::new(PublicationLoader { protect_pack }),
        )
        .await;
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

#[tokio::test]
async fn publication_preserves_private_edits_when_source_disk_is_low() {
    let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let (runtime, session_id) = open_session(
        root.path(),
        &source,
        Arc::new(PublicationLoader {
            protect_pack: false,
        }),
    )
    .await;
    let _space = crate::storage::SpaceOverride::new(&source, 0);
    let error = task
        .clone()
        .publish(&runtime.inner, session_id)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        TaskExecutionError::Storage(crate::storage::Pressure::Low { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(source.join("vendor/policy.md")).unwrap(),
        "original"
    );
    assert_eq!(
        std::fs::read_to_string(task.scope.workspace.path.join("vendor/policy.md")).unwrap(),
        "agent edit"
    );
    runtime.shutdown().await.unwrap();
}

struct DiskPressureLoader;
impl RuntimeLoader for DiskPressureLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async move {
            struct FillWorkspace(PathBuf);
            impl qq_provider::Provider for FillWorkspace {
                fn stream(&self, _: qq_provider::ModelRequest) -> qq_provider::ProviderStream {
                    let path = self.0.clone();
                    Box::pin(futures_util::stream::once(async move {
                        let _space = tokio::task::spawn_blocking(move || {
                            std::fs::write(path.join("vendor/policy.md"), "retained edit").unwrap();
                            crate::storage::SpaceOverride::new(&path, 0)
                        })
                        .await
                        .unwrap();
                        std::future::pending().await
                    }))
                }
            }
            let path = request
                .execution_workspace
                .map_or_else(|| PathBuf::from(request.workspace), |grant| grant.path);
            let runtime = crate::Runtime::new(FillWorkspace(path.clone()), "demo", 256).unwrap();
            Ok(LoadedRuntime::from_runtime(runtime, request.profile, path)
                .await
                .unwrap())
        })
    }
}

#[tokio::test]
async fn disk_pressure_stops_provider_work_and_retains_unpublished_edits() {
    let (root, source, _) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let (runtime, session_id) =
        open_session(root.path(), &source, Arc::new(DiskPressureLoader)).await;
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
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: resolved.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("edit the file".to_owned())],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let finished = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::RunFinished { outcome, .. } = event.event {
                break outcome;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(&finished, RunOutcome::Failed { failure }
            if failure.message.contains("disk_space_low")
        ),
        "{finished:?}"
    );
    assert_eq!(
        std::fs::read_to_string(source.join("vendor/policy.md")).unwrap(),
        "original"
    );
    let tasks = std::fs::read_dir(root.path().join("control"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("vendor/policy.md"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 1);
    assert_eq!(std::fs::read_to_string(&tasks[0]).unwrap(), "retained edit");
    runtime.shutdown().await.unwrap();
}

struct SideFileLoader;
impl RuntimeLoader for SideFileLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async move {
            struct EchoRead;
            impl qq_provider::Provider for EchoRead {
                fn stream(
                    &self,
                    request: qq_provider::ModelRequest,
                ) -> qq_provider::ProviderStream {
                    use qq_provider::ProviderEvent;
                    let result = request.messages().iter().rev().find_map(|message| {
                        message.content().iter().find_map(|block| match block {
                            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                            _ => None,
                        })
                    });
                    let events = if let Some(text) = result {
                        vec![
                            ProviderEvent::OutputTextDelta { text },
                            ProviderEvent::Completed {
                                usage: Some(qq_provider::ProviderUsage {
                                    input_tokens: 1,
                                    output_tokens: 1,
                                    cache_read_input_tokens: 0,
                                    cache_write_input_tokens: 0,
                                    reasoning_tokens: None,
                                }),
                            },
                        ]
                    } else {
                        vec![
                            ProviderEvent::ToolCallStarted {
                                id: "read".into(),
                                name: "read_file".into(),
                            },
                            ProviderEvent::ToolCallArgumentsDelta {
                                id: "read".into(),
                                json: r#"{"path":"vendor/policy.md"}"#.into(),
                            },
                            ProviderEvent::ToolCallCompleted { id: "read".into() },
                            ProviderEvent::Completed {
                                usage: Some(qq_provider::ProviderUsage {
                                    input_tokens: 1,
                                    output_tokens: 1,
                                    cache_read_input_tokens: 0,
                                    cache_write_input_tokens: 0,
                                    reasoning_tokens: None,
                                }),
                            },
                        ]
                    };
                    Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                }
            }
            let runtime = crate::Runtime::new(EchoRead, "demo", 256).unwrap();
            let workspace = request
                .execution_workspace
                .map_or_else(|| PathBuf::from(request.workspace), |grant| grant.path);
            Ok(
                LoadedRuntime::from_runtime(runtime, request.profile, workspace)
                    .await
                    .expect("side fixture plan compiles"),
            )
        })
    }
}

#[tokio::test]
async fn side_question_reads_active_private_task_edits() {
    let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let (runtime, session) = open_session(root.path(), &source, Arc::new(SideFileLoader)).await;
    let record = encode(&task.record.lock().unwrap()).unwrap();
    let owner = task.owner;
    runtime.inner.store.call_write(Priority::AwaitControl, move |connection| {
        connection.execute(
            "INSERT INTO runs(id,session_id,command_id,user_message_id,assistant_message_id,status,created_at_ms) VALUES(?1,?2,?1,?1,?1,'running',0)",
            params![owner.to_string(), session.to_string()],
        )?;
        connection.execute("UPDATE sessions SET active_run_id=?1 WHERE id=?2", params![owner.to_string(), session.to_string()])?;
        for (key, value) in [
            (format!("{RUN_PREFIX}{owner}"), owner.to_string()),
            (format!("{RECORD_PREFIX}{owner}"), record),
        ] {
            connection.execute("INSERT INTO metadata(key,value) VALUES(?1,?2)", params![key,value])?;
        }
        Ok(())
    }).await.unwrap();
    let answer = runtime
        .answer_side_question(
            session,
            "read the current file".into(),
            RunCancellation::new(),
        )
        .await
        .unwrap();
    assert!(answer.text.contains("agent edit"), "{}", answer.text);
    assert!(!answer.text.contains("original"), "{}", answer.text);
    assert_eq!(
        std::fs::read_to_string(source.join("vendor/policy.md")).unwrap(),
        "original"
    );
    runtime
        .inner
        .store
        .call_write(Priority::AwaitControl, move |connection| {
            connection.execute(
                "DELETE FROM metadata WHERE key=?1",
                [format!("{RECORD_PREFIX}{owner}")],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let missing = runtime
        .answer_side_question(
            session,
            "inspect with missing record".into(),
            RunCancellation::new(),
        )
        .await;
    assert!(
        matches!(missing, Err(SessionRuntimeError::InvalidNativeScope { .. })),
        "{missing:?}"
    );
    let record = encode(&task.record.lock().unwrap()).unwrap();
    save(
        &runtime.inner.store,
        format!("{RECORD_PREFIX}{owner}"),
        record,
    )
    .await
    .unwrap();
    runtime
        .inner
        .store
        .call_write(Priority::AwaitControl, move |connection| {
            connection.execute(
                "DELETE FROM metadata WHERE key=?1",
                [format!("{RUN_PREFIX}{owner}")],
            )?;
            connection.execute(
                "UPDATE sessions SET active_run_id=NULL, preparing_run_id=?1 WHERE id=?2",
                params![owner.to_string(), session.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let pending = runtime
        .answer_side_question(
            session,
            "inspect while preparing".into(),
            RunCancellation::new(),
        )
        .await;
    assert!(
        matches!(pending, Err(SessionRuntimeError::SideQuestionBusy)),
        "{pending:?}"
    );
    save(
        &runtime.inner.store,
        format!("{RUN_PREFIX}{owner}"),
        owner.to_string(),
    )
    .await
    .unwrap();
    runtime
        .inner
        .store
        .call_write(Priority::AwaitControl, move |connection| {
            connection.execute(
                "UPDATE sessions SET active_run_id=?1, preparing_run_id=NULL WHERE id=?2",
                params![owner.to_string(), session.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let task_path = task.scope.workspace.path.clone();
    let retained = root.path().join("retained");
    tokio::task::spawn_blocking(move || {
        std::fs::rename(&task_path, &retained).unwrap();
        std::fs::create_dir(&task_path).unwrap();
        std::fs::create_dir(task_path.join("vendor")).unwrap();
        std::fs::write(task_path.join("vendor/policy.md"), "replacement").unwrap();
    })
    .await
    .unwrap();
    assert!(matches!(
        runtime
            .answer_side_question(
                session,
                "inspect a replaced task root".into(),
                RunCancellation::new()
            )
            .await,
        Err(SessionRuntimeError::InvalidNativeScope { .. })
    ));
    // These fixture rows represent the already-running main run; no executor
    // owns them. Retire them before asking shutdown to drain actual work.
    runtime
        .inner
        .store
        .call_write(Priority::AwaitControl, move |connection| {
            connection.execute(
                "UPDATE sessions SET active_run_id=NULL WHERE id=?1",
                [session.to_string()],
            )?;
            connection.execute(
                "UPDATE runs SET status='completed' WHERE id=?1",
                [owner.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
}
