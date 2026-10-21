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
        exercise_goal(max_runs).await;
    }
}

async fn exercise_goal(max_runs: u32) {
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
    let listener = tokio::net::UnixListener::bind(workspace.join(&marker)).unwrap();
    let binary = std::env::current_exe().unwrap();
    let arguments = json!({"program":binary,"args":["--exact","cli::blocked_command","--nocapture","--skip",marker]}).to_string();
    let mut replies = vec![tool_reply("exec", arguments)];
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
    let checker = format!(
        "'{}' --exact goal_recovery::verify_goal_artifact --nocapture --skip qq-goal-verifier",
        binary.to_str().unwrap().replace('\'', "'\\''")
    );
    let mut original = configured_cli(root.path(), &workspace, &config)
        .args([
            "run",
            "--approval",
            "full",
            "--format",
            "jsonl",
            "--goal",
            "produce the fixture artifact exactly once and verify it",
            "--check",
            &checker,
            "--goal-check-approval",
            "for-goal",
            "--goal-max-runs",
            &max_runs.to_string(),
            "--goal-deadline",
            "5m",
        ])
        .spawn()
        .unwrap();
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut ready = [0; 5];
    stream.read_exact(&mut ready).await.unwrap();
    assert_eq!(&ready, b"ready");
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
    stream.write_all(b"e").await.unwrap();
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
    if max_runs > 1 {
        assert_eq!(terminal.status, GoalStatus::Achieved);
        assert_eq!(
            std::fs::read_to_string(workspace.join("verified")).unwrap(),
            "independent-check-passed"
        );
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
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifact")).unwrap(),
        "native-once"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("executions")).unwrap(),
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
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), if max_runs > 1 { 3 } else { 1 });
    if max_runs > 1 {
        assert!(
            captured[1].body().contains("native-result"),
            "the automatic successor must receive the original command result"
        );
    }
}
