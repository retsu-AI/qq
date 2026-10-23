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
    let root = root.to_owned();
    let (root, policy) = tokio::task::spawn_blocking(move || {
        let root = root.canonicalize().unwrap();
        let control = root.join("control");
        std::fs::create_dir(&control).unwrap();
        let policy = TaskWorkspacePolicy {
            directory: NativeDirectoryGrant::capture(&control).unwrap(),
            read_only: vec![],
            resources: NativeResources::Host,
            limits: TaskWorkspaceLimits {
                max_entries: 16,
                max_bytes: 1024,
            },
        };
        (root, policy)
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

async fn publish_fixture(
    task: Arc<TaskExecution>,
    runtime: &SessionRuntime,
    session: SessionId,
) -> Result<(), TaskExecutionError> {
    let run = task.owner;
    runtime.inner.store.call_write(Priority::AwaitControl, move |connection| {
        connection.execute("INSERT INTO runs(id,session_id,command_id,user_message_id,assistant_message_id,status,created_at_ms) VALUES(?1,?2,?1,?1,?1,'running',0)", params![run.to_string(), session.to_string()])?;
        Ok(())
    }).await.unwrap();
    let (cancel, _) = watch::channel(false);
    assert!(
        runtime
            .inner
            .cancellations
            .lock()
            .unwrap()
            .insert(run, cancel)
            .is_none()
    );
    let result = task.publish(&runtime.inner, session, run).await;
    runtime.inner.cancellations.lock().unwrap().remove(&run);
    runtime
        .inner
        .store
        .call_write(Priority::AwaitControl, move |connection| {
            connection.execute(
                "UPDATE runs SET status='completed' WHERE id=?1",
                [run.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    result
}

#[tokio::test]
async fn publication_delivers_new_ignored_files_without_generated_directories() {
    let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let path = &task.scope.workspace.path;
    std::fs::write(path.join(".gitignore"), "*.local\n").unwrap();
    std::fs::write(path.join("requested.local"), "requested output").unwrap();
    std::fs::create_dir(path.join("target")).unwrap();
    std::fs::write(path.join("target/build-cache"), "disposable").unwrap();
    let storage_root = root.path().to_path_buf();
    #[cfg(unix)]
    let storage_root = {
        // Exercise the same storage alias used by macOS temporary directories.
        let alias = storage_root.join("storage-alias");
        std::os::unix::fs::symlink(storage_root.canonicalize().unwrap(), &alias).unwrap();
        alias
    };
    let (runtime, session) = open_session(
        &storage_root,
        &source,
        Arc::new(PublicationLoader {
            protect_pack: false,
        }),
    )
    .await;
    publish_fixture(task, &runtime, session).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(source.join("requested.local"))
            .ok()
            .as_deref(),
        Some("requested output")
    );
    assert!(!source.join("target").exists());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_goal_task_respects_tightened_publication_byte_limit() {
    restored_goal_task_respects_tightened_publication_limits(true).await;
}

#[tokio::test]
async fn goal_check_imports_published_ignored_outputs_from_current_source() {
    let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let private = task.scope.workspace.path.clone();
    std::fs::write(private.join(".gitignore"), "*.local\n").unwrap();
    std::fs::write(private.join("requested.local"), "published output").unwrap();
    std::fs::write(private.join("removed.local"), "published then removed").unwrap();
    std::fs::remove_file(private.join("vendor/policy.md")).unwrap();
    let loader: Arc<dyn RuntimeLoader> = Arc::new(PublicationLoader {
        protect_pack: false,
    });
    let (runtime, session) = open_session(root.path(), &source, loader).await;
    publish_fixture(task.clone(), &runtime, session)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(source.join("requested.local")).unwrap(),
        "published output"
    );
    assert!(!source.join("vendor/policy.md").exists());
    let goal = qq_protocol::GoalId::generate().unwrap();
    save(
        &runtime.inner.store,
        format!("{GOAL_PREFIX}{goal}"),
        task.owner.to_string(),
    )
    .await
    .unwrap();
    // Verification must import source bytes, never revive the goal's private
    // tree or its unpublished recreation of a successfully deleted input.
    std::fs::write(private.join("requested.local"), "private stale").unwrap();
    std::fs::write(private.join("vendor/policy.md"), "private recreation").unwrap();
    std::fs::write(source.join("requested.local"), "current source").unwrap();
    std::fs::remove_file(source.join("removed.local")).unwrap();
    let workspace = runtime
        .inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            session_workspace(connection, session)
        })
        .await
        .unwrap();
    let mut claim = super::super::settlement::recovery_claim(RunIdentity {
        workspace_id: workspace,
        session_id: session,
        run_id: RunId::generate().unwrap(),
        command_id: CommandId::generate().unwrap(),
        kind: RunKind::GoalCheck,
        child: false,
    });
    claim.workspace = source.to_string_lossy().into_owned();
    claim.approval_mode = ApprovalMode::Auto;
    claim.origin = Some(qq_protocol::RunOrigin::GoalCheck {
        goal_id: goal,
        revision: 1,
    });
    let mut resources = super::super::execution::RunResources::default();
    prepare_check(
        &runtime.inner,
        &claim,
        &RunCancellation::new(),
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        &mut resources,
    )
    .await
    .unwrap();
    let checked = resources.task_scope().expect("check has a private scope");
    assert_ne!(checked.workspace.path, private);
    assert_eq!(
        std::fs::read_to_string(checked.workspace.path.join("requested.local")).unwrap(),
        "current source"
    );
    assert!(!checked.workspace.path.join("vendor/policy.md").exists());
    assert!(!checked.workspace.path.join("removed.local").exists());
    assert_eq!(
        std::fs::read_to_string(private.join("requested.local")).unwrap(),
        "private stale"
    );
    assert_eq!(
        read(&runtime.inner.store, format!("{GOAL_PREFIX}{goal}"))
            .await
            .unwrap(),
        Some(task.owner.to_string()),
        "verification must not replace the goal's private-tree owner"
    );
    std::fs::remove_dir_all(&private).unwrap();
    claim.identity.run_id = RunId::generate().unwrap();
    let mut next_resources = super::super::execution::RunResources::default();
    prepare_check(
        &runtime.inner,
        &claim,
        &RunCancellation::new(),
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        &mut next_resources,
    )
    .await
    .expect("published source verification does not require the old private tree");
    let next = next_resources.task_scope().unwrap();
    assert_ne!(next.workspace.path, checked.workspace.path);
    assert_eq!(
        std::fs::read_to_string(next.workspace.path.join("requested.local")).unwrap(),
        "current source"
    );
    assert_eq!(
        read(&runtime.inner.store, format!("{GOAL_PREFIX}{goal}"))
            .await
            .unwrap(),
        Some(task.owner.to_string())
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn goal_check_filters_current_pack_protections_and_refuses_invalid_publication_provenance() {
    for scenario in ["protected", "outer-source", "embedded-source", "scope"] {
        let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
            .await
            .unwrap();
        std::fs::write(task.scope.workspace.path.join(".gitignore"), "*.local\n").unwrap();
        std::fs::write(
            task.scope.workspace.path.join("vendor/extra.local"),
            "published pack input",
        )
        .unwrap();
        let loader: Arc<dyn RuntimeLoader> = Arc::new(PublicationLoader {
            protect_pack: false,
        });
        let (runtime, session) = open_session(root.path(), &source, loader).await;
        publish_fixture(task.clone(), &runtime, session)
            .await
            .unwrap();
        let goal = qq_protocol::GoalId::generate().unwrap();
        save(
            &runtime.inner.store,
            format!("{GOAL_PREFIX}{goal}"),
            task.owner.to_string(),
        )
        .await
        .unwrap();
        let key = format!("{RECORD_PREFIX}{}", task.owner);
        let saved = read(&runtime.inner.store, key.clone())
            .await
            .unwrap()
            .unwrap();
        let mut record: serde_json::Value = serde_json::from_str(&saved).unwrap();
        match scenario {
            "outer-source" => {
                record["source"]["path"] = serde_json::json!(source.join("different"))
            }
            "embedded-source" => {
                record["workspace"]["source"]["path"] = serde_json::json!(source.join("different"))
            }
            "scope" => record["scope"]["workspace"]["path"] = serde_json::json!(source),
            "protected" => {}
            _ => unreachable!(),
        }
        save(&runtime.inner.store, key, record.to_string())
            .await
            .unwrap();
        let workspace = runtime
            .inner
            .store
            .call(Priority::AwaitControl, move |connection| {
                session_workspace(connection, session)
            })
            .await
            .unwrap();
        let mut claim = super::super::settlement::recovery_claim(RunIdentity {
            workspace_id: workspace,
            session_id: session,
            run_id: RunId::generate().unwrap(),
            command_id: CommandId::generate().unwrap(),
            kind: RunKind::GoalCheck,
            child: false,
        });
        claim.workspace = source.to_string_lossy().into_owned();
        claim.approval_mode = ApprovalMode::Auto;
        claim.origin = Some(qq_protocol::RunOrigin::GoalCheck {
            goal_id: goal,
            revision: 1,
        });
        let plan = PublicationLoader {
            protect_pack: scenario == "protected",
        }
        .load(task.source_request.clone())
        .await
        .unwrap()
        .plan;
        let prepared = prepare(&runtime.inner, &claim, plan, &RunCancellation::new(), None).await;
        runtime.shutdown().await.unwrap();
        if scenario == "protected" {
            let checked = prepared.unwrap().unwrap();
            assert!(
                !checked
                    .scope
                    .workspace
                    .path
                    .join("vendor/extra.local")
                    .exists()
            );
            assert!(
                !checked
                    .scope
                    .workspace
                    .path
                    .join("vendor/policy.md")
                    .exists()
            );
            assert_eq!(
                std::fs::read_to_string(source.join("vendor/extra.local")).unwrap(),
                "published pack input"
            );
        } else {
            assert!(
                prepared.is_err(),
                "{scenario}: invalid published authority was accepted"
            );
        }
    }
}

#[tokio::test]
async fn restored_goal_task_respects_tightened_publication_entry_limit() {
    restored_goal_task_respects_tightened_publication_limits(false).await;
}

async fn restored_goal_task_respects_tightened_publication_limits(bytes: bool) {
    let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let loader: Arc<dyn RuntimeLoader> = Arc::new(PublicationLoader {
        protect_pack: false,
    });
    let (runtime, session) = open_session(root.path(), &source, loader.clone()).await;
    let goal = qq_protocol::GoalId::generate().unwrap();
    let owner = task.owner;
    let encoded = encode(&task.record.lock().unwrap()).unwrap();
    save(
        &runtime.inner.store,
        format!("{RECORD_PREFIX}{owner}"),
        encoded,
    )
    .await
    .unwrap();
    save(
        &runtime.inner.store,
        format!("{GOAL_PREFIX}{goal}"),
        owner.to_string(),
    )
    .await
    .unwrap();
    let mut policy = runtime.inner.task_workspaces.clone().unwrap();
    runtime.shutdown().await.unwrap();
    runtime.close().await.unwrap();
    drop(runtime);
    // Retained edits predate the tighter policy and must remain recoverable.
    let retained = if bytes {
        vec![(PathBuf::from("vendor/policy.md"), "x".repeat(128))]
    } else {
        (0..4)
            .map(|index| (PathBuf::from(format!("new-{index}.txt")), "new".to_owned()))
            .collect()
    };
    for (path, contents) in &retained {
        std::fs::write(task.scope.workspace.path.join(path), contents).unwrap();
    }
    if bytes {
        policy.limits.max_bytes = 64;
    } else {
        policy.limits.max_entries = 4;
    }
    let mut options =
        SessionRuntimeOptions::new(root.path().canonicalize().unwrap().join("sessions.sqlite3"));
    options.task_workspaces = Some(policy);
    let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
    let workspace = runtime
        .inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            session_workspace(connection, session)
        })
        .await
        .unwrap();
    let mut claim = super::super::settlement::recovery_claim(RunIdentity {
        workspace_id: workspace,
        session_id: session,
        run_id: RunId::generate().unwrap(),
        command_id: CommandId::generate().unwrap(),
        kind: RunKind::Prompt,
        child: false,
    });
    claim.workspace = source.to_string_lossy().into_owned();
    claim.approval_mode = ApprovalMode::Auto;
    claim.origin = Some(qq_protocol::RunOrigin::Goal {
        goal_id: goal,
        revision: 1,
    });
    let plan = loader.load(task.source_request.clone()).await.unwrap().plan;
    let restored = prepare(&runtime.inner, &claim, plan, &RunCancellation::new(), None)
        .await
        .unwrap()
        .expect("goal continuation restores its retained task");
    assert_eq!(restored.scope.workspace.path, task.scope.workspace.path);
    let published = publish_fixture(restored, &runtime, session).await;
    runtime.shutdown().await.unwrap();
    assert!(
        published.is_err(),
        "saved authority exceeded the current host limit: {published:?}"
    );
    assert_eq!(
        std::fs::read_to_string(source.join("vendor/policy.md")).unwrap(),
        "original"
    );
    for (path, contents) in retained {
        assert_eq!(
            std::fs::read_to_string(task.scope.workspace.path.join(&path)).unwrap(),
            contents
        );
        if path != Path::new("vendor/policy.md") {
            assert!(!source.join(path).exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn task_selection_never_blocks_reading_special_ignore_files() {
    use std::{sync::mpsc::sync_channel, time::Duration};
    for name in [".gitignore", ".ignore"] {
        for inputs in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().canonicalize().unwrap();
            let fifo = path.join(name);
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            std::fs::write(path.join("input.txt"), "input").unwrap();
            let workspace = Workspace::open(&path).unwrap();
            let (started, ready) = sync_channel(1);
            let (finished, result) = sync_channel(1);
            let thread = std::thread::spawn(move || {
                started.send(()).unwrap();
                finished
                    .send(files(
                        &workspace,
                        TaskWorkspaceLimits {
                            max_entries: 16,
                            max_bytes: 1024,
                        },
                        inputs,
                    ))
                    .unwrap();
            });
            ready.recv_timeout(Duration::from_secs(15)).unwrap();
            let (stalled, selected) = match result.recv_timeout(Duration::from_secs(1)) {
                Ok(selected) => (false, selected),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Release the old blocking reader so the failing regression
                    // still joins its thread and removes its temporary files.
                    let writer = rustix::fs::open(
                        &fifo,
                        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
                        rustix::fs::Mode::empty(),
                    )
                    .unwrap();
                    drop(writer);
                    (true, result.recv_timeout(Duration::from_secs(15)).unwrap())
                }
                Err(error) => panic!("selection worker stopped: {error}"),
            };
            thread.join().unwrap();
            assert!(!stalled, "{name} blocked task selection (inputs={inputs})");
            if inputs {
                assert!(selected.unwrap().contains(&PathBuf::from("input.txt")));
            } else {
                assert!(selected.is_err(), "special outputs must be refused");
            }
        }
    }
}

#[test]
fn task_selection_stops_inside_a_directory_listing() {
    use std::cell::Cell;
    let root = tempfile::tempdir().unwrap();
    for index in 0..128 {
        std::fs::write(root.path().join(format!("file-{index}")), "input").unwrap();
    }
    let workspace = Workspace::open(&root.path().canonicalize().unwrap()).unwrap();
    let checks = Cell::new(0usize);
    let stopped = || {
        checks.set(checks.get() + 1);
        checks.get() >= 4
    };
    let result = files_until(
        &workspace,
        TaskWorkspaceLimits {
            max_entries: 256,
            max_bytes: 1024,
        },
        true,
        &stopped,
    );
    assert!(
        matches!(result, Err(TaskExecutionError::Stopped)),
        "{result:?}"
    );
    assert!(checks.get() < 128, "stopping must not finish the listing");
}

#[test]
fn task_import_selection_keeps_dotfiles_and_nested_ignore_rules_with_bounds() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    std::fs::create_dir(path.join("nested")).unwrap();
    std::fs::create_dir(path.join("target")).unwrap();
    for (name, content) in [
        (".gitignore", "*.local\n!keep.local\n"),
        (".hidden", "dotfile"),
        ("ignored.local", "ignored"),
        ("keep.local", "kept"),
        ("nested/.ignore", "drop.txt\n"),
        ("nested/drop.txt", "ignored"),
        ("nested/keep.txt", "kept"),
        ("target/cache", "generated"),
    ] {
        std::fs::write(path.join(name), content).unwrap();
    }
    let workspace = Workspace::open(&path).unwrap();
    let limits = TaskWorkspaceLimits {
        max_entries: 32,
        max_bytes: 1024,
    };
    let selected: BTreeSet<_> = files(&workspace, limits, true)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        selected,
        [
            ".gitignore",
            ".hidden",
            "keep.local",
            "nested/.ignore",
            "nested/keep.txt",
            "target/cache"
        ]
        .map(PathBuf::from)
        .into_iter()
        .collect()
    );
    assert!(
        files(
            &workspace,
            TaskWorkspaceLimits {
                max_entries: 1,
                ..limits
            },
            true
        )
        .is_err()
    );
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
        let published = publish_fixture(task.clone(), &runtime, session_id).await;
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
    let error = publish_fixture(task.clone(), &runtime, session_id)
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

#[tokio::test]
async fn task_preparation_cancellation_settles_without_filesystem_capacity() {
    stalled_task_preparation(false).await;
}

#[tokio::test]
async fn task_preparation_deadline_settles_without_filesystem_capacity() {
    stalled_task_preparation(true).await;
}

async fn stalled_task_preparation(expire: bool) {
    let (root, source, _) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let (runtime, session) = open_session(root.path(), &source, Arc::new(SideFileLoader)).await;
    let workspace = runtime
        .inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            session_workspace(connection, session)
        })
        .await
        .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: workspace,
            after: EventCursor {
                store_id: runtime.store_id(),
                workspace_id: workspace,
                sequence: 0,
            },
        })
        .unwrap();
    let permits = crate::workspace::blocking_permits();
    let held = permits
        .clone()
        .acquire_many_owned(u32::try_from(permits.available_permits()).unwrap())
        .await
        .unwrap();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    PREPARATION_WAITERS
        .lock()
        .unwrap()
        .push((source.to_string_lossy().into_owned(), entered));
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: session,
                input: vec![InputPart::text("prepare a task".to_owned())],
                limits: RunLimits {
                    max_duration_ms: expire.then_some(60_000),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("prompt queued")
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    if expire {
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        tokio::time::resume();
    } else {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();
    }
    let settled = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::RunFinished {
                run_id: finished,
                outcome,
                ..
            } = event.event
                && finished == run_id
            {
                break outcome;
            }
        }
    })
    .await;
    // Release the old implementation as well, so a failing test still drains
    // its executor and removes its temporary files.
    drop(held);
    runtime.shutdown().await.unwrap();
    assert!(
        if expire {
            matches!(&settled, Ok(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
        } else {
            matches!(settled, Ok(RunOutcome::Cancelled))
        },
        "cancellation must settle while capacity remains occupied: {settled:?}"
    );
    assert!(
        std::fs::read_dir(root.path().join("control"))
            .unwrap()
            .next()
            .is_none(),
        "cancelled preparation must not import a task after capacity returns"
    );
}

#[tokio::test]
async fn check_task_preparation_cancellation_releases_its_capacity_wait() {
    stalled_check_task_preparation(false).await;
}

#[tokio::test]
async fn check_task_preparation_deadline_releases_its_capacity_wait() {
    stalled_check_task_preparation(true).await;
}

async fn stalled_check_task_preparation(expire: bool) {
    let (root, source, task) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let (runtime, session) = open_session(root.path(), &source, Arc::new(SideFileLoader)).await;
    let workspace = runtime
        .inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            session_workspace(connection, session)
        })
        .await
        .unwrap();
    let mut claim = super::super::settlement::recovery_claim(RunIdentity {
        workspace_id: workspace,
        session_id: session,
        run_id: task.owner,
        command_id: CommandId::generate().unwrap(),
        kind: RunKind::GoalCheck,
        child: false,
    });
    claim.workspace = source.to_string_lossy().into_owned();
    claim.approval_mode = ApprovalMode::Auto;
    let permits = crate::workspace::blocking_permits();
    let held = permits
        .clone()
        .acquire_many_owned(u32::try_from(permits.available_permits()).unwrap())
        .await
        .unwrap();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    PREPARATION_WAITERS
        .lock()
        .unwrap()
        .push((claim.workspace.clone(), entered));
    let cancellation = RunCancellation::new();
    let stopped = cancellation.clone();
    let inner = runtime.inner.clone();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut preparation = tokio::spawn(async move {
        let mut resources = super::super::execution::RunResources::default();
        prepare_check(&inner, &claim, &stopped, deadline, &mut resources).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    if expire {
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        tokio::time::resume();
    } else {
        cancellation.cancel();
    }
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), &mut preparation).await;
    drop(held);
    if stopped.is_err() {
        let _ = preparation.await;
    }
    runtime.shutdown().await.unwrap();
    assert!(
        matches!(stopped, Ok(Ok(Err(TaskExecutionError::Stopped)))),
        "check preparation must stop before capacity is released: {stopped:?}"
    );
    assert_eq!(cancellation.timed_out(), expire);
    assert!(
        std::fs::read_dir(root.path().join("control"))
            .unwrap()
            .next()
            .is_none()
    );
}

