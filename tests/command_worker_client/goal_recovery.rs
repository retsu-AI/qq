//! Finite artifact verification across an actual coordinator SIGKILL.
use super::cli::{Worker, configured_cli, reply};
use futures_util::StreamExt as _;
use qq_protocol::{GoalSnapshot, GoalStatus, SessionEvent, SnapshotRequest};
use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{os::unix::process::ExitStatusExt as _, time::Duration};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

#[test]
fn verify_goal_artifact() {
    if !std::env::args().any(|arg| arg == "qq-goal-verifier") {
        return;
    }
    assert_eq!(std::fs::read_to_string("artifact").unwrap(), "native-once");
    assert_eq!(std::fs::read_to_string("executions").unwrap(), "once\n");
    std::fs::write("verified", "independent-check-passed").unwrap();
}

fn tool_reply(name: &str, arguments: String) -> (u16, Option<&'static str>, Vec<Vec<u8>>) {
    reply(vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":name,"arguments":""}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":name,"arguments":arguments}}),
        completed(),
    ])
}
fn completed() -> Value {
    json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}})
}

#[tokio::test]
async fn server_restart_finishes_the_same_goal_without_replaying_work_or_resetting_budget() {
    // Nextest supplies the hang timeout for this multi-process fixture.
    for max_runs in [3, 1] {
        exercise_goal(max_runs, false).await;
    }
}

#[tokio::test]
async fn default_auto_server_restart_resumes_private_work_without_reexecution() {
    for max_runs in [3, 1] {
        exercise_goal(max_runs, true).await;
    }
}

