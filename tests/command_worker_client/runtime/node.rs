//! Actual installed Node/npm compatibility, independent of Rust's toolchain inputs.
use super::*;

#[tokio::test]
#[ignore = "requires installed Node/npm; unix-sdk-native workflow runs this exact test"]
async fn installed_node_builds_and_tests_in_default_auto() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let workspace = home.join("workspace");
    let host_workspace = home.join("host-workspace");
    let credential = home.join("outside-credential");
    std::fs::write(&credential, b"owned private credential canary").unwrap();
    let package = json!({"name":"qq-node-proof","version":"1.0.0","private":true,"scripts":{"build":"node build.js","test":"node test.js"}});
    for directory in [&workspace, &host_workspace] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("package.json"), package.to_string()).unwrap();
        std::fs::write(
            directory.join("build.js"),
            "const os=require('os'),fs=require('fs'),assert=require('assert');assert.ok(os.type().length>0);assert.ok(os.version().length>0);fs.writeFileSync('node-os-proof.json',JSON.stringify({type:os.type(),version:os.version()}));let privateCode='READ_ALLOWED';try{fs.readFileSync(__PRIVATE_CANARY__)}catch(error){privateCode=error.code}fs.writeFileSync('private-canary-proof.json',JSON.stringify({code:privateCode}));fs.writeFileSync('built.txt','built');\n"
                .replace("__PRIVATE_CANARY__", &serde_json::to_string(&credential).unwrap()),
        )
        .unwrap();
        std::fs::write(directory.join("test.js"), "const fs=require('fs');require('assert').strictEqual(fs.readFileSync('built.txt','utf8'),'built');fs.writeFileSync('tested.txt','tested');\n").unwrap();
    }
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    const COMMAND: &str = "command -v node; command -v npm; node --version && npm run build --offline && npm test --offline && cat tested.txt";
    let host = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("/bin/sh")
            .args(["-c", COMMAND])
            .current_dir(&host_workspace)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    println!(
        "Host control stdout: {}\nHost control stderr: {}",
        String::from_utf8_lossy(&host.stdout),
        String::from_utf8_lossy(&host.stderr)
    );
    assert!(host.status.success());
    let host_private: Value = serde_json::from_slice(
        &std::fs::read(host_workspace.join("private-canary-proof.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(host_private["code"], "READ_ALLOWED");
    let host_os: Value =
        serde_json::from_slice(&std::fs::read(host_workspace.join("node-os-proof.json")).unwrap())
            .unwrap();
    assert_eq!(
        std::fs::read_to_string(host_workspace.join("tested.txt")).unwrap(),
        "tested"
    );
    let worker = client::CommandWorker::new(
        data_directory(&home).join("command-owner-v3"),
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
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_node","call_id":"call_node","name":"shell","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_node","call_id":"call_node","name":"shell","arguments":json!({"command":COMMAND}).to_string()}}),
                completed.clone(),
            ]),
            reply(vec![
                json!({"type":"response.output_text.delta","delta":"Node build result recorded"}),
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
                "Build and test this Node project offline.",
            ])
            .output(),
    )
    .await;
    worker.stop_spawned().await;
    let output = output.unwrap().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "Auto stdout: {stdout}\nAuto stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success());
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !events
            .iter()
            .any(|event| event["envelope"]["event"]["type"] == "tool_approval_requested")
    );
    let calls: Vec<_> = events
        .iter()
        .filter(|event| event["envelope"]["event"]["type"] == "tool_call_finished")
        .collect();
    assert_eq!(calls.len(), 1);
    let call = &calls[0]["envelope"]["event"]["tool_call"];
    assert_eq!(call["is_error"], false, "{call}");
    assert!(
        call["result"].as_str().unwrap().contains("tested"),
        "{call}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("built.txt")).unwrap(),
        "built"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("tested.txt")).unwrap(),
        "tested"
    );
    assert!(!workspace.join(".qq/task-home").exists());
    let task_os: Value =
        serde_json::from_slice(&std::fs::read(workspace.join("node-os-proof.json")).unwrap())
            .unwrap();
    assert_eq!(task_os, host_os);
    let private_proof: Value = serde_json::from_slice(
        &std::fs::read(workspace.join("private-canary-proof.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(private_proof["code"], "EACCES");
    assert_eq!(
        std::fs::read(&credential).unwrap(),
        b"owned private credential canary"
    );
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), 2);
}