struct AttachmentLoader {
    path: String,
    edit: bool,
    prompts: Arc<Mutex<Vec<String>>>,
}
impl RuntimeLoader for AttachmentLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let path = self.path.clone();
        let edit = self.edit;
        let prompts = self.prompts.clone();
        Box::pin(async move {
            struct CaptureAttachment {
                script: qq_provider::test_support::ScriptedProvider,
                prompts: Arc<Mutex<Vec<String>>>,
            }
            impl qq_provider::Provider for CaptureAttachment {
                fn stream(
                    &self,
                    request: qq_provider::ModelRequest,
                ) -> qq_provider::ProviderStream {
                    let prompt = request
                        .messages()
                        .iter()
                        .filter(|message| message.role() == Role::User)
                        .flat_map(|message| message.content())
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.prompts.lock().unwrap().push(prompt);
                    self.script.stream(request)
                }
            }
            let script = if edit {
                qq_provider::test_support::ScriptedProvider::tool_then_text(
                    "write_file",
                    &serde_json::json!({"path": path, "content": "updated attachment"}).to_string(),
                    "updated",
                )
            } else {
                qq_provider::test_support::ScriptedProvider::new(vec![vec![
                    qq_provider::ProviderEvent::OutputTextDelta {
                        text: "read attachment".to_owned(),
                    },
                    qq_provider::ProviderEvent::Completed { usage: None },
                ]])
            };
            let runtime =
                crate::Runtime::new(CaptureAttachment { script, prompts }, "demo", 256).unwrap();
            let workspace = request
                .execution_workspace
                .map_or_else(|| PathBuf::from(request.workspace), |grant| grant.path);
            Ok(
                LoadedRuntime::from_runtime(runtime, request.profile, workspace)
                    .await
                    .unwrap(),
            )
        })
    }
}