async fn exercise_goal(max_runs: u32, auto: bool) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let marker = format!("qq-client-{}", qq_protocol::ToolCallId::generate().unwrap());
    let listener = if auto {
        None
    } else {
        Some(tokio::net::UnixListener::bind(workspace.join(&marker)).unwrap())
    };
    let binary = std::env::current_exe().unwrap();
    let arguments = json!({"program":binary,"args":["--exact","cli::blocked_command","--nocapture","--skip",marker]}).to_string();
    let mut replies = vec![if auto {
        tool_reply("shell", json!({"command":"set -e; printf 'once\n' >> executions; mkfifo .qq/task-home/release; printf 'qq-task-ready\n'; read release < .qq/task-home/release; printf native-once > artifact; printf native-result"}).to_string())
    } else {
        tool_reply("exec", arguments)
    }];
    if auto && max_runs > 1 {
        replies.push(tool_reply(
            "read_file",
            json!({"path":"executions"}).to_string(),
        ));
        replies.push(reply(vec![
            json!({"type":"response.output_text.delta","delta":"inspected the running task"}),
            completed(),
        ]));
    }
    if max_runs > 1 {
        replies.push(tool_reply("update_goal", json!({"status":{"type":"achieved","evidence":"the saved command produced the requested artifact; verify it independently"}}).to_string()));
        replies.push(reply(vec![
            json!({"type":"response.output_text.delta","delta":"artifact ready for verification"}),
            completed(),
        ]));
    }
    let provider = LoopbackServer::respond_sequence_with_timeout(replies, Duration::from_secs(30));
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    let checker = if auto {
        "test \"$(cat artifact)\" = native-once && test \"$(cat executions)\" = once".to_owned()
    } else {
        format!(
            "'{}' --exact goal_recovery::verify_goal_artifact --nocapture --skip qq-goal-verifier",
            binary.to_str().unwrap().replace('\'', "'\\''")
        )
    };
    let mut command = configured_cli(root.path(), &workspace, &config);
    command.arg("run");
    if !auto {
        command.args(["--approval", "full", "--goal-check-approval", "for-goal"]);
    }
    let mut original = command
        .args([
            "--format",
            "jsonl",
            "--goal",
            "produce the fixture artifact exactly once and verify it",
            "--check",
            &checker,
            "--goal-max-runs",
            &max_runs.to_string(),
            "--goal-deadline",
            "5m",
        ])
        .spawn()
        .unwrap();
    let mut stream = if let Some(listener) = listener {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"ready");
        Some(stream)
    } else {
        let mut output = BufReader::new(original.stdout.take().unwrap());
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut captured = String::new();
            loop {
                let mut line = String::new();
                assert!(
                    output.read_line(&mut line).await.unwrap() > 0,
                    "command ended before its private barrier: {captured}"
                );
                let line: Value = serde_json::from_str(&line).unwrap();
                if line["envelope"]["event"]["type"] == "tool_call_output_delta" {
                    captured.push_str(line["envelope"]["event"]["chunk"].as_str().unwrap());
                    assert!(captured.len() <= 128 * 1024);
                    if captured.contains("qq-task-ready") {
                        break;
                    }
                }
            }
        })
        .await
        .expect("the Auto command must reach its private barrier");
        None
    };
    let worker = Worker::child_of(original.id().unwrap());
    original.start_kill().unwrap();
    assert_eq!(
        original.wait_with_output().await.unwrap().status.signal(),
        Some(9)
    );
    let database = root.path().join("data/qq/sessions.sqlite3");
    let db = rusqlite::Connection::open(&database).unwrap();
    let (workspace_id, payload): (String, String) = db.query_row("SELECT s.workspace_id,g.payload_json FROM session_goals g JOIN sessions s ON s.id=g.session_id WHERE g.live=1", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    let original_goal: GoalSnapshot = serde_json::from_str(&payload).unwrap();
    assert_eq!(original_goal.budget.max_goal_runs, max_runs);
    assert_eq!(original_goal.status, GoalStatus::Active);
    if auto {
        assert_eq!(
            original_goal.check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::TaskScoped),
            "default Auto derives contained check authority without explicit consent"
        );
    }
    let private = if auto {
        let record: String = db
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!(
                    "qq_task_workspace:{}",
                    original_goal.pending_run_id.unwrap()
                )],
                |row| row.get(0),
            )
            .unwrap();
        let mut record: Value = serde_json::from_str(&record).unwrap();
        let resources = worker.resource_root();
        let groups = std::fs::read_dir(&resources)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .collect::<Vec<_>>();
        assert_eq!(
            groups.len(),
            1,
            "the blocked invocation has one resource group"
        );
        let group = groups[0].path();
        for (name, expected) in [
            ("memory.max", "4294967296"),
            ("memory.swap.max", "0"),
            ("pids.max", "512"),
        ] {
            assert_eq!(
                std::fs::read_to_string(group.join(name)).unwrap().trim(),
                expected
            );
        }
        let cpu = record["scope"]["resources"]["limits"]["cpu_millicores"]
            .as_u64()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(group.join("cpu.max"))
                .unwrap()
                .trim(),
            format!("{} 100000", cpu * 100)
        );
        assert!(
            std::fs::read_to_string(group.join("pids.current"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
                > 0,
            "limits must contain the running invocation, not an empty placeholder group"
        );
        assert_eq!(
            std::fs::read_to_string(resources.join("pids.max"))
                .unwrap()
                .trim(),
            "4096"
        );
        assert!(
            std::fs::read_to_string(resources.join("memory.max"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        let task: qq_core::task_workspace::TaskWorkspace =
            serde_json::from_value(record["workspace"].clone()).unwrap();
        assert!(
            !workspace.join("executions").exists(),
            "in-flight Auto work must stay private"
        );
        if max_runs > 1 {
            // Simulate a tighter host policy after restart. Old persisted
            // authority must not override the current server's resource limits.
            record["scope"]["resources"] = json!({"access":"host"});
            db.execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                rusqlite::params![
                    format!(
                        "qq_task_workspace:{}",
                        original_goal.pending_run_id.unwrap()
                    ),
                    record.to_string()
                ],
            )
            .unwrap();
        }
        Some(task.path().to_owned())
    } else {
        None
    };
    drop(db);

    // Opening only the server must resume the goal. No resume command or new
    // user prompt is sent, and its first invocation remains blocked here.
    let mut server = configured_cli(root.path(), &workspace, &config)
        .args(["serve", "--bind", "127.0.0.1:0"])
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let mut listening = String::new();
    stdout.read_line(&mut listening).await.unwrap();
    assert!(
        listening.starts_with("qq server listening at "),
        "{listening}"
    );
    let connection =
        qq_server::discover_at(&qq_server::ServerPaths::new(root.path().join("runtime/qq")))
            .await
            .unwrap()
            .unwrap();
    let client = qq_client::SessionClient::new(connection).unwrap();
    let workspace_id = workspace_id.parse().unwrap();
    let before = client
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(original_goal.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let current = before.focused.as_ref().unwrap().goal.as_ref().unwrap();
    assert_eq!(current.goal_id, original_goal.goal_id);
    assert_eq!(current.budget, original_goal.budget);
    assert_eq!(current.status, GoalStatus::Active);
    let mut events = client.events(workspace_id, before.cursor).await.unwrap();
    if auto && max_runs > 1 {
        if before
            .focused
            .as_ref()
            .unwrap()
            .summary
            .active_run_id
            .is_none()
        {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::RunStarted { session, .. } = event.event
                    && session.id == original_goal.session_id
                {
                    break;
                }
            }
        }
        assert!(!workspace.join("executions").exists());
        let receipt = client
            .command(
                qq_protocol::CommandId::generate().unwrap(),
                qq_protocol::SessionCommand::SubmitSideQuestion {
                    session_id: original_goal.session_id,
                    question: "read executions from the current task".into(),
                    new_thread: true,
                },
            )
            .await
            .unwrap();
        let qq_protocol::CommandOutcome::SideQuestionSubmitted {
            side_question_id, ..
        } = receipt.outcome
        else {
            panic!("side question must be admitted");
        };
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::SideQuestionUpdated { side_question } = event.event
                && side_question.id == side_question_id
                && side_question.state != qq_protocol::SideQuestionState::Running
            {
                assert_eq!(
                    side_question.state,
                    qq_protocol::SideQuestionState::Completed
                );
                assert_eq!(side_question.answer, "inspected the running task");
                break;
            }
        }
        assert!(!workspace.join("executions").exists());
    }
    if let Some(stream) = stream.as_mut() {
        stream.write_all(b"e").await.unwrap();
    } else {
        let release = private.as_ref().unwrap().join(".qq/task-home/release");
        tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .write(true)
                .open(release)
                .unwrap()
                .write_all(b"continue\n")
                .unwrap();
        })
        .await
        .unwrap();
    }
    let terminal = loop {
        let event = events.next().await.unwrap().unwrap();
        if let SessionEvent::GoalUpdated { goal } = event.event
            && goal.goal_id == original_goal.goal_id
            && goal.status != GoalStatus::Active
        {
            break goal;
        }
    };
    assert_eq!(terminal.goal_id, original_goal.goal_id);
    assert_eq!(
        terminal.budget, original_goal.budget,
        "recovery must not renew the deadline or spending limits"
    );
    if auto && max_runs > 1 {
        let db = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let record: String = db
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!(
                    "qq_task_workspace:{}",
                    original_goal.pending_run_id.unwrap()
                )],
                |row| row.get(0),
            )
            .unwrap();
        let record: Value = serde_json::from_str(&record).unwrap();
        assert_eq!(
            record["scope"]["resources"]["access"], "limited",
            "restored tasks must use the current host policy"
        );
        assert_eq!(
            record["scope"]["workspace"]["path"],
            private.as_ref().unwrap().to_str().unwrap(),
            "restoration must retain the original private tree"
        );
    }
    if max_runs > 1 {
        assert_eq!(terminal.status, GoalStatus::Achieved);
        if !auto {
            assert_eq!(
                std::fs::read_to_string(workspace.join("verified")).unwrap(),
                "independent-check-passed"
            );
        }
        assert_eq!(terminal.counters.runs, 2);
        assert_eq!(terminal.counters.audits, 1);
    } else {
        assert!(matches!(
            terminal.status,
            GoalStatus::BudgetExhausted { .. }
        ));
        assert_eq!(terminal.counters.runs, 1);
        assert!(!workspace.join("verified").exists());
    }
    let artifact_root = if auto && max_runs == 1 {
        assert!(
            !workspace.join("artifact").exists(),
            "exhausted work stays private for explicit continuation"
        );
        private.as_ref().unwrap()
    } else {
        &workspace
    };
    assert_eq!(
        std::fs::read_to_string(artifact_root.join("artifact")).unwrap(),
        "native-once"
    );
    assert_eq!(
        std::fs::read_to_string(artifact_root.join("executions")).unwrap(),
        "once\n"
    );
    drop(events);
    drop(client);
    drop(stdout);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(server.id().unwrap()).unwrap()).unwrap(),
        rustix::process::Signal::INT,
    )
    .unwrap();
    let stopped = server.wait_with_output().await.unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    worker.stop().await;
    if auto {
        let db = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let approvals: u64 = db
            .query_row(
                "SELECT COUNT(*) FROM events WHERE json_extract(envelope_json, '$.event.type') = 'tool_approval_requested'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(approvals, 0, "Auto recovery must never request approval");
    }
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(
        captured.len(),
        if auto && max_runs > 1 {
            5
        } else if max_runs > 1 {
            3
        } else {
            1
        }
    );
    if max_runs > 1 {
        let successor = if auto { 3 } else { 1 };
        assert!(
            captured[successor].body().contains("native-result"),
            "the automatic successor must receive the original command result"
        );
        if auto {
            assert!(
                captured[2].body().contains("read executions L1/1"),
                "the side question must receive a successful read from the still-private tree: {}",
                captured[2].body()
            );
        }
    }
}

