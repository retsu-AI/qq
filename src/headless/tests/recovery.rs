use super::*;
use qq_core::command_owner::{
    CommandLauncher, CommandOwner, CommandOwnerError, CommandSnapshot, CommandStatus,
    NativeCommand, NativeCommandFuture, NativeCommandService,
};

struct InheritedWorker {
    owner: CommandOwner,
    stopped: tokio::sync::watch::Sender<bool>,
    observed: Arc<tokio::sync::Notify>,
    submissions: Arc<std::sync::atomic::AtomicUsize>,
}
impl NativeCommandService for InheritedWorker {
    fn identity(&self) -> NativeCommandFuture<qq_protocol::StoreId> {
        self.owner.identity()
    }
    fn acknowledge(&self, ids: Vec<qq_protocol::ToolCallId>) -> NativeCommandFuture<()> {
        NativeCommandService::acknowledge(&self.owner, ids)
    }
    fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
        self.submissions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let owner = self.owner.clone();
        Box::pin(async move { owner.cancel(request.id).await })
    }
    fn cancel(&self, id: qq_protocol::ToolCallId) -> NativeCommandFuture<()> {
        self.stopped.send_replace(true);
        NativeCommandService::cancel(&self.owner, id)
    }
    fn wait(
        &self,
        id: qq_protocol::ToolCallId,
        after: u64,
    ) -> NativeCommandFuture<CommandSnapshot> {
        let mut stopped = self.stopped.subscribe();
        let observed = self.observed.clone();
        let owner = self.owner.clone();
        Box::pin(async move {
            if after == 0 && !*stopped.borrow() {
                return Ok(CommandSnapshot {
                    id,
                    version: 1,
                    status: CommandStatus::Running,
                    process: None,
                    output: String::new(),
                    result: None,
                });
            }
            observed.notify_one();
            stopped
                .wait_for(|stopped| *stopped)
                .await
                .map(|_| ())
                .map_err(|_| CommandOwnerError::Unavailable)?;
            owner.wait(id, 0).await
        })
    }
}
struct OneNativeCall;
impl Provider for OneNativeCall {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(ProviderEvent::ToolCallStarted {
                id: "native".to_owned(),
                name: "shell".to_owned(),
            }),
            Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: "native".to_owned(),
                json: r#"{"command":"unused"}"#.to_owned(),
            }),
            Ok(ProviderEvent::ToolCallCompleted {
                id: "native".to_owned(),
            }),
            Ok(ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
            }),
        ]))
    }
}

#[tokio::test]
async fn interrupt_during_inherited_command_wait_cancels_and_drains_prompt_and_goal() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for goal_mode in [false,true] {
            let directory = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(directory.path()).unwrap();
            let workspace = root.join("work");
            std::fs::create_dir(&workspace).unwrap();
            let database = root.join("sessions.sqlite3");
            let owner = CommandOwner::open(root.join("owner"),"unknown",CommandLauncher::new(std::env::current_exe().unwrap())).await.unwrap();
            let (stopped,_) = tokio::sync::watch::channel(false);
            let observed = Arc::new(tokio::sync::Notify::new());
            let submissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let service = Arc::new(InheritedWorker {owner:owner.clone(),stopped,observed:observed.clone(),submissions:submissions.clone()});
            let mut runtime_options = SessionRuntimeOptions::new(database.clone());
            runtime_options.native_commands = Some(service.clone());
            let loader: Arc<dyn RuntimeLoader> = Arc::new(ProviderLoader(||OneNativeCall));
            let first = SessionRuntime::open(runtime_options,loader.clone()).await.unwrap();
            let mut opts = options(&workspace);
            opts.approval = HeadlessApproval::Full;
            let session = prepare_session(&first,&opts,SessionIntent::Prompt).await.unwrap();
            let receipt = first.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session.session_id,"objective":"complete the inherited task",
                "budget":{"max_goal_runs":3,"deadline_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64+60_000}
            })).unwrap()).await.unwrap();
            let CommandOutcome::GoalUpdated {goal:original_goal,..} = receipt.outcome else {panic!("goal")};
            observed.notified().await;
            first.abandon_for_test().await.unwrap();
            drop(first);
            let mut runtime_options = SessionRuntimeOptions::new(database);
            runtime_options.native_commands = Some(service);
            let second = SessionRuntime::open(runtime_options,loader).await.unwrap();
            let fixture = Fixture {sessions:second,workspace:workspace.clone(),_directory:directory};
            opts.session = Some(session.session_id);
            if goal_mode {
                let mut request = goal_request();
                request.objective = None;
                request.max_runs = None;
                let mut stdout = Vec::new(); let mut stderr = Vec::new();
                let code = run_goal(&fixture.sessions,opts,request,std::future::ready(()),&mut stdout,&mut stderr).await;
                assert_eq!(code,130,"{}",String::from_utf8_lossy(&stderr));
            } else {
                let (status,_,stderr) = run_to_end(&fixture,opts,std::future::ready(())).await;
                assert_eq!(status,HeadlessStatus::Interrupted,"{stderr}");
            }
            let snapshot = fixture.sessions.snapshot(SnapshotRequest::new(session.workspace_id,Some(session.session_id),8,8)).await.unwrap();
            let focused = snapshot.focused.unwrap();
            assert!(focused.runs.iter().all(|run|run.status==RunStatus::Cancelled));
            let goal = focused.goal.unwrap();
            assert_eq!(goal.goal_id,original_goal.goal_id);
            assert!(matches!(goal.status,qq_protocol::GoalStatus::Paused{..}));
            assert_eq!(submissions.load(std::sync::atomic::Ordering::SeqCst),1,"interrupt must not submit a successor");
            fixture.sessions.close().await.unwrap();
            owner.shutdown().await.unwrap();
        }
    }).await.expect("interrupt must not wait for an inherited command to finish by itself");
}