#[tokio::test]
async fn explicit_task_attachments_survive_ignore_rules_and_keep_source_authority() {
    for (relative, edit) in [("notes.local", true), (".qq/config.ron", false)] {
        let (root, source, _) = tokio::task::spawn_blocking(publication_fixture)
            .await
            .unwrap();
        std::fs::write(source.join(".gitignore"), "*.local\n").unwrap();
        std::fs::create_dir_all(source.join(".qq")).unwrap();
        std::fs::write(source.join(relative), "explicit attachment bytes").unwrap();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let (runtime, session) = open_session(
            root.path(),
            &source,
            Arc::new(AttachmentLoader {
                path: relative.to_owned(),
                edit,
                prompts: prompts.clone(),
            }),
        )
        .await;
        let workspace = runtime
            .inner
            .store
            .call(Priority::AwaitControl, move |connection| {
                session_workspace(connection, session)
            })
            .await
            .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: workspace,
                after: EventCursor {
                    store_id: runtime.store_id(),
                    workspace_id: workspace,
                    sequence: 0,
                },
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![
                        InputPart::text("inspect the attached file".to_owned()),
                        InputPart::WorkspaceFile {
                            path: relative.to_owned(),
                            expected_hash: Some(
                                crate::workspace::content_hash(b"explicit attachment bytes")
                                    .parse()
                                    .unwrap(),
                            ),
                            range: None,
                        },
                    ],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::RunFinished { outcome, .. } = event.event {
                    break outcome;
                }
            }
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
        assert!(
            matches!(outcome, RunOutcome::Completed),
            "{relative}: {outcome:?}"
        );
        let prompts = prompts.lock().unwrap();
        assert!(!prompts.is_empty());
        assert!(
            prompts[0].contains("explicit attachment bytes"),
            "{relative}: {}",
            prompts[0]
        );
        assert_eq!(
            std::fs::read_to_string(source.join(relative)).unwrap(),
            if edit {
                "updated attachment"
            } else {
                "explicit attachment bytes"
            }
        );
        for task in std::fs::read_dir(root.path().join("control")).unwrap() {
            let task = task.unwrap().path();
            if edit {
                assert_eq!(
                    std::fs::read_to_string(task.join(relative)).unwrap(),
                    "updated attachment"
                );
            } else {
                assert!(
                    !task.join(relative).exists(),
                    "source authority must not become task-controlled configuration"
                );
            }
        }
    }
}

