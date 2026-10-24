//! The real CLI must use the same worker as explicit embedding clients.
use super::*;
use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};

#[path = "runtime/cargo.rs"]
mod cargo;
#[cfg(target_os = "macos")]
#[path = "runtime/node.rs"]
mod node;

#[tokio::test]
async fn default_auto_cannot_overwrite_host_files_through_a_project_symlink() {
    exercise_default_auto(None, false, "for-goal").await;
}

#[tokio::test]
async fn default_auto_completion_check_uses_the_detached_native_boundary() {
    for scope in ["once", "for-goal"] {
        exercise_default_auto(
            Some("printf check-write > alias; test -f result.txt"),
            false,
            scope,
        )
        .await;
    }
}

#[tokio::test]
async fn default_auto_goal_continuations_reuse_the_same_private_task_files() {
    exercise_default_auto(Some("test -f result.txt"), true, "for-goal").await;
}

async fn exercise_default_auto(check: Option<&str>, continuation: bool, check_scope: &str) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let secret = home.join("host-secret");
    std::fs::write(&secret, "host bytes").unwrap();
    std::os::unix::fs::symlink(&secret, workspace.join("alias")).unwrap();
    let data = data_directory(&home);
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
    let mut replies = vec![reply(vec![
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":""}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":r#"{"command":"printf overwritten > alias; rm alias; printf task-output > result.txt; printf cache-retained > .qq/task-home/cache-proof; printf task-command-ran"}"#}}),
        completed.clone(),
    ])];
    if continuation {
        replies.push(reply(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_wait","call_id":"call_wait","name":"update_goal","arguments":""}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_wait","call_id":"call_wait","name":"update_goal","arguments":json!({"wait":{"seconds":0,"reason":"continue the next bounded run"}}).to_string()}}),
            completed.clone(),
        ]));
        replies.push(reply(vec![json!({"type":"response.output_text.delta","delta":"artifact published; continue verification next run"}),completed.clone()]));
        replies.push(reply(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_resume","call_id":"call_resume","name":"shell","arguments":""}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_resume","call_id":"call_resume","name":"shell","arguments":r#"{"command":"cat .qq/task-home/cache-proof"}"#}}),
            completed.clone(),
        ]));
    }
    if check.is_some() {
        replies.push(reply(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"update_goal","arguments":""}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"update_goal","arguments":json!({"status":{"type":"achieved","evidence":"result.txt contains the requested output"}}).to_string()}}),
            completed.clone(),
        ]));
    }
    replies.push(reply(vec![
        json!({"type":"response.output_text.delta","delta":"finished"}),
        completed,
    ]));
    let provider = LoopbackServer::respond_sequence_with_timeout(replies, Duration::from_secs(60));
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    // Omit --approval: this must exercise the ordinary Auto default.
    let mut command = configured_cli(&home, &workspace, &config);
    command.args(["run", "--format", "jsonl"]);
    if let Some(check) = check {
        command.args([
            "--goal",
            "write result.txt and verify the finished artifact",
            "--check",
            check,
            "--goal-check-approval",
            check_scope,
            "--goal-max-runs",
            "4",
            "--goal-deadline",
            "1m",
        ]);
    } else {
        command.arg("write the requested file");
    }
    let output = tokio::time::timeout(Duration::from_secs(30), command.output()).await;
    worker.stop_spawned().await;
    let output = match output {
        Ok(output) => output.unwrap(),
        Err(error) => {
            let retained = root.keep();
            panic!(
                "Auto did not finish: {error}; fixture retained at {}",
                retained.display()
            );
        }
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "host bytes",
        "Auto must contain an allowed shell redirect to its task files"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt")).unwrap_or_else(|error| panic!(
            "{error}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
        "task-output"
    );
    assert!(
        std::fs::symlink_metadata(workspace.join("alias"))
            .unwrap()
            .is_symlink()
    );
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(
        captured.len(),
        if continuation {
            6
        } else if check.is_some() {
            3
        } else {
            2
        },
        "the model must receive the command outcome"
    );
    assert!(
        !workspace.join(".qq/task-home").exists(),
        "private caches must not publish into the project"
    );
    if continuation {
        let request = captured[4].json_body();
        let result = request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call_resume")
            .unwrap();
        assert!(
            result["output"]
                .as_str()
                .unwrap()
                .contains("cache-retained"),
            "{result}"
        );
        assert_eq!(
            std::fs::read_dir(data.join("task-workspaces"))
                .unwrap()
                .filter(|entry| entry.as_ref().is_ok_and(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.parse::<qq_protocol::RunId>().is_ok())
                }))
                .count(),
            2,
            "a goal shares one work tree across runs; verification gets a fresh copy"
        );
    }
    if check.is_some() {
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("\"type\":\"achieved\""),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    assert!(
        captured[1].body().contains("task-command-ran"),
        "{}",
        captured[1].body()
    );
}

#[tokio::test]
async fn real_cli_persists_native_ownership_and_releases_worker_results() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let data = data_directory(&home);
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
        configured_cli(&home, &workspace, &config)
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

fn data_directory(home: &Path) -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    let relative = "data/qq";
    #[cfg(target_os = "macos")]
    let relative = "Library/Application Support/dev.qq.qq";
    home.join(relative)
}
