#[cfg(target_os = "linux")]
use super::execution_context::pending_process_call;
use super::execution_context::process_call_fixture;
use super::*;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn reopen_preserves_completed_native_results_without_reexecuting_commands() {
    use crate::command_owner::{
        CommandOwner, CommandOwnerError, CommandStatus, NativeCommand, NativeEnvironment,
        NativeWorkspaceIdentity,
    };
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sessions.sqlite3");
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(database.clone()).await.unwrap();
    let arguments = r#"{"program":"/bin/sh","args":["-c","printf once >> counter; printf recovered-result; seq 1 10000"]}"#;
    let (claimed, id) = pending_process_call(&store, &workspace, "exec", arguments).await;
    let run = claimed.identity.run_id;
    let owner = CommandOwner::open(
        directory.path().join("owner"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    store
        .set_native_owner(claimed.identity, id, Some(owner.store_id()))
        .await
        .unwrap();
    owner
        .submit(NativeCommand {
            id,
            workspace: workspace.clone(),
            workspace_identity: NativeWorkspaceIdentity::capture(&workspace).unwrap(),
            tool: "exec".to_owned(),
            arguments: arguments.to_owned(),
            shell_policy: crate::ShellPolicy::default(),
            approved_images: Vec::new(),
            environment: NativeEnvironment::capture(&[]).unwrap(),
            not_after_ms: None,
        })
        .await
        .unwrap();
    let mut version = 0;
    loop {
        let observed = owner.wait(id, version).await.unwrap();
        if observed.status == CommandStatus::Finished {
            assert!(observed.result.unwrap().text().contains("recovered-result"));
            break;
        }
        version = observed.version;
    }
    // The process stopped before its observation reached the session store.
    store.close().await.unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut options = SessionRuntimeOptions::new(database);
    options.native_commands = Some(Arc::new(owner.clone()));
    let reopened = SessionRuntime::open(
        options,
        Arc::new(ScriptedRunsLoader {
            requests: requests.clone(),
            runs: Vec::new(),
            loads: StdMutex::new(0),
        }),
    )
    .await
    .unwrap();
    wait_for_recovery(&reopened, run).await;
    let (state, result, receipt): (String, String, Option<String>) = reopened.inner.store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT state, result, execution_context_json FROM tool_calls WHERE id=?1 AND run_id=?2", params![id.to_string(),run.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?)
    }).await.unwrap();
    reopened.shutdown().await.unwrap();
    assert_eq!(
        state, "completed",
        "a completed worker result must survive session recovery"
    );
    assert!(result.contains("recovered-result"), "{result}");
    assert!(
        result.contains("full output t:exec:"),
        "recovery must finalize the retained spill handle"
    );
    assert!(
        receipt.is_some(),
        "recovery must preserve the original process evidence"
    );
    assert!(matches!(
        owner.snapshot(id).await,
        Err(CommandOwnerError::Retired)
    ));
    owner.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("counter")).unwrap(),
        "once"
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "reconciliation must never invoke a model or replay effects"
    );
}

