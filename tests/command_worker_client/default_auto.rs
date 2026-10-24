//! Real CLI defaults, isolated app state, detached native execution and publication.
use super::{NativeCommandService as _, client};
use futures_util::FutureExt as _;
use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{panic::AssertUnwindSafe, process::Stdio, time::Duration};

fn reply(events: Vec<Value>) -> (u16, Option<&'static str>, Vec<Vec<u8>>) {
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    (200, Some("text/event-stream"), vec![body.into_bytes()])
}

#[tokio::test]
async fn real_cli_explicit_goal_resume_keeps_identity_budget_and_artifact() {
    use qq_protocol::{GoalSnapshot, GoalStatus};

    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let source = home.join("ordinary workspace");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(home.join("config")).unwrap();
    let worker = client::CommandWorker::new(
        home.join("data/command-owner-v3"),
        env!("CARGO_BIN_EXE_qq").into(),
        concat!(
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("QQ_BUILD_REVISION"),
            ".",
            env!("QQ_BUILD_DATE")
        ),
    )
    .unwrap();
    let result = AssertUnwindSafe(async {
        worker.identity().await.unwrap();
        let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
        let tool = |id: &str, name: &str, arguments: Value| reply(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":id,"call_id":id,"name":name,"arguments":""}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":id,"call_id":id,"name":name,"arguments":arguments.to_string()}}),
            completed.clone(),
        ]);
        let provider = LoopbackServer::respond_sequence_with_timeout(vec![
            tool("checkpoint", "write_file", json!({"path":"checkpoint.txt","content":"phase-one"})),
            tool("blocked", "update_goal", json!({"status":{"type":"blocked","reason":"fixture interruption; resume explicitly"}})),
            tool("read-checkpoint", "read_file", json!({"path":"checkpoint.txt"})),
            tool("artifact", "write_file", json!({"path":"result.txt","content":"completed from phase-one"})),
            tool("achieved", "update_goal", json!({"status":{"type":"achieved","evidence":"result.txt completes the saved checkpoint"}})),
            reply(vec![json!({"type":"response.output_text.delta","delta":"completed from the saved checkpoint"}), completed.clone()]),
            reply(vec![json!({"type":"response.output_text.delta","delta":r#"{"achieved":true,"evidence":"the requested artifact was independently checked"}"#}), completed]),
        ], Duration::from_secs(150));
        let config = format!(r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#, provider.base_url);
        let command = || {
            let mut cli = tokio::process::Command::new(env!("CARGO_BIN_EXE_qq"));
            cli.env_clear();
            for name in ["PATH", "HOME", "LANG", "TERM", "SYSTEMROOT", "WINDIR", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(name) { cli.env(name, value); }
            }
            cli.env("QQ_HOME", &home).env("QQ_CONFIG_CONTENT", &config)
                .current_dir(&source).stdin(Stdio::null()).stdout(Stdio::piped())
                .stderr(Stdio::piped()).kill_on_drop(true);
            cli
        };
        let first = tokio::time::timeout(Duration::from_secs(150), command().args([
            "run", "--format", "jsonl", "--goal", "finish the saved checkpoint",
            "--goal-max-runs", "4", "--goal-deadline", "5m",
        ]).output()).await.unwrap().unwrap();
        let diagnostics = |output: &std::process::Output| format!("stdout={} stderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert_eq!(first.status.code(), Some(7), "{}", diagnostics(&first));
        let database = home.join("data/sessions.sqlite3");
        let paused: GoalSnapshot = {
            let store = rusqlite::Connection::open(&database).unwrap();
            let payload: String = store.query_row("SELECT payload_json FROM session_goals WHERE live=1", [], |row| row.get(0)).unwrap();
            serde_json::from_str(&payload).unwrap()
        };
        assert!(matches!(paused.status, GoalStatus::Blocked { .. }));
        assert_eq!(paused.counters.runs, 1);
        assert_eq!(paused.counters.usage.as_ref().unwrap().input_tokens, 6);
        let resumed = tokio::time::timeout(Duration::from_secs(150), command().args([
            "run", "--format", "jsonl", "--goal", "--session", &paused.session_id.to_string(),
        ]).output()).await.unwrap().unwrap();
        assert!(resumed.status.success(), "{}", diagnostics(&resumed));
        let store = rusqlite::Connection::open(&database).unwrap();
        let payload: String = store.query_row("SELECT payload_json FROM session_goals WHERE live=1", [], |row| row.get(0)).unwrap();
        let finished: GoalSnapshot = serde_json::from_str(&payload).unwrap();
        assert_eq!(finished.goal_id, paused.goal_id);
        assert_eq!(finished.session_id, paused.session_id);
        assert_eq!(finished.budget, paused.budget, "resume must not renew the deadline or allowance");
        assert_eq!(finished.status, GoalStatus::Achieved);
        assert_eq!(finished.counters.runs, 3, "one work successor and one audit");
        assert_eq!(finished.counters.audits, 1);
        assert_eq!(std::fs::read_to_string(source.join("checkpoint.txt")).unwrap(), "phase-one");
        assert_eq!(std::fs::read_to_string(source.join("result.txt")).unwrap(), "completed from phase-one");
        let events: Vec<qq_protocol::SessionEventEnvelope> = store.prepare("SELECT envelope_json FROM events ORDER BY sequence").unwrap()
            .query_map([], |row| row.get::<_, String>(0)).unwrap()
            .map(|encoded| serde_json::from_str(&encoded.unwrap()).unwrap()).collect();
        assert_eq!(events.iter().filter(|event| matches!(event.event, qq_protocol::SessionEvent::SessionCreated { .. })).count(), 1);
        assert!(!events.iter().any(|event| matches!(event.event, qq_protocol::SessionEvent::ToolApprovalRequested { .. } | qq_protocol::SessionEvent::ToolApprovalEscalated { .. })));
        let captured = tokio::task::spawn_blocking(move || provider.capture_all()).await.unwrap();
        assert_eq!(captured.len(), 7);
        let request = captured[3].json_body();
        let read = request["input"].as_array().unwrap().iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "read-checkpoint")
            .unwrap();
        assert!(read["output"].as_str().unwrap().contains("phase-one"), "{read}");
        let writes: Vec<_> = events.iter().filter_map(|event| match &event.event {
            qq_protocol::SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "write_file" => Some(tool_call),
            _ => None,
        }).collect();
        assert_eq!(writes.len(), 2, "one checkpoint write and one resumed artifact write");
        for call in writes {
            assert_eq!(call.state, qq_protocol::ToolCallState::Completed);
            assert!(!call.is_error);
        }
        let usage = finished.counters.usage.unwrap();
        assert_eq!(usage.input_tokens, 21);
        assert_eq!(usage.output_tokens, 14);
        assert!(usage.input_tokens > paused.counters.usage.as_ref().unwrap().input_tokens);
    }).catch_unwind().await;
    worker.stop_spawned().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn real_cli_default_auto_publishes_detached_native_shell_work_without_approvals() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let data = home.join("data");
    let source = home.join("ordinary workspace");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(home.join("config")).unwrap();
    assert!(!source.join(".git").exists());
    assert!(!data.join("sessions.sqlite3").exists());
    let worker_directory = data.join("command-owner-v3");
    let worker = client::CommandWorker::new(
        worker_directory.clone(),
        env!("CARGO_BIN_EXE_qq").into(),
        concat!(
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("QQ_BUILD_REVISION"),
            ".",
            env!("QQ_BUILD_DATE")
        ),
    )
    .unwrap();
    // The fixture owns the prestarted worker; the real CLI reconnects to it.
    // Cleanup also runs after timeout or an assertion panic.
    let result = AssertUnwindSafe(async {
        let worker_id = worker.identity().await.unwrap();
        #[cfg(windows)]
        let command = "echo task-output> result.txt & cd > executed-cwd.txt & echo native-cli-result";
        #[cfg(not(windows))]
        let command = "echo task-output> result.txt; pwd > executed-cwd.txt; echo native-cli-result";
        let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
        let provider = LoopbackServer::respond_sequence_with_timeout(
            vec![
                reply(vec![
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_shell","call_id":"shell","name":"shell","arguments":""}}),
                    json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_shell","call_id":"shell","name":"shell","arguments":json!({"command":command}).to_string()}}),
                    completed.clone(),
                ]),
                reply(vec![json!({"type":"response.output_text.delta","delta":"finished"}),completed]),
            ],
            Duration::from_secs(150),
        );
        let config = format!(
            r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
            provider.base_url,
        );
        let mut cli = tokio::process::Command::new(env!("CARGO_BIN_EXE_qq"));
        cli.env_clear();
        // Keep OS/tool discovery inputs, with no real provider credentials or
        // machine-user QQ configuration. QQ_HOME alone selects app state.
        for name in ["PATH", "HOME", "LANG", "TERM", "SYSTEMROOT", "WINDIR", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                cli.env(name, value);
            }
        }
        let output = tokio::time::timeout(
            Duration::from_secs(150),
            cli.env("QQ_HOME", &home)
                .env("QQ_CONFIG_CONTENT", config)
                .current_dir(&source)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                // No approval flag: this is the shipped Auto default.
                .args(["run", "--format", "jsonl", "write the requested result file"])
                .output(),
        )
        .await
        .expect("real Auto CLI must finish without an approval waiter")
        .unwrap();
        let diagnostics = format!("stdout={} stderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{diagnostics}");
        assert_eq!(std::fs::read_to_string(source.join("result.txt")).unwrap().trim(), "task-output");
        let requests = tokio::task::spawn_blocking(move || provider.capture_all()).await.unwrap();
        assert_eq!(requests.len(), 2, "no additional reviewer inference: {diagnostics}");
        assert!(requests[1].body().contains("native-cli-result"), "{}", requests[1].body());
        let sessions = rusqlite::Connection::open(data.join("sessions.sqlite3")).unwrap();
        let (id, saved, context): (String, String, String) = sessions.query_row(
            "SELECT id,result,execution_context_json FROM tool_calls WHERE name='shell' AND state='completed' AND is_error=0", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert!(saved.contains("native-cli-result"), "{saved}");
        let context: Value = serde_json::from_str(&context).unwrap();
        assert_eq!(context["version"], "2");
        assert_eq!(context["facts"]["context_id"], id);
        assert!(context["facts"]["pid"].as_u64().is_some_and(|pid| pid > 0));
        let mut completed_run = false;
        let mut auto_session = false;
        let mut events = sessions.prepare("SELECT envelope_json FROM events ORDER BY sequence").unwrap();
        for encoded in events.query_map([], |row| row.get::<_, String>(0)).unwrap() {
            let event: qq_protocol::SessionEventEnvelope = serde_json::from_str(&encoded.unwrap()).unwrap();
            match event.event {
                qq_protocol::SessionEvent::SessionCreated { session } => {
                    assert_eq!(session.approval_mode, qq_protocol::ApprovalMode::Auto);
                    auto_session = true;
                }
                qq_protocol::SessionEvent::RunFinished { outcome, .. } => {
                    assert_eq!(outcome, qq_protocol::RunOutcome::Completed);
                    completed_run = true;
                }
                qq_protocol::SessionEvent::ToolApprovalRequested { .. }
                | qq_protocol::SessionEvent::ToolApprovalEscalated { .. } => panic!("Auto must not request a human or reviewer: {diagnostics}"),
                _ => {}
            }
        }
        assert!(auto_session && completed_run, "{diagnostics}");
        let encoded: String = sessions.query_row("SELECT value FROM metadata WHERE key LIKE 'qq_task_workspace:%'", [], |row| row.get(0)).unwrap();
        let task: Value = serde_json::from_str(&encoded).unwrap();
        let scope: qq_core::command_owner::NativeCommandScope = serde_json::from_value(task["scope"].clone()).unwrap();
        assert_eq!(scope.read_access, qq_core::command_owner::NativeReadAccess::Host);
        assert_ne!(scope.workspace.path, source);
        assert!(scope.workspace.path.starts_with(data.join("task-workspaces")));
        let executed = std::path::PathBuf::from(std::fs::read_to_string(source.join("executed-cwd.txt")).unwrap().trim());
        assert_eq!(executed.canonicalize().unwrap(), scope.workspace.path);
        let journal = rusqlite::Connection::open(worker_directory.join("commands/commands.sqlite3")).unwrap();
        let stored_worker: String = journal.query_row("SELECT store_id FROM metadata WHERE singleton=1", [], |row| row.get(0)).unwrap();
        assert_eq!(stored_worker, worker_id.to_string());
        let retired: bool = journal.query_row("SELECT EXISTS(SELECT 1 FROM retired_commands WHERE id=?1)", [&id], |row| row.get(0)).unwrap();
        assert!(retired, "the actual CLI call must be observed and released by its owned worker");
    })
    .catch_unwind()
    .await;
    worker.stop_spawned().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
