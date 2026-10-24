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
    let database = directory
        .path()
        .canonicalize()
        .unwrap()
        .join("sessions.sqlite3");
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(database.clone()).await.unwrap();
    let arguments = r#"{"program":"/bin/sh","args":["-c","printf once >> counter; printf recovered-result; seq 1 10000"]}"#;
    let (claimed, id) = pending_process_call(&store, &workspace, "exec", arguments).await;
    let run = claimed.identity.run_id;
    let owner = CommandOwner::open(
        directory.path().canonicalize().unwrap().join("owner"),
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
            network: crate::command_owner::NativeNetwork::Host,
            resources: crate::command_owner::NativeResources::Host,
            filesystem: crate::command_owner::NativeFilesystem::Host,
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
        let database = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        let store = Store::open(database.clone()).await.unwrap();
        let (claimed, id, _) = process_call_fixture(&store, directory.path()).await;
        store
            .set_native_owner(claimed.identity, id, Some(StoreId::from_bytes([7; 16])))
            .await
            .unwrap();
        store.close().await.unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        for fail_runtime in [false, true] {
            let mut options = SessionRuntimeOptions::new(database.clone());
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
            let mut barrier = Box::pin(reopened.await_recovered_commands(
                claimed.identity.workspace_id,
                claimed.identity.session_id,
            ));
            assert!(futures_util::poll!(barrier.as_mut()).is_pending());
            let (state, pending): (String, bool) = reopened.inner.store.call(Priority::Control, move |connection| {
                Ok(connection.query_row("SELECT state, EXISTS(SELECT 1 FROM native_command_owners WHERE tool_call_id=?1 AND result_saved=0) FROM tool_calls WHERE id=?1", [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?)))?)
            }).await.unwrap();
            // The intervening store call drains the barrier's initial read;
            // this poll registers its wait before shutdown stops observation.
            assert!(futures_util::poll!(barrier.as_mut()).is_pending());
            if fail_runtime {
                reopened.inner.failed.send_replace(true);
            } else {
                reopened.shutdown().await.unwrap();
            }
            let result = match futures_util::poll!(barrier.as_mut()) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    // Drain the authoritative recheck after the shutdown wake.
                    reopened
                        .inner
                        .store
                        .call(Priority::Control, |_| Ok(()))
                        .await
                        .unwrap();
                    match futures_util::poll!(barrier.as_mut()) {
                        std::task::Poll::Ready(result) => result,
                        std::task::Poll::Pending => {
                            panic!("runtime termination left the recovery barrier asleep")
                        }
                    }
                }
            };
            assert!(matches!(result, Err(SessionRuntimeError::Unavailable)));
            assert_eq!(state, "interrupted");
            assert!(pending, "unknown command evidence must remain unreclaimed");
            assert!(requests.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn reopening_retains_lost_uncertainty_but_reclaims_unadmitted_invocations() {
    use crate::command_owner::{CommandOwner, CommandOwnerError};
    for lost in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        let store = Store::open(database.clone()).await.unwrap();
        let (claimed, id, _) = process_call_fixture(&store, directory.path()).await;
        let owner_path = directory.path().canonicalize().unwrap().join("owner");
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
        let (state, result, unresolved): (String, String, bool) = reopened
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT state,result,EXISTS(SELECT 1 FROM native_command_owners WHERE tool_call_id=?1 AND result_saved=0) FROM tool_calls WHERE id=?1",
                    [id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .await
            .unwrap();
        if lost {
            assert_eq!(state, "interrupted");
            assert!(unresolved, "journal loss does not attest native teardown");
            assert_eq!(
                owner.snapshot(id).await.unwrap().status,
                crate::command_owner::CommandStatus::Lost
            );
        } else {
            assert_eq!(
                state, "failed",
                "genuine terminal evidence must survive recovery"
            );
            assert!(result.contains("cancelled before admission"), "{result}");
            assert!(!unresolved);
            assert!(matches!(
                owner.snapshot(id).await,
                Err(CommandOwnerError::Retired)
            ));
        }
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
        let database = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let release = workspace.join("release");
        crate::tools::create_test_fifo(&release).await;
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(release)
            .unwrap();
        let begin_read = workspace.join("begin-read");
        crate::tools::create_test_fifo(&begin_read).await;
        let mut begin_read = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(begin_read)
            .unwrap();
        let store = Store::open(database.clone()).await.unwrap();
        let arguments = r#"{"command":"printf once >> counter; printf ready; read token < begin-read; read token < release; printf original-result"}"#;
        let (claimed, id) = pending_process_call(&store, &workspace, "shell", arguments).await;
        let run = claimed.identity.run_id;
        let owner = CommandOwner::open(
            directory.path().canonicalize().unwrap().join("owner"),
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
                network: crate::command_owner::NativeNetwork::Host,
                resources: crate::command_owner::NativeResources::Host,
                filesystem: crate::command_owner::NativeFilesystem::Host,
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
            let mut writer = gate.try_clone().unwrap();
            tokio::task::spawn_blocking(move || writer.write_all(b"continue\n").unwrap())
                .await
                .unwrap();
            // Delay opening the payload reader until after the write. The
            // original gate must retain the FIFO's buffered token across this gap.
            begin_read.write_all(b"begin\n").unwrap();
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
        drop(gate);
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

#[tokio::test]
async fn late_native_evidence_releases_only_fully_drained_workspaces_without_reaccounting() {
    late_native_evidence_releases_only_fully_drained_workspaces(false, false).await;
}

#[tokio::test]
async fn late_native_evidence_replaces_failed_unobserved_placeholder_without_reaccounting() {
    late_native_evidence_releases_only_fully_drained_workspaces(true, true).await;
}

#[tokio::test]
async fn late_native_evidence_removes_failed_placeholder_spill_when_original_has_no_spill() {
    late_native_evidence_releases_only_fully_drained_workspaces(true, false).await;
}

async fn late_native_evidence_releases_only_fully_drained_workspaces(
    failed_placeholder: bool,
    terminal_spill: bool,
) {
    use crate::command_owner::{
        CommandSnapshot, CommandStatus, NativeCommand, NativeCommandFuture, NativeCommandService,
    };
    struct OriginalWorker {
        snapshots: Vec<CommandSnapshot>,
        release_second: Arc<tokio::sync::Notify>,
    }
    impl NativeCommandService for OriginalWorker {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            Box::pin(async { Ok(StoreId::from_bytes([7; 16])) })
        }
        fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
            panic!("late recovery must never replay effects")
        }
        fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
            panic!("original invocations already have terminal evidence")
        }
        fn wait(&self, id: ToolCallId, version: u64) -> NativeCommandFuture<CommandSnapshot> {
            let index = self
                .snapshots
                .iter()
                .position(|snapshot| snapshot.id == id)
                .unwrap();
            let mut snapshot = self.snapshots[index].clone();
            let release = self.release_second.clone();
            Box::pin(async move {
                if version == 0 {
                    snapshot.version = 1;
                    snapshot.status = CommandStatus::Running;
                    snapshot.result = None;
                } else if index == 1 {
                    release.notified().await;
                }
                Ok(snapshot)
            })
        }
        fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            Box::pin(async { Ok(()) })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let database = workspace.join("sessions.sqlite3");
    let store = Store::open(database.clone()).await.unwrap();
    let first = process_call_fixture(&store, &workspace).await;
    let second = process_call_fixture(&store, &workspace).await;
    let terminal_text = if terminal_spill {
        "original late result\n".repeat(5_000)
    } else {
        "original late result".to_owned()
    };
    let mut snapshots = Vec::new();
    for (claimed, id, receipt) in [&first, &second] {
        store
            .set_native_owner(claimed.identity, *id, Some(StoreId::from_bytes([7; 16])))
            .await
            .unwrap();
        let mut output = crate::tools::bounded_result(terminal_text.clone(), false);
        assert_eq!(output.spill.is_some(), terminal_spill);
        output.execution_context = Some(Box::new(receipt.clone()));
        output.native_outcome =
            Some(crate::command_owner::NativeCommandOutcome::Exited { code: Some(0) });
        snapshots.push(CommandSnapshot {
            id: *id,
            version: 2,
            status: CommandStatus::Finished,
            process: Some(serde_json::from_value(serde_json::to_value(receipt).unwrap()).unwrap()),
            output: "original late result".to_owned(),
            result: Some(serde_json::from_value(serde_json::to_value(output).unwrap()).unwrap()),
        });
    }
    if failed_placeholder {
        let placeholder = crate::cite_spill(
            crate::tools::ToolOutput::unobserved("old missing\n".repeat(6_000)),
            "exec",
            first.1,
            true,
        );
        assert!(placeholder.spill.as_ref().unwrap().text.len() > 64 * 1024);
        assert!(placeholder.model_text.contains("full output t:exec:"));
        store
            .finish_tool_call(
                &first.0,
                first.1,
                placeholder.model_text,
                true,
                Vec::new(),
                None,
                placeholder.spill,
                None,
                None,
            )
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    // No command service is reachable. Neither interruption nor another reopen
    // supplies evidence that these independently owned commands have stopped.
    for _ in 0..2 {
        let store = Store::open(database.clone()).await.unwrap();
        store.recover_interrupted_runs().await.unwrap();
        store.close().await.unwrap();
    }
    let store = Store::open(database.clone()).await.unwrap();
    let independent = tempfile::tempdir().unwrap();
    let (other_workspace, other_session, other) =
        create_claimed_parent(&store, independent.path()).await;
    store
        .finish_run(
            &other,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let mut continuation = None;
    for session_id in [first.0.identity.session_id, other_session] {
        let queued = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::text("continue the work")],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        if session_id == first.0.identity.session_id {
            let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
                panic!("expected continuation")
            };
            continuation = Some(run_id);
        }
    }
    let allowed = store.reserve_next_run(false).await.unwrap().unwrap();
    assert_eq!(
        allowed.identity.workspace_id, other_workspace,
        "unknown execution must fence only its workspace"
    );
    store
        .finish_reserved_run(&allowed, RunOutcome::Cancelled)
        .await
        .unwrap();
    assert!(
        store.reserve_next_run(false).await.unwrap().is_none(),
        "unknown native work must fence replacement work across restarts"
    );
    let run = first.0.identity.run_id;
    let accounting = move |connection: &mut Connection| -> Result<
        (String, Option<String>, Option<u64>, u64),
        SessionRuntimeError,
    > {
        Ok(connection.query_row("SELECT outcome_json,usage_json,estimated_cost_usd_nanos,context_increment_bytes FROM runs WHERE id=?1", [run.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)
    };
    let before = store.call(Priority::Control, accounting).await.unwrap();
    store.close().await.unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut options = SessionRuntimeOptions::new(database);
    options.native_commands = Some(Arc::new(OriginalWorker {
        snapshots,
        release_second: release.clone(),
    }));
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
    wait_for_late_tool(&reopened, first.1).await;
    assert!(
        requests.lock().unwrap().is_empty(),
        "one completed command must not release another command's fence"
    );
    release.notify_one();
    wait_for_late_tool(&reopened, second.1).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_recovery(&reopened, continuation.unwrap()),
    )
    .await
    .unwrap();
    let after = reopened
        .inner
        .store
        .call(Priority::Control, accounting)
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "late evidence must not rewrite settled outcome or accounting"
    );
    let first_id = first.1;
    for (_, id, expected) in [first, second] {
        let (state, result, receipt): (String, String, String) = reopened
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT state,result,execution_context_json FROM tool_calls WHERE id=?1",
                    [id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(state, "completed");
        assert!(result.contains("original late result"));
        if terminal_spill {
            assert!(result.contains("full output t:exec:"));
        }
        assert_eq!(
            serde_json::from_str::<crate::tools::process_context::ExecutionContextReceipt>(
                &receipt
            )
            .unwrap(),
            expected
        );
    }
    let retained: Option<Vec<u8>> = reopened
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok(connection
                .query_row(
                    "SELECT content FROM tool_spills WHERE tool_call_id=?1",
                    [first_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten())
        })
        .await
        .unwrap();
    if terminal_spill {
        assert!(terminal_text.len() > 64 * 1024);
        assert_eq!(
            retained.as_deref(),
            Some(terminal_text.as_bytes()),
            "original full output must replace the unobserved placeholder spill"
        );
    } else {
        assert!(
            retained.is_none(),
            "a short original result cannot retain the placeholder's obsolete spill"
        );
    }
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "only the queued continuation may invoke the model"
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn transient_native_outage_recovers_same_owner_without_another_restart() {
    use crate::command_owner::{
        CommandOwnerError, CommandSnapshot, CommandStatus, NativeCommand, NativeCommandFuture,
        NativeCommandService,
    };
    struct ReturningWorker {
        available: Arc<std::sync::atomic::AtomicBool>,
        snapshot: CommandSnapshot,
    }
    impl NativeCommandService for ReturningWorker {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            let available = self.available.clone();
            Box::pin(async move {
                if available.load(std::sync::atomic::Ordering::SeqCst) {
                    Ok(StoreId::from_bytes([7; 16]))
                } else {
                    Err(CommandOwnerError::Unavailable)
                }
            })
        }
        fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
            panic!("reconnection must not replay execution")
        }
        fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
            panic!("a transient outage is not cancellation")
        }
        fn wait(&self, id: ToolCallId, version: u64) -> NativeCommandFuture<CommandSnapshot> {
            let mut snapshot = self.snapshot.clone();
            assert_eq!(snapshot.id, id);
            Box::pin(async move {
                if version == 0 {
                    snapshot.version = 1;
                    snapshot.status = CommandStatus::Running;
                    snapshot.result = None;
                }
                Ok(snapshot)
            })
        }
        fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            Box::pin(async { Ok(()) })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let database = directory
        .path()
        .canonicalize()
        .unwrap()
        .join("sessions.sqlite3");
    let store = Store::open(database.clone()).await.unwrap();
    let (claimed, id, receipt) = process_call_fixture(&store, directory.path()).await;
    store
        .set_native_owner(claimed.identity, id, Some(StoreId::from_bytes([7; 16])))
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut output = crate::tools::ToolOutput::unobserved("returned original result");
    output.disposition = crate::tools::ResultDisposition::Observed;
    output.is_error = false;
    output.execution_context = Some(Box::new(receipt.clone()));
    let available = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut options = SessionRuntimeOptions::new(database);
    options.native_commands = Some(Arc::new(ReturningWorker {
        available: available.clone(),
        snapshot: CommandSnapshot {
            id,
            version: 2,
            status: CommandStatus::Finished,
            process: Some(serde_json::from_value(serde_json::to_value(&receipt).unwrap()).unwrap()),
            output: "returned original result".to_owned(),
            result: Some(serde_json::from_value(serde_json::to_value(output).unwrap()).unwrap()),
        },
    }));
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
    // Settling the original model run proves the first failed recovery pass
    // completed; only the original worker's evidence can resolve its command.
    tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_recovery(&reopened, claimed.identity.run_id),
    )
    .await
    .unwrap();
    available.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_for_late_tool(&reopened, id).await;
    let (outcome, result, context): (String, String, String) = reopened.inner.store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT r.outcome_json,t.result,t.execution_context_json FROM runs r JOIN tool_calls t ON t.run_id=r.id WHERE t.id=?1", [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?)
    }).await.unwrap();
    assert_eq!(
        serde_json::from_str::<RunOutcome>(&outcome).unwrap(),
        RunOutcome::Interrupted
    );
    assert_eq!(result, "returned original result");
    assert_eq!(
        serde_json::from_str::<crate::tools::process_context::ExecutionContextReceipt>(&context)
            .unwrap(),
        receipt
    );
    assert!(requests.lock().unwrap().is_empty());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn newly_unknown_native_work_refuses_an_already_reserved_prompt_start() {
    let directory = TempDir::new().unwrap();
    let store = Store::open(
        directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3"),
    )
    .await
    .unwrap();
    let (original, id, _) = process_call_fixture(&store, directory.path()).await;
    let (_, session, candidate) = create_claimed_parent(&store, directory.path()).await;
    store
        .finish_run(
            &candidate,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: session,
                input: vec![InputPart::text("replacement")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let reserved = store.reserve_next_run(false).await.unwrap().unwrap();
    store
        .set_native_owner(original.identity, id, Some(StoreId::from_bytes([7; 16])))
        .await
        .unwrap();
    store
        .finish_run(
            &original,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let result = store
        .start_reserved_run(&reserved, test_prepared_audit(&reserved), None)
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(claim::GoalStartRefusal::NativeRecoveryPending)
    ));
    let run = reserved.identity.run_id;
    let (state, outcome, preparing): (String, Option<String>, Option<String>) = store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT r.status,r.outcome_json,s.preparing_run_id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1", [run.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?)
    }).await.unwrap();
    assert_eq!(state, "queued");
    assert!(outcome.is_none());
    assert!(
        preparing.is_none(),
        "the refused reservation must be released for later admission"
    );
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    store.close().await.unwrap();
}

