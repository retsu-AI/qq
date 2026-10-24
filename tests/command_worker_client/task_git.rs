//! Real Auto edits and private Git commits share the independent worker.
use super::{
    client,
    runtime::{configured_cli, reply},
};
use qq_core::command_owner::NativeCommandService as _;
use qq_provider::test_support::LoopbackServer;
use serde_json::json;
use std::{path::Path, time::Duration};

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "init.templateDir=",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn default_auto_edits_and_commits_without_host_git_mutation_or_approvals() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q", "--template="]);
    std::fs::write(workspace.join("artifact"), "original").unwrap();
    std::fs::write(workspace.join("unrelated"), "committed").unwrap();
    git(&workspace, &["add", "artifact", "unrelated"]);
    git(&workspace, &["commit", "-qm", "Initial"]);
    let base = git(&workspace, &["rev-parse", "HEAD"]);
    // Preexisting user edits must publish unchanged, without becoming task changes.
    std::fs::write(workspace.join("unrelated"), "user dirty bytes").unwrap();
    let worker = client::CommandWorker::new(
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
    .unwrap();
    worker.identity().await.unwrap();
    let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
    let call = |id: &str, name: &str, arguments: serde_json::Value| {
        reply(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":format!("fc_{id}"),"call_id":id,"name":name,"arguments":""}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":format!("fc_{id}"),"call_id":id,"name":name,"arguments":arguments.to_string()}}),
            completed.clone(),
        ])
    };
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![
            call(
                "edit",
                "shell",
                json!({"command":"printf task-output > artifact; test $(cat artifact) = task-output"}),
            ),
            call(
                "commit",
                "task_commit",
                json!({"message":"Implement requested artifact"}),
            ),
            reply(vec![
                json!({"type":"response.output_text.delta","delta":"finished"}),
                completed,
            ]),
        ],
        Duration::from_secs(90),
    );
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    let output = tokio::time::timeout(
        Duration::from_secs(90),
        configured_cli(&home, &workspace, &config)
            .args([
                "run",
                "--format",
                "jsonl",
                "implement and commit the requested artifact",
            ])
            .output(),
    )
    .await;
    worker.stop_spawned().await;
    let output = match output {
        Ok(output) => output.unwrap(),
        Err(error) => panic!(
            "Auto task timed out: {error}; fixture {}",
            root.keep().display()
        ),
    };
    let diagnostics = format!(
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{diagnostics}");
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), 3, "{diagnostics}");
    assert!(
        captured[0].body().contains("task_commit"),
        "Auto must advertise the live broker"
    );
    let request = captured[2].json_body();
    let result = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "commit")
        .unwrap();
    let result = result["output"].as_str().unwrap();
    let receipt = result
        .lines()
        .find_map(|line| {
            serde_json::from_str::<qq_core::task_git_job::TaskGitJobReceipt>(line).ok()
        })
        .unwrap_or_else(|| panic!("missing typed commit receipt: {result}; {diagnostics}"));
    let qq_core::task_git_job::TaskGitJobResult::Committed { commit } = receipt.result else {
        panic!("{result}; {diagnostics}");
    };
    let private_git = home
        .join("data/qq/task-workspaces")
        .join(format!("task-git-{}", receipt.task));
    assert_eq!(
        git(&private_git, &["rev-parse", &commit.branch]),
        commit.oid
    );
    assert_eq!(
        git(&private_git, &["show", &format!("{}:artifact", commit.oid)]),
        "task-output"
    );
    assert_eq!(
        git(
            &private_git,
            &["show", &format!("{}:unrelated", commit.oid)]
        ),
        "committed",
        "unchanged user edits must not enter the private task commit"
    );
    assert_eq!(
        git(&private_git, &["rev-parse", &format!("{}^", commit.oid)]),
        base
    );
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        git(
            &workspace,
            &["for-each-ref", "--format=%(refname)", "refs/heads/qq/task"]
        ),
        ""
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifact")).unwrap(),
        "task-output"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("unrelated")).unwrap(),
        "user dirty bytes"
    );
    let sessions = rusqlite::Connection::open(home.join("data/qq/sessions.sqlite3")).unwrap();
    let (id, saved, context): (String, String, Option<String>) = sessions.query_row(
        "SELECT id,result,execution_context_json FROM tool_calls WHERE name='task_commit' AND state='completed'", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
    assert!(
        context
            .as_ref()
            .is_some_and(|context| context.contains("task_commit")),
        "commit must carry the worker-validated logical call proof: {context:?}"
    );
    assert!(saved.contains("committed"), "{saved}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("approval_requested"),
        "{diagnostics}"
    );
    let journal =
        rusqlite::Connection::open(home.join("data/qq/command-owner-v3/commands/commands.sqlite3"))
            .unwrap();
    assert!(
        journal
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM retired_commands WHERE id=?1)",
                [&id],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
    );
}