/// A loader witness stops before provider/tool work, so a forbidden admission
/// is observable without letting the replacement mutate the original files.
struct RecoveryAdmissionWitness(tokio::sync::mpsc::Sender<String>);
impl qq_core::RuntimeLoader for RecoveryAdmissionWitness {
    fn load(&self, request: qq_core::RuntimeLoadRequest) -> qq_core::RuntimeLoadFuture {
        self.0.try_send(request.workspace).unwrap();
        Box::pin(async {
            Err(qq_core::RuntimeLoadError {
                kind: qq_protocol::RunFailureKind::Configuration,
                message: "admission witnessed; no replacement execution".into(),
            })
        })
    }
}

struct WrongRecoveryOwner(qq_protocol::StoreId);
impl qq_core::command_owner::NativeCommandService for WrongRecoveryOwner {
    fn identity(&self) -> qq_core::command_owner::NativeCommandFuture<qq_protocol::StoreId> {
        let id = self.0;
        Box::pin(async move { Ok(id) })
    }
    fn acknowledge(
        &self,
        _: Vec<qq_protocol::ToolCallId>,
    ) -> qq_core::command_owner::NativeCommandFuture<()> {
        panic!("a different owner cannot acknowledge the original result")
    }
    fn submit(
        &self,
        _: qq_core::command_owner::NativeCommand,
    ) -> qq_core::command_owner::NativeCommandFuture<()> {
        panic!("recovery cannot replay a native invocation")
    }
    fn wait(
        &self,
        _: qq_protocol::ToolCallId,
        _: u64,
    ) -> qq_core::command_owner::NativeCommandFuture<qq_core::command_owner::CommandSnapshot> {
        panic!("a different owner cannot observe the original invocation")
    }
    fn cancel(
        &self,
        _: qq_protocol::ToolCallId,
    ) -> qq_core::command_owner::NativeCommandFuture<()> {
        panic!("a different owner cannot cancel the original invocation")
    }
}

