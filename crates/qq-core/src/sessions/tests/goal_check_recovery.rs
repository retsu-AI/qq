use super::*;
use crate::command_owner::*;

const OWNER: StoreId = StoreId::from_bytes([84; 16]);

async fn pending_check(
    store: &Store,
    workspace: &Path,
    owner: StoreId,
) -> (ClaimedRun, ToolCallId) {
    let mut read_only = Vec::new();
    if cfg!(target_os = "linux") {
        for path in ["/bin", "/usr", "/lib", "/lib64", "/nix/store"] {
            if let Ok(path) = Path::new(path).canonicalize()
                && !read_only
                    .iter()
                    .any(|grant: &NativeDirectoryGrant| grant.path == path)
            {
                read_only.push(NativeDirectoryGrant::capture(&path).unwrap());
            }
        }
    }
    crate::sessions::native_scope::load(
        store,
        vec![NativeCommandScope {
            workspace: NativeDirectoryGrant::capture(workspace).unwrap(),
            read_only,
            network: NativeNetwork::Isolated,
            resources: NativeResources::Host,
        }],
    )
    .await
    .unwrap();
    let (workspace_id, session, _) = create_claimed_parent(store, workspace).await;
    let store_id = store.store_id();
    store
        .call_write(Priority::Control, move |connection| {
            crate::sessions::settlement::recover_interrupted_runs(connection, store_id)?;
            let transaction = store::begin_unit(connection)?;
            let mut goal = crate::sessions::goals::set(
                &transaction,
                session,
                crate::sessions::goals::Specification {
                    objective: "verify the same task after restart".into(),
                    check: Some(qq_protocol::GoalCheck {
                        command: "printf once >> counter; printf verified".into(),
                        timeout_ms: 60_000,
                    }),
                    budget: serde_json::from_value(
                        serde_json::json!({"max_goal_runs":2,"deadline_ms":now_ms()+300_000}),
                    )?,
                },
                None,
                false,
                now_ms(),
                GoalPolicy::default(),
            )?;
            goal.status = qq_protocol::GoalStatus::Active;
            goal.counters.runs = 1;
            goal.audit_pending = true;
            goal.audit_revision = Some(goal.revision);
            goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::Once);
            crate::sessions::goal_check::queue(
                &transaction,
                store_id,
                workspace_id,
                &mut goal,
                now_ms(),
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    let claim = store.reserve_next_run(false).await.unwrap().unwrap();
    let captured = claim.clone();
    let deadline = claim.check_deadline.unwrap().0;
    let id = store
        .call_write(Priority::Control, move |connection| {
            match crate::sessions::goal_check::try_start(
                connection, store_id, &captured, deadline, true,
            )? {
                crate::sessions::goal_check::Start::Started(_, Some(id)) => Ok(id),
                _ => Err(SessionRuntimeError::CONSTRAINT),
            }
        })
        .await
        .unwrap();
    store
        .set_native_owner(claim.identity, id, Some(owner))
        .await
        .unwrap();
    (claim, id)
}

fn result(code: i32) -> crate::tools::ToolOutput {
    let mut result = crate::tools::dispatch::bounded_result("verified-result".into(), code != 0);
    result.native_outcome = Some(NativeCommandOutcome::Exited { code: Some(code) });
    result
}

#[tokio::test]
async fn native_check_result_is_not_reclaimable_before_goal_settlement() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().canonicalize().unwrap();
    let store = Store::open(path.join("sessions.sqlite3")).await.unwrap();
    let (claim, id) = pending_check(&store, &path, OWNER).await;
    store
        .finish_tool_call(
            &claim,
            id,
            "verified-result".into(),
            false,
            vec![],
            None,
            None,
            None,
            Some(OWNER),
        )
        .await
        .unwrap();
    let saved = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT result_saved FROM native_command_owners WHERE tool_call_id=?1",
                [id.to_string()],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await
        .unwrap();
    store.close().await.unwrap();
    assert!(
        !saved,
        "a crash before goal settlement must leave the typed worker result available"
    );
}

struct Worker {
    fault: Option<Fault>,
    snapshot: CommandSnapshot,
    acknowledgements: Arc<StdMutex<Vec<ToolCallId>>>,
}
impl NativeCommandService for Worker {
    fn identity(&self) -> NativeCommandFuture<StoreId> {
        let fault = self.fault;
        Box::pin(async move {
            match fault {
                Some(Fault::UnavailableWorker) => Err(CommandOwnerError::Unavailable),
                Some(Fault::ReplacedWorker) => Ok(StoreId::from_bytes([85; 16])),
                _ => Ok(OWNER),
            }
        })
    }
    fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
        panic!("recovery must not replay a command")
    }
    fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
        Box::pin(async { Ok(()) })
    }
    fn wait(&self, id: ToolCallId, _: u64) -> NativeCommandFuture<CommandSnapshot> {
        assert!(
            !matches!(
                self.fault,
                Some(Fault::ReplacedWorker | Fault::UnavailableWorker)
            ),
            "recovery must not query a different or unavailable journal"
        );
        let snapshot = self.snapshot.clone();
        assert_eq!(snapshot.id, id);
        Box::pin(async move { Ok(snapshot) })
    }
    fn acknowledge(&self, ids: Vec<ToolCallId>) -> NativeCommandFuture<()> {
        self.acknowledgements.lock().unwrap().extend(ids);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn native_check_reopen_settles_the_same_goal_without_replay_or_new_consent() {
    for result_saved in [false, true] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().canonicalize().unwrap();
        let database = path.join("sessions.sqlite3");
        let store = Store::open(database.clone()).await.unwrap();
        let (claim, id) = pending_check(&store, &path, OWNER).await;
        let run = claim.identity.run_id;
        if result_saved {
            store
                .finish_tool_call(
                    &claim,
                    id,
                    "verified-result".into(),
                    false,
                    vec![],
                    None,
                    None,
                    None,
                    Some(OWNER),
                )
                .await
                .unwrap();
        }
        store.close().await.unwrap();
        let acknowledgements = Arc::new(StdMutex::new(Vec::new()));
        let worker = Worker {
            fault: None,
            snapshot: CommandSnapshot {
                id,
                version: 1,
                status: CommandStatus::Finished,
                process: None,
                output: String::new(),
                result: Some(
                    serde_json::from_value(serde_json::to_value(result(0)).unwrap()).unwrap(),
                ),
            },
            acknowledgements: acknowledgements.clone(),
        };
        let loader = Arc::new(ScriptedRunsLoader {
            requests: Arc::default(),
            runs: vec![],
            loads: StdMutex::new(0),
        });
        let mut options = SessionRuntimeOptions::new(database.clone());
        options.native_commands = Some(Arc::new(worker));
        let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: claim.identity.workspace_id,
                after: EventCursor {
                    store_id: runtime.store_id(),
                    workspace_id: claim.identity.workspace_id,
                    sequence: 0,
                },
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if matches!(event.event,SessionEvent::RunFinished {run_id,..} if run_id==run) {
                    break;
                }
            }
        })
        .await
        .expect("recovered check settles");
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(
                claim.identity.workspace_id,
                Some(claim.identity.session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(
            snapshot
                .tool_calls
                .iter()
                .filter(|call| call.id == id)
                .count(),
            1
        );
        runtime.shutdown().await.unwrap();
        let goal = snapshot.goal.unwrap();
        assert_eq!(goal.goal_id, claim.goal.as_ref().unwrap().goal_id);
        assert_eq!(
            goal.status,
            qq_protocol::GoalStatus::Achieved,
            "saved result: {result_saved}"
        );
        assert_eq!(goal.counters.audits, 1);
        assert_eq!(
            goal.check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::Consumed)
        );
        assert!(acknowledgements.lock().unwrap().contains(&id));
        assert_eq!(
            *loader.loads.lock().unwrap(),
            0,
            "recovery must not load a provider"
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum Fault {
    Cancelled,
    Expired,
    Edited,
    ReplacedWorkspace,
    UnprovenOutcome,
    MissingService,
    ReplacedWorker,
    UnavailableWorker,
}

#[tokio::test]
async fn native_check_recovery_preserves_cancellation_budgets_and_unknown_execution_holds() {
    for fault in [
        Fault::Cancelled,
        Fault::Expired,
        Fault::Edited,
        Fault::ReplacedWorkspace,
        Fault::UnprovenOutcome,
        Fault::MissingService,
        Fault::ReplacedWorker,
        Fault::UnavailableWorker,
    ] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().canonicalize().unwrap();
        let workspace = path.join("task");
        std::fs::create_dir(&workspace).unwrap();
        let database = path.join("sessions.sqlite3");
        let store = Store::open(database.clone()).await.unwrap();
        let (claim, id) = pending_check(&store, &workspace, OWNER).await;
        let run = claim.identity.run_id;
        let session = claim.identity.session_id;
        store
            .call_write(Priority::Control, move |connection| {
                match fault {
                    Fault::Cancelled => {
                        connection.execute(
                            "UPDATE runs SET cancel_requested=1 WHERE id=?1",
                            [run.to_string()],
                        )?;
                    }
                    Fault::Expired | Fault::Edited => {
                        let mut goal =
                            crate::sessions::goals::load_live(connection, session)?.unwrap();
                        if matches!(fault, Fault::Expired) {
                            goal.budget.deadline_ms = 0;
                        } else {
                            goal.revision += 1;
                            goal.status = qq_protocol::GoalStatus::Paused {
                                reason: qq_protocol::GoalPauseReason::User,
                            };
                        }
                        crate::sessions::goals::save(connection, &goal)?;
                    }
                    _ => {}
                }
                Ok(())
            })
            .await
            .unwrap();
        store.close().await.unwrap();
        if matches!(fault, Fault::ReplacedWorkspace) {
            std::fs::rename(&workspace, path.join("old-task")).unwrap();
            std::fs::create_dir(&workspace).unwrap();
        }
        let mut output = result(0);
        if matches!(fault, Fault::UnprovenOutcome) {
            output.native_outcome = None;
        }
        let acknowledgements = Arc::new(StdMutex::new(Vec::new()));
        let worker = Worker {
            fault: Some(fault),
            acknowledgements: acknowledgements.clone(),
            snapshot: CommandSnapshot {
                id,
                version: 1,
                status: CommandStatus::Finished,
                process: None,
                output: String::new(),
                result: Some(
                    serde_json::from_value(serde_json::to_value(output).unwrap()).unwrap(),
                ),
            },
        };
        let mut options = SessionRuntimeOptions::new(database);
        if !matches!(fault, Fault::MissingService) {
            options.native_commands = Some(Arc::new(worker));
        }
        let loader = Arc::new(ScriptedRunsLoader {
            requests: Arc::default(),
            runs: vec![],
            loads: StdMutex::new(0),
        });
        let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: claim.identity.workspace_id,
                after: EventCursor {
                    store_id: runtime.store_id(),
                    workspace_id: claim.identity.workspace_id,
                    sequence: 0,
                },
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if matches!(event.event,SessionEvent::RunFinished {run_id,..} if run_id==run) {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{fault:?}: recovery did not settle"));
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(
                claim.identity.workspace_id,
                Some(session),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        let workspace_id = claim.identity.workspace_id;
        let hold = runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goal_check::interruption(connection, workspace_id)?.is_some())
            })
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();
        let goal = snapshot.goal.unwrap();
        assert_ne!(
            goal.status,
            qq_protocol::GoalStatus::Achieved,
            "{fault:?}: stale evidence must not attest current task success"
        );
        assert_eq!(
            goal.counters.audits, 1,
            "{fault:?}: recovery cannot rerun the audit"
        );
        assert_eq!(
            goal.check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::Consumed)
        );
        assert_eq!(
            *loader.loads.lock().unwrap(),
            0,
            "{fault:?}: recovery cannot start another provider run"
        );
        let unknown = matches!(
            fault,
            Fault::ReplacedWorkspace
                | Fault::UnprovenOutcome
                | Fault::MissingService
                | Fault::ReplacedWorker
                | Fault::UnavailableWorker
        );
        assert_eq!(
            hold, unknown,
            "{fault:?}: unknown execution retains the workspace hold"
        );
        assert_eq!(
            acknowledgements.lock().unwrap().contains(&id),
            !unknown,
            "{fault:?}: reclaim only settled typed evidence"
        );
        match fault {
            Fault::Expired => {
                assert_eq!(goal.budget.deadline_ms, 0);
                assert!(matches!(
                    goal.status,
                    qq_protocol::GoalStatus::BudgetExhausted {
                        reason: qq_protocol::GoalExhaustion::Deadline
                    }
                ));
            }
            Fault::Edited => assert_eq!(goal.revision, claim.goal.as_ref().unwrap().revision + 1),
            _ => {}
        }
    }
}