#[tokio::test]
async fn unavailable_or_replaced_native_workers_do_not_block_startup_or_replay_effects() {
    use crate::command_owner::{
        CommandOwnerError, CommandSnapshot, NativeCommand, NativeCommandFuture,
        NativeCommandService,
    };
    #[derive(Clone, Copy)]
    enum Fault {
        Unavailable,
        Pending,
        Replaced,
    }
    struct Worker(Fault);
    impl NativeCommandService for Worker {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            let fault = self.0;
            Box::pin(async move {
                match fault {
                    Fault::Unavailable => Err(CommandOwnerError::Unavailable),
                    Fault::Pending => std::future::pending().await,
                    Fault::Replaced => Ok(StoreId::from_bytes([8; 16])),
                }
            })
        }
        fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
            panic!("recovery must never replay effects")
        }
        fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
            panic!("recovery must never signal another worker")
        }
        fn wait(&self, _: ToolCallId, _: u64) -> NativeCommandFuture<CommandSnapshot> {
            panic!("an unavailable or replaced worker must not be queried")
        }
        fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            panic!("unobserved results must not be acknowledged")
        }
    }
    for fault in [Fault::Unavailable, Fault::Pending, Fault::Replaced] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let store = Store::open(database.clone()).await.unwrap();
        let (claimed, id, _) = process_call_fixture(&store, directory.path()).await;
        store
            .set_native_owner(claimed.identity, id, Some(StoreId::from_bytes([7; 16])))
            .await
            .unwrap();
        store.close().await.unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut options = SessionRuntimeOptions::new(database);
        options.native_commands = Some(Arc::new(Worker(fault)));
        tokio::time::pause();
        let reopened = SessionRuntime::open(
            options,
            Arc::new(ScriptedRunsLoader {
                requests: requests.clone(),
                runs: Vec::new(),
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        wait_for_recovery(&reopened, claimed.identity.run_id).await;
        tokio::time::resume();
        let (state, pending): (String, bool) = reopened.inner.store.call(Priority::Control, move |connection| {
            Ok(connection.query_row("SELECT state, EXISTS(SELECT 1 FROM native_command_owners WHERE tool_call_id=?1 AND result_saved=0) FROM tool_calls WHERE id=?1", [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?)))?)
        }).await.unwrap();
        reopened.shutdown().await.unwrap();
        assert_eq!(state, "interrupted");
        assert!(pending, "unknown command evidence must remain unreclaimed");
        assert!(requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn reopening_reclaims_lost_or_unadmitted_invocations_without_replay() {
    use crate::command_owner::{CommandOwner, CommandOwnerError};
    for lost in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let store = Store::open(database.clone()).await.unwrap();
        let (claimed, id, _) = process_call_fixture(&store, directory.path()).await;
        let owner_path = directory.path().join("owner");
        let owner = CommandOwner::open(
            owner_path.clone(),
            "unknown",
            crate::command_owner::tests::launcher(),
        )
        .await
        .unwrap();
        store
            .set_native_owner(claimed.identity, id, Some(owner.store_id()))
            .await
            .unwrap();
        if lost {
            owner.cancel(id).await.unwrap();
            tokio::task::spawn_blocking(move || {
                let db = Connection::open(owner_path.join("commands.sqlite3")).unwrap();
                db.execute(
                    "UPDATE commands SET status='lost', result=NULL, version=version+1 WHERE id=?1",
                    [id.to_string()],
                )
                .unwrap();
            })
            .await
            .unwrap();
        }
        store.close().await.unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut options = SessionRuntimeOptions::new(database);
        options.native_commands = Some(Arc::new(owner.clone()));
        let reopened = SessionRuntime::open(
            options,
            Arc::new(ScriptedRunsLoader {
                requests: requests.clone(),
                runs: Vec::new(),
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        wait_for_recovery(&reopened, claimed.identity.run_id).await;
        reopened.shutdown().await.unwrap();
        let (state, result): (String, String) = reopened
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT state,result FROM tool_calls WHERE id=?1",
                    [id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            state, "failed",
            "terminal worker evidence must survive recovery"
        );
        assert!(
            result.contains(if lost {
                "not replayed"
            } else {
                "cancelled before admission"
            }),
            "{result}"
        );
        assert!(matches!(
            owner.snapshot(id).await,
            Err(CommandOwnerError::Retired)
        ));
        owner.shutdown().await.unwrap();
        assert!(requests.lock().unwrap().is_empty());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn reopening_observes_a_still_running_command_until_its_original_result_is_saved() {
    use crate::command_owner::{
        CommandOwner, CommandStatus, NativeCommand, NativeEnvironment, NativeWorkspaceIdentity,
    };
    use std::io::Write as _;
    enum Completion {
        Finish,
        Cancel,
        Shutdown,
    }
    for completion in [Completion::Finish, Completion::Cancel, Completion::Shutdown] {
        let cancel = !matches!(completion, Completion::Finish);
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let release = workspace.join("release");
        crate::tools::create_test_fifo(&release).await;
        let mut gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(release)
            .unwrap();
        let store = Store::open(database.clone()).await.unwrap();
        let arguments = r#"{"command":"printf once >> counter; printf ready; read token < release; printf original-result"}"#;
        let (claimed, id) = pending_process_call(&store, &workspace, "shell", arguments).await;
        let run = claimed.identity.run_id;
        let owner = CommandOwner::open(
            directory.path().join("owner"),
            "unknown",
            crate::command_owner::tests::launcher(),
        )
        .await
        .unwrap();
        store
            .set_native_owner(claimed.identity, id, Some(owner.store_id()))
            .await
            .unwrap();
        owner
            .submit(NativeCommand {
                id,
                workspace: workspace.clone(),
                workspace_identity: NativeWorkspaceIdentity::capture(&workspace).unwrap(),
                tool: "shell".to_owned(),
                arguments: arguments.to_owned(),
                shell_policy: crate::ShellPolicy::default(),
                approved_images: Vec::new(),
                environment: NativeEnvironment::capture(&[]).unwrap(),
                not_after_ms: None,
            })
            .await
            .unwrap();
        let mut version = 0;
        loop {
            let observed = owner.wait(id, version).await.unwrap();
            if observed.status == CommandStatus::Running && observed.output.contains("ready") {
                break;
            }
            assert_ne!(observed.status, CommandStatus::Finished);
            version = observed.version;
        }
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SteerRun {
                    run_id: run,
                    input: vec![InputPart::text("steering recorded before the crash")],
                    interrupt: false,
                },
            )
            .await
            .unwrap();
        store.close().await.unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut options = SessionRuntimeOptions::new(database);
        options.native_commands = Some(Arc::new(owner.clone()));
        let reopened = SessionRuntime::open(
            options,
            Arc::new(ScriptedRunsLoader {
                requests: requests.clone(),
                runs: Vec::new(),
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        let mut barrier =
            Box::pin(reopened.await_recovered_commands(
                claimed.identity.workspace_id,
                claimed.identity.session_id,
            ));
        assert!(futures_util::poll!(barrier.as_mut()).is_pending());
        let live: (String, String) = reopened.inner.store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT r.status,t.state FROM runs r JOIN tool_calls t ON t.run_id=r.id WHERE t.id=?1", [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?)))?)
    }).await.unwrap();
        assert!(
            futures_util::poll!(barrier.as_mut()).is_pending(),
            "explicit resume must wait for the original invocation"
        );
        assert!(
            matches!(
                reopened
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::SteerRun {
                            run_id: run,
                            input: vec![InputPart::text("this run has no model executor")],
                            interrupt: true,
                        }
                    )
                    .await,
                Err(SessionRuntimeError::RunNotSteerable)
            ),
            "observation alone cannot consume steering"
        );
        let queued = if cancel {
            if matches!(completion, Completion::Shutdown) {
                reopened.shutdown().await.unwrap();
            } else {
                reopened
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::CancelRun { run_id: run },
                    )
                    .await
                    .unwrap();
            }
            None
        } else {
            let receipt = reopened
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id: claimed.identity.session_id,
                        input: vec![InputPart::text("continue")],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
                panic!("expected queued continuation");
            };
            assert!(
                requests.lock().unwrap().is_empty(),
                "a continuation must not race the recovered invocation"
            );
            tokio::task::spawn_blocking(move || gate.write_all(b"continue\n").unwrap())
                .await
                .unwrap();
            Some(run_id)
        };
        let mut version = 0;
        loop {
            let observed = match owner.wait(id, version).await {
                Ok(snapshot) => snapshot,
                Err(crate::command_owner::CommandOwnerError::Retired) => break,
                Err(error) => panic!("could not observe fixture completion: {error}"),
            };
            if observed.status == CommandStatus::Finished {
                break;
            }
            version = observed.version;
        }
        assert_eq!(
            live,
            ("running".to_owned(), "running".to_owned()),
            "recovery must retain the original pending command instead of fabricating interruption"
        );
        wait_for_recovery(&reopened, run).await;
        barrier.await.unwrap();
        let superseded: (u64,u64) = reopened.inner.store.call(Priority::Control, move |connection| {
            Ok((connection.query_row("SELECT COUNT(*) FROM messages WHERE run_id=?1 AND steering=1 AND state='queued'", [run.to_string()], |row| row.get(0))?,
                connection.query_row("SELECT COUNT(*) FROM events WHERE json_extract(envelope_json,'$.run_id')=?1 AND json_extract(envelope_json,'$.event.type')='steering_superseded'", [run.to_string()], |row| row.get(0))?))
        }).await.unwrap();
        assert_eq!(
            superseded,
            (0, 1),
            "pre-crash steering must be visibly superseded"
        );
        let (state, result): (String, String) = reopened
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT state,result FROM tool_calls WHERE id=?1",
                    [id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(state, if cancel { "failed" } else { "completed" });
        assert!(
            result.contains(if cancel {
                "cancelled"
            } else {
                "original-result"
            }),
            "{result}"
        );
        if let Some(queued) = queued {
            wait_for_recovery(&reopened, queued).await;
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].messages().iter().flat_map(|message| message.content()).any(|block| matches!(block, ContentBlock::ToolResult {content,..} if content.contains("original-result"))));
        } else {
            assert!(requests.lock().unwrap().is_empty());
        }
        if !matches!(completion, Completion::Shutdown) {
            reopened.shutdown().await.unwrap();
        }
        owner.shutdown().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("counter")).unwrap(),
            "once"
        );
    }
}

async fn wait_for_recovery(runtime: &SessionRuntime, run: RunId) {
    let mut settlements = runtime.inner.settlements.subscribe();
    loop {
        let settled = runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT status NOT IN ('running','queued') FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        if settled {
            break;
        }
        assert!(
            !*runtime.inner.failed.borrow(),
            "native result recovery failed"
        );
        settlements.changed().await.unwrap();
    }
}