struct PublicationReloadGate {
    loader: AttachmentLoader,
    source_loads: std::sync::atomic::AtomicUsize,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}
impl RuntimeLoader for PublicationReloadGate {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let paused = request.execution_workspace.is_none()
            && self
                .source_loads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 1;
        let gate = paused.then(|| {
            (
                self.entered.lock().unwrap().take().unwrap(),
                self.release.lock().unwrap().take().unwrap(),
            )
        });
        let load = self.loader.load(request);
        Box::pin(async move {
            let loaded = load.await?;
            if let Some((entered, release)) = gate {
                let _ = entered.send(());
                release.await.unwrap();
            }
            Ok(loaded)
        })
    }
}

#[tokio::test]
async fn cancellation_during_publication_reload_keeps_edits_private() {
    exercise_publication_cancellation_order(PublicationPause::Reload).await;
}

#[tokio::test]
async fn cancellation_after_publication_authorization_preserves_committed_outcome() {
    exercise_publication_cancellation_order(PublicationPause::Authorized).await;
}

#[tokio::test]
async fn cancellation_before_publication_authorization_settles_without_filesystem_capacity() {
    exercise_publication_cancellation_order(PublicationPause::Capacity).await;
}

#[derive(Clone, Copy)]
enum PublicationPause {
    Reload,
    Capacity,
    Authorized,
}