struct LiveWorker {
    state: watch::Sender<CommandSnapshot>,
    entered: Arc<tokio::sync::Notify>,
    acknowledgements: Arc<StdMutex<Vec<ToolCallId>>>,
}
impl NativeCommandService for LiveWorker {
    fn identity(&self) -> NativeCommandFuture<StoreId> {
        Box::pin(async { Ok(OWNER) })
    }
    fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
        panic!("a live command must not be resubmitted")
    }
    fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
        panic!("reconnecting is not cancellation")
    }
    fn wait(&self, id: ToolCallId, after: u64) -> NativeCommandFuture<CommandSnapshot> {
        let mut state = self.state.subscribe();
        let entered = self.entered.clone();
        Box::pin(async move {
            entered.notify_one();
            let snapshot = state
                .wait_for(|snapshot| snapshot.version > after)
                .await
                .map_err(|_| CommandOwnerError::Unavailable)?
                .clone();
            assert_eq!(snapshot.id, id);
            Ok(snapshot)
        })
    }
    fn acknowledge(&self, ids: Vec<ToolCallId>) -> NativeCommandFuture<()> {
        self.acknowledgements.lock().unwrap().extend(ids);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn native_check_reopen_observes_live_execution_and_wakes_on_completion() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().canonicalize().unwrap();
    let database = path.join("sessions.sqlite3");
    let store = Store::open(database.clone()).await.unwrap();
    let (claim, id) = pending_check(&store, &path, OWNER).await;
    store.close().await.unwrap();
    let (state, _) = watch::channel(CommandSnapshot {
        id,
        version: 1,
        status: CommandStatus::Running,
        process: None,
        output: String::new(),
        result: None,
    });
    let entered = Arc::new(tokio::sync::Notify::new());
    let acknowledgements = Arc::new(StdMutex::new(Vec::new()));
    let mut options = SessionRuntimeOptions::new(database);
    options.native_commands = Some(Arc::new(LiveWorker {
        state: state.clone(),
        entered: entered.clone(),
        acknowledgements: acknowledgements.clone(),
    }));
    let loader = Arc::new(ScriptedRunsLoader {
        requests: Arc::default(),
        runs: vec![],
        loads: StdMutex::new(0),
    });
    let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            claim.identity.workspace_id,
            Some(claim.identity.session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    let goal = snapshot.goal.unwrap();
    assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
    assert_eq!(goal.pending_run_id, Some(claim.identity.run_id));
    assert_eq!(goal.counters.audits, 1);
    assert!(acknowledgements.lock().unwrap().is_empty());
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: claim.identity.workspace_id,
            after: EventCursor {
                store_id: runtime.store_id(),
                workspace_id: claim.identity.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    state.send_replace(CommandSnapshot {
        id,
        version: 2,
        status: CommandStatus::Finished,
        process: None,
        output: String::new(),
        result: Some(serde_json::from_value(serde_json::to_value(result(0)).unwrap()).unwrap()),
    });
    tokio::time::timeout(Duration::from_secs(5),async {
        loop { let event=events.next().await.unwrap().unwrap();
            if matches!(event.event,SessionEvent::RunFinished {run_id,outcome:RunOutcome::Completed,..} if run_id==claim.identity.run_id) {break;} }
    }).await.expect("worker completion wakes the original goal");
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            claim.identity.workspace_id,
            Some(claim.identity.session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(
        snapshot.goal.unwrap().status,
        qq_protocol::GoalStatus::Achieved
    );
    runtime.shutdown().await.unwrap();
    assert_eq!(*loader.loads.lock().unwrap(), 0);
    assert!(acknowledgements.lock().unwrap().contains(&id));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_check_real_linux_result_survives_coordinator_loss_without_reexecution() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().canonicalize().unwrap();
    let workspace = path.join("task");
    std::fs::create_dir(&workspace).unwrap();
    let database = path.join("sessions.sqlite3");
    let owner = CommandOwner::open(
        path.join("commands"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    let store = Store::open(database.clone()).await.unwrap();
    let (claim, id) = pending_check(&store, &workspace, owner.store_id()).await;
    let policy = crate::sessions::native_scope::load(&store, vec![])
        .await
        .unwrap()
        .unwrap()
        .get(&workspace)
        .unwrap();
    let arguments = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT arguments_json FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    owner
        .submit(NativeCommand {
            id,
            workspace: workspace.clone(),
            workspace_identity: policy.workspace.identity,
            tool: "shell".into(),
            arguments,
            environment: NativeEnvironment::capture(&[]).unwrap(),
            filesystem: policy.filesystem(),
            network: policy.network,
            resources: policy.resources,
            shell_policy: crate::ShellPolicy::default(),
            approved_images: vec![],
            not_after_ms: Some(claim.check_deadline.unwrap().0),
        })
        .await
        .unwrap();
    let mut version = 0;
    let completed = loop {
        let snapshot = owner.wait(id, version).await.unwrap();
        if snapshot.status == CommandStatus::Finished {
            break snapshot.result.unwrap();
        }
        version = snapshot.version;
    };
    let unavailable = completed
        .text()
        .contains("required Linux filesystem enforcement is unavailable")
        || completed
            .text()
            .contains("required Linux network enforcement is unavailable");
    if !unavailable {
        assert_eq!(
            completed.outcome(),
            Some(NativeCommandOutcome::Exited { code: Some(0) }),
            "{}",
            completed.text()
        );
    }
    // The coordinator never received or saved the terminal worker observation.
    store.close().await.unwrap();
    let mut options = SessionRuntimeOptions::new(database);
    options.native_commands = Some(Arc::new(owner.clone()));
    let loader = Arc::new(ScriptedRunsLoader {
        requests: Arc::default(),
        runs: vec![],
        loads: StdMutex::new(0),
    });
    let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: claim.identity.workspace_id,
            after: EventCursor {
                store_id: runtime.store_id(),
                workspace_id: claim.identity.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10),async {loop {let event=events.next().await.unwrap().unwrap();
        if matches!(event.event,SessionEvent::RunFinished {run_id,..} if run_id==claim.identity.run_id) {break;}
    }}).await.unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            claim.identity.workspace_id,
            Some(claim.identity.session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    runtime.shutdown().await.unwrap();
    if unavailable {
        assert_ne!(
            snapshot.goal.unwrap().status,
            qq_protocol::GoalStatus::Achieved
        );
        assert!(!workspace.join("counter").exists());
    } else {
        assert_eq!(
            snapshot.goal.unwrap().status,
            qq_protocol::GoalStatus::Achieved
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("counter")).unwrap(),
            "once"
        );
        assert!(matches!(
            owner.snapshot(id).await,
            Err(CommandOwnerError::Retired)
        ));
    }
    assert_eq!(*loader.loads.lock().unwrap(), 0);
    owner.shutdown().await.unwrap();
}