async fn queue_recovery_witness(
    runtime: &qq_core::SessionRuntime,
    session_id: qq_protocol::SessionId,
) {
    runtime
        .command(
            qq_protocol::CommandId::generate().unwrap(),
            qq_protocol::SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text("continue the work")],
                limits: Default::default(),
                correlation: Default::default(),
                output: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn unavailable_native_owner_fences_restarts_until_authenticated_result_arrives() {
    use qq_core::{
        SessionRuntime, SessionRuntimeOptions, command_owner::NativeCommandService as _,
    };
    use qq_protocol::{CommandId, CommandOutcome, SessionCommand};
    use std::sync::Arc;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let workspace = home.join("workspace");
    let independent = home.join("independent");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&independent).unwrap();
    let marker = format!("qq-client-{}", qq_protocol::ToolCallId::generate().unwrap());
    let listener = tokio::net::UnixListener::bind(workspace.join(&marker)).unwrap();
    let arguments = json!({"program":std::env::current_exe().unwrap(),"args":["--exact","cli::blocked_command","--nocapture","--skip",marker]}).to_string();
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![tool_reply("exec", arguments)],
        Duration::from_secs(30),
    );
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    let mut original = configured_cli(&home, &workspace, &config)
        .args([
            "run",
            "--approval",
            "full",
            "--format",
            "jsonl",
            "make the artifact once",
        ])
        .spawn()
        .unwrap();
    let (mut barrier, _) = listener.accept().await.unwrap();
    let mut ready = [0; 5];
    barrier.read_exact(&mut ready).await.unwrap();
    assert_eq!(&ready, b"ready");
    let worker_process = Worker::child_of(original.id().unwrap());
    original.start_kill().unwrap();
    assert_eq!(
        original.wait_with_output().await.unwrap().status.signal(),
        Some(9)
    );
    let database = home.join("data/qq/sessions.sqlite3");
    let read_db = || {
        rusqlite::Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
    };
    let (session, call, owner): (String, String, String) = read_db().query_row(
        "SELECT r.session_id,t.id,n.owner_id FROM native_command_owners n JOIN tool_calls t ON t.id=n.tool_call_id JOIN runs r ON r.id=t.run_id",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).unwrap();
    let session = session.parse().unwrap();
    let call: qq_protocol::ToolCallId = call.parse().unwrap();
    let wrong = qq_protocol::StoreId::generate().unwrap();
    assert_ne!(wrong.to_string(), owner);
    let (loaded, mut admissions) = tokio::sync::mpsc::channel(16);
    let loader = Arc::new(RecoveryAdmissionWitness(loaded));

    // No service on first restart; another owner on the next two. Interrupting
    // the model must neither erase its command binding nor free its workspace.
    for replacement in [false, true, true] {
        let mut options = SessionRuntimeOptions::new(database.clone());
        if replacement {
            options.native_commands = Some(Arc::new(WrongRecoveryOwner(wrong)));
        }
        let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
        queue_recovery_witness(&runtime, session).await;
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: independent.to_str().unwrap().into(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
            panic!("workspace")
        };
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: qq_protocol::ModelSelection {
                        model: Some("fake/model".into()),
                        ..Default::default()
                    },
                    approval_mode: qq_protocol::ApprovalMode::Full,
                    profile: Default::default(),
                    reasoning_effort: None,
                    correlation: Default::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            panic!("session")
        };
        queue_recovery_witness(&runtime, session_id).await;
        assert_eq!(
            admissions.recv().await.unwrap(),
            independent.to_str().unwrap(),
            "unknown native work must fence its own workspace, while unrelated work proceeds"
        );
        runtime.close().await.unwrap();
        assert!(
            admissions.try_recv().is_err(),
            "the original workspace must remain fenced"
        );
        let unresolved: bool = read_db().query_row(
            "SELECT EXISTS(SELECT 1 FROM native_command_owners WHERE tool_call_id=?1 AND result_saved=0)",
            [call.to_string()], |row| row.get(0),
        ).unwrap();
        assert!(
            unresolved,
            "interruption and wrong-owner recovery cannot discharge native custody"
        );
        let interrupted: bool = read_db().query_row(
            "SELECT r.status='interrupted' FROM runs r JOIN tool_calls t ON t.run_id=r.id WHERE t.id=?1",
            [call.to_string()], |row| row.get(0),
        ).unwrap();
        assert!(
            interrupted,
            "the model interruption must remain distinct from command completion"
        );
        assert!(!workspace.join("artifact").exists());
    }

    let worker = Arc::new(
        super::client::CommandWorker::new(
            home.join("data/qq/command-owner-v3"),
            env!("CARGO_BIN_EXE_qq").into(),
            concat!(
                env!("CARGO_PKG_VERSION"),
                "+",
                env!("QQ_BUILD_REVISION"),
                ".",
                env!("QQ_BUILD_DATE")
            ),
        )
        .unwrap(),
    );
    assert_eq!(worker.identity().await.unwrap().to_string(), owner);
    let mut options = SessionRuntimeOptions::new(database.clone());
    options.native_commands = Some(worker.clone());
    let runtime = SessionRuntime::open(options, loader).await.unwrap();
    queue_recovery_witness(&runtime, session).await;
    barrier.write_all(b"e").await.unwrap();
    assert_eq!(
        admissions.recv().await.unwrap(),
        workspace.to_str().unwrap(),
        "only the original owner's terminal result may release replacement work"
    );
    runtime.close().await.unwrap();
    assert!(
        admissions.try_recv().is_err(),
        "the queued work must be admitted once"
    );
    let (state, result, receipt): (String, String, Option<String>) = read_db()
        .query_row(
            "SELECT state,result,execution_context_json FROM tool_calls WHERE id=?1",
            [call.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "completed");
    assert!(result.contains("native-result"), "{result}");
    assert!(
        receipt.is_some(),
        "late recovery must retain process evidence"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("executions")).unwrap(),
        "once\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifact")).unwrap(),
        "native-once"
    );
    worker_process.stop().await;
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(
        captured.len(),
        1,
        "recovery must never invoke the model or replay work"
    );
}