async fn exercise_publication_cancellation_order(pause: PublicationPause) {
    let after_authorization = matches!(pause, PublicationPause::Authorized);
    let (root, source, _) = tokio::task::spawn_blocking(publication_fixture)
        .await
        .unwrap();
    let (entered, reloading) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let mut release = Some(release);
    let loader = AttachmentLoader {
        path: "vendor/policy.md".to_owned(),
        edit: true,
        prompts: Arc::default(),
    };
    let loader: Arc<dyn RuntimeLoader> = if after_authorization {
        PUBLICATION_GATES.lock().unwrap().push(PublicationGate {
            source: source.to_string_lossy().into_owned(),
            entered,
            release: gate,
        });
        Arc::new(loader)
    } else {
        Arc::new(PublicationReloadGate {
            loader,
            source_loads: std::sync::atomic::AtomicUsize::new(0),
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(Some(gate)),
        })
    };
    let (runtime, session) = open_session(root.path(), &source, loader).await;
    let workspace = runtime
        .inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            session_workspace(connection, session)
        })
        .await
        .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: workspace,
            after: EventCursor {
                store_id: runtime.store_id(),
                workspace_id: workspace,
                sequence: 0,
            },
        })
        .unwrap();
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: session,
                input: vec![
                    InputPart::text("edit the attachment".to_owned()),
                    InputPart::WorkspaceFile {
                        path: "vendor/policy.md".to_owned(),
                        expected_hash: None,
                        range: None,
                    },
                ],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("prompt")
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), reloading)
        .await
        .unwrap()
        .unwrap();
    let held = if matches!(pause, PublicationPause::Capacity) {
        let permits = crate::workspace::blocking_permits();
        let held = permits
            .clone()
            .acquire_many_owned(u32::try_from(permits.available_permits()).unwrap())
            .await
            .unwrap();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        PUBLICATION_WAITERS
            .lock()
            .unwrap()
            .push((source.to_string_lossy().into_owned(), entered));
        release.take().unwrap().send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        Some(held)
    } else {
        None
    };
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    if let Some(release) = release {
        release.send(()).unwrap();
    }
    let settle_seconds = if matches!(pause, PublicationPause::Capacity) {
        1
    } else {
        5
    };
    let settled = tokio::time::timeout(std::time::Duration::from_secs(settle_seconds), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::RunFinished { outcome, .. } = event.event {
                break outcome;
            }
        }
    })
    .await;
    let shutdown_while_held = if matches!(pause, PublicationPause::Capacity) && settled.is_ok() {
        Some(tokio::time::timeout(std::time::Duration::from_secs(1), runtime.shutdown()).await)
    } else {
        None
    };
    // The old implementation also gets its permits back so a red regression
    // cleans up its runtime instead of leaving a blocked publication behind.
    drop(held);
    runtime.shutdown().await.unwrap();
    let outcome = settled.expect("cancellation must settle without filesystem capacity");
    if matches!(pause, PublicationPause::Capacity) {
        assert!(
            matches!(shutdown_while_held, Some(Ok(Ok(())))),
            "shutdown must not need the held filesystem permits: {shutdown_while_held:?}"
        );
    }
    assert!(
        if after_authorization {
            matches!(outcome, RunOutcome::Completed)
        } else {
            matches!(outcome, RunOutcome::Cancelled)
        },
        "publication already authorized={after_authorization}: {outcome:?}",
    );
    assert_eq!(
        std::fs::read_to_string(source.join("vendor/policy.md")).unwrap(),
        if after_authorization {
            "updated attachment"
        } else {
            "original"
        },
        "publication must follow its durable ordering with cancellation"
    );
    let tasks = std::fs::read_dir(root.path().join("control"))
        .unwrap()
        .map(|task| task.unwrap().path().join("vendor/policy.md"))
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&tasks[0]).unwrap(),
        "updated attachment"
    );
}
