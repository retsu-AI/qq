//! The real CLI must use the same worker as explicit embedding clients.
use super::*;
use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};

#[tokio::test]
async fn real_cli_persists_native_ownership_and_releases_worker_results() {
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
    #[cfg(target_os = "linux")]
    let data = root.path().join("data/qq");
    #[cfg(target_os = "macos")]
    let data = root.path().join("Library/Application Support/dev.qq.qq");
    let worker = client::CommandWorker::new(
        data.join("command-owner-v3"),
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
    worker.identity().await.unwrap();
    let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![
            reply(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":r#"{"command":"printf once >> artifact; printf native-result"}"#}}),
                completed.clone(),
            ]),
            reply(vec![
                json!({"type":"response.output_text.delta","delta":"finished"}),
                completed,
            ]),
        ],
        Duration::from_secs(60),
    );
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        configured_cli(root.path(), &workspace, &config)
            .args([
                "run",
                "--approval",
                "full",
                "--format",
                "jsonl",
                "execute the fixture",
            ])
            .output(),
    )
    .await;
    worker.stop_spawned().await;
    let output = output
        .expect("CLI output must close independently of worker lifetime")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifact")).unwrap(),
        "once"
    );
    let connection = rusqlite::Connection::open(data.join("sessions.sqlite3")).unwrap();
    let (id, saved): (String, String) = connection
        .query_row(
            "SELECT id,result FROM tool_calls WHERE state='completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("normal CLI execution must save its command result");
    assert!(saved.contains("native-result"));
    let journal =
        rusqlite::Connection::open(data.join("command-owner-v3/commands/commands.sqlite3"))
            .unwrap();
    let retired: bool = journal
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM retired_commands WHERE id=?1)",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        retired,
        "the durable session result must release its worker observation"
    );
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), 2);
    assert!(captured[1].body().contains("native-result"));
}

pub(super) fn reply(events: Vec<Value>) -> (u16, Option<&'static str>, Vec<Vec<u8>>) {
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    (200, Some("text/event-stream"), vec![body.into_bytes()])
}

pub(super) fn configured_cli(
    home: &Path,
    workspace: &Path,
    config: &str,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_qq"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("QQ_CONFIG_CONTENT", config)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, directory) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ] {
        command.env(name, home.join(directory));
    }
    command
}
