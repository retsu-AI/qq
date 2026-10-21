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
async fn unavailable_replaced_or_active_native_workers_are_not_replayed_or_reclaimed() {
    use crate::command_owner::{
        CommandOwnerError, CommandSnapshot, CommandStatus, NativeCommand, NativeCommandFuture,
        NativeCommandService,
    };
    #[derive(Clone, Copy)]
    enum Fault {
        Unavailable,
        Pending,
        Replaced,
        Queued,
        Running,
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
                    Fault::Queued | Fault::Running => Ok(StoreId::from_bytes([7; 16])),
                }
            })
        }
        fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
            panic!("recovery must never replay effects")
        }
        fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
            panic!("recovery must never signal another worker")
        }
        fn wait(&self, id: ToolCallId, _: u64) -> NativeCommandFuture<CommandSnapshot> {
            let status = match self.0 {
                Fault::Queued => CommandStatus::Queued,
                Fault::Running => CommandStatus::Running,
                _ => panic!("an unavailable or replaced worker must not be queried"),
            };
            Box::pin(async move {
                Ok(CommandSnapshot {
                    id,
                    version: 1,
                    status,
                    process: None,
                    output: String::new(),
                    result: None,
                })
            })
        }
        fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            panic!("unobserved results must not be acknowledged")
        }
    }
    for fault in [
        Fault::Unavailable,
        Fault::Pending,
        Fault::Replaced,
        Fault::Queued,
        Fault::Running,
    ] {
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
async fn reopening_reclaims_terminal_loss_without_replaying_the_invocation() {
    use crate::command_owner::{CommandOwner, CommandOwnerError};
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
        "lost evidence must remain unobserved, not a generic interruption"
    );
    assert!(result.contains("not replayed"));
    assert!(matches!(
        owner.snapshot(id).await,
        Err(CommandOwnerError::Retired)
    ));
    owner.shutdown().await.unwrap();
    assert!(requests.lock().unwrap().is_empty());
}