async fn reserve_check_for_native_recovery(store: &Store, path: &Path) -> ClaimedRun {
    let (workspace, session, initial) = create_claimed_parent(store, path).await;
    store
        .finish_run(
            &initial,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let store_id = store.store_id();
    store
        .call_write(Priority::Control, move |connection| {
            let transaction = store::begin_unit(connection)?;
            let mut goal = crate::sessions::goals::set(
                &transaction,
                session,
                crate::sessions::goals::Specification {
                    objective: "verify original published work".into(),
                    check: Some(qq_protocol::GoalCheck {
                        command: "printf verified".into(),
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
                workspace,
                &mut goal,
                now_ms(),
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    store.reserve_next_run(false).await.unwrap().unwrap()
}

#[tokio::test]
async fn newly_unknown_native_work_refuses_an_already_reserved_check_start() {
    let directory = TempDir::new().unwrap();
    let store = Store::open(
        directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3"),
    )
    .await
    .unwrap();
    let (original, id, _) = process_call_fixture(&store, directory.path()).await;
    let check = reserve_check_for_native_recovery(&store, directory.path()).await;
    store
        .set_native_owner(original.identity, id, Some(StoreId::from_bytes([7; 16])))
        .await
        .unwrap();
    store
        .finish_run(
            &original,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let store_id = store.store_id();
    store
        .call_write(Priority::Control, move |connection| {
            assert!(matches!(
                crate::sessions::goal_check::try_start(
                    connection,
                    store_id,
                    &check,
                    check.check_deadline.unwrap().0,
                    true
                )?,
                crate::sessions::goal_check::Start::Waiting
            ));
            let calls: u64 = connection.query_row(
                "SELECT COUNT(*) FROM tool_calls WHERE run_id=?1",
                [check.identity.run_id.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(calls, 0, "a fenced check cannot admit a native invocation");
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn late_interrupted_check_result_preserves_goal_and_human_acknowledgment() {
    late_check_result_preserves_goal_and_human_acknowledgment(false).await;
}

#[tokio::test]
async fn late_failed_check_result_preserves_goal_and_human_acknowledgment() {
    late_check_result_preserves_goal_and_human_acknowledgment(true).await;
}

async fn late_check_result_preserves_goal_and_human_acknowledgment(failed_placeholder: bool) {
    let directory = TempDir::new().unwrap();
    let store = Store::open(
        directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3"),
    )
    .await
    .unwrap();
    let check = reserve_check_for_native_recovery(&store, directory.path()).await;
    let store_id = store.store_id();
    let captured = check.clone();
    let id = store
        .call_write(Priority::Control, move |connection| {
            match crate::sessions::goal_check::try_start(
                connection,
                store_id,
                &captured,
                captured.check_deadline.unwrap().0,
                true,
            )? {
                crate::sessions::goal_check::Start::Started(_, Some(id)) => Ok(id),
                _ => Err(SessionRuntimeError::CONSTRAINT),
            }
        })
        .await
        .unwrap();
    let owner = StoreId::from_bytes([7; 16]);
    store
        .set_native_owner(check.identity, id, Some(owner))
        .await
        .unwrap();
    if failed_placeholder {
        // The live observer can lose contact after admission and persist an
        // unobserved error before its unconfirmed lease stops the runtime.
        store
            .finish_tool_call(
                &check,
                id,
                "could not observe native completion check".into(),
                true,
                Vec::new(),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
    }
    store.recover_interrupted_runs().await.unwrap();
    let session = check.identity.session_id;
    let before = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT payload_json FROM session_goals WHERE session_id=?1 AND live=1",
                [session.to_string()],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    let identity = check.identity;
    let events = store
        .call_write(Priority::Control, move |connection| {
            let mut result = crate::tools::ToolOutput::unobserved("original verified result");
            result.is_error = false;
            result.disposition = crate::tools::ResultDisposition::Observed;
            result.native_outcome =
                Some(crate::command_owner::NativeCommandOutcome::Exited { code: Some(0) });
            crate::sessions::goal_check::recover_result(
                connection, store_id, identity, id, result, None, owner,
            )
        })
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
    );
    let (after, outcome, hold, saved): (String, String, bool, bool) = store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT g.payload_json,r.outcome_json,r.check_acknowledgment_pending,n.result_saved FROM session_goals g JOIN runs r ON r.session_id=g.session_id JOIN native_command_owners n ON n.tool_call_id=?2 WHERE g.session_id=?1 AND g.live=1 AND r.id=?3", params![session.to_string(),id.to_string(),identity.run_id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)
    }).await.unwrap();
    assert_eq!(
        after, before,
        "late success must not reinterpret a settled goal"
    );
    assert_eq!(
        serde_json::from_str::<RunOutcome>(&outcome).unwrap(),
        RunOutcome::Interrupted
    );
    assert!(
        hold,
        "human acknowledgment of unknown execution remains required"
    );
    assert!(saved, "original terminal evidence may now be acknowledged");
    let (state, result): (String, String) = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT state,result FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(state, "completed");
    assert_eq!(result, "original verified result");
    store.close().await.unwrap();
}

#[tokio::test]
async fn unresolved_native_capacity_preserves_existing_bindings_and_reopens_after_saved_evidence() {
    let directory = TempDir::new().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let store = Store::open(workspace.join("sessions.sqlite3"))
        .await
        .unwrap();
    let (original, existing, _) = process_call_fixture(&store, &workspace).await;
    let (candidate, next, _) = process_call_fixture(&store, &workspace).await;
    let owner = StoreId::from_bytes([7; 16]);
    store
        .set_native_owner(original.identity, existing, Some(owner))
        .await
        .unwrap();
    store.call_write(Priority::Control, move |connection| {
        connection.execute(
            "WITH RECURSIVE remaining(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM remaining WHERE n<9999)
             INSERT INTO native_command_owners(tool_call_id,owner_id)
             SELECT printf('%032x',n),?1 FROM remaining",
            [owner.to_string()],
        )?;
        Ok(())
    }).await.unwrap();
    assert!(matches!(
        store
            .set_native_owner(candidate.identity, next, Some(owner))
            .await,
        Err(SessionRuntimeError::NativeCommandCapacity)
    ));
    store
        .set_native_owner(original.identity, existing, Some(owner))
        .await
        .unwrap();
    store.call_write(Priority::Control, move |connection| {
        let candidate_bindings: u64 = connection.query_row(
            "SELECT COUNT(*) FROM native_command_owners WHERE tool_call_id=?1",
            [next.to_string()], |row| row.get(0),
        )?;
        assert_eq!(candidate_bindings, 0, "capacity refusal cannot partially bind an invocation");
        connection.execute(
            "UPDATE native_command_owners SET result_saved=1 WHERE tool_call_id=printf('%032x',1)", [],
        )?;
        Ok(())
    }).await.unwrap();
    store
        .set_native_owner(candidate.identity, next, Some(owner))
        .await
        .unwrap();
    let unresolved: u64 = store
        .call(Priority::Control, |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM native_command_owners WHERE result_saved=0",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(unresolved, 10_000);
    store.close().await.unwrap();
}

async fn wait_for_late_tool(runtime: &SessionRuntime, id: ToolCallId) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut settlements = runtime.inner.settlements.subscribe();
        loop {
            let finished = runtime.inner.store.call(Priority::Control, move |connection| {
                Ok(connection.query_row("SELECT state='completed' AND NOT EXISTS(SELECT 1 FROM native_command_owners WHERE tool_call_id=?1 AND result_saved=0) FROM tool_calls WHERE id=?1", [id.to_string()], |row| row.get::<_, bool>(0))?)
            }).await.unwrap();
            if finished { return; }
            assert!(!*runtime.inner.failed.borrow(), "late native observation failed");
            settlements.changed().await.unwrap();
        }
    }).await.unwrap();
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
