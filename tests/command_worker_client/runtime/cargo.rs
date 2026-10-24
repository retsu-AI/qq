//! Explicit native qualification: the workflow supplies an already-built public cache.
use super::*;

#[tokio::test]
#[ignore = "requires an installed Rust toolchain and prebuilt public itoa cache; unix-sdk-native workflow runs this exact test"]
async fn cached_public_dependency_builds_in_default_auto() {
    let original_home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap();
    let cargo = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| original_home.join(".cargo"));
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| original_home.join(".rustup"));
    const REGISTRY: &str = "index.crates.io-1949cf8c6b5b557f";
    assert!(
        cargo
            .join(format!("registry/cache/{REGISTRY}/itoa-1.0.18.crate"))
            .is_file(),
        "build QQ dependencies before this explicit qualification"
    );
    assert!(
        cargo
            .join(format!("registry/index/{REGISTRY}/.cache/it/oa/itoa"))
            .is_file()
    );
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
    std::fs::write(workspace.join("Cargo.toml"), "[package]\nname=\"qq-sdk-proof\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\nitoa=\"=1.0.18\"\n[workspace]\n").unwrap();
    std::fs::write(workspace.join("Cargo.lock"), "version = 4\n\n[[package]]\nname = \"qq-sdk-proof\"\nversion = \"0.1.0\"\ndependencies = [\"itoa\"]\n\n[[package]]\nname = \"itoa\"\nversion = \"1.0.18\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682\"\n").unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("src/lib.rs"),
        "#[test] fn formats() { assert_eq!(itoa::Buffer::new().format(42), \"42\"); }\n",
    )
    .unwrap();
    // Same project/dependency must work on the host before attributing a failure to Auto.
    let host = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("cargo")
            .args(["test", "--offline", "--locked"])
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", home.join("host-target"))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        host.status.success(),
        "host control: {}{}",
        String::from_utf8_lossy(&host.stdout),
        String::from_utf8_lossy(&host.stderr)
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
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_cargo","call_id":"call_cargo","name":"shell","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_cargo","call_id":"call_cargo","name":"shell","arguments":r#"{"command":"cargo test --offline --locked"}"#}}),
                completed.clone(),
            ]),
            reply(vec![
                json!({"type":"response.output_text.delta","delta":"build result recorded"}),
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
            .env("CARGO_HOME", &cargo)
            .env("RUSTUP_HOME", &rustup)
            .args([
                "run",
                "--format",
                "jsonl",
                "Build and test this Rust project offline.",
            ])
            .output(),
    )
    .await;
    worker.stop_spawned().await;
    let output = output.unwrap().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "CLI stdout: {stdout}\nCLI stderr: {}",
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
    let finished: Vec<_> = events
        .iter()
        .filter(|event| event["envelope"]["event"]["type"] == "tool_call_finished")
        .collect();
    assert_eq!(finished.len(), 1);
    let call = &finished[0]["envelope"]["event"]["tool_call"];
    assert_eq!(call["is_error"], false, "{call}");
    assert!(
        call["result"]
            .as_str()
            .unwrap()
            .contains("1 passed; 0 failed"),
        "{call}"
    );
    let cache = data_directory(&home)
        .join("task-workspaces")
        .join(call["run_id"].as_str().unwrap())
        .join(".qq/task-home/.cargo");
    assert!(
        cache
            .join(format!("registry/cache/{REGISTRY}/itoa-1.0.18.crate"))
            .is_file()
    );
    for forbidden in [
        "credentials",
        "credentials.toml",
        "config",
        "config.toml",
        "git",
    ] {
        assert!(
            !cache.join(forbidden).exists(),
            "host Cargo authority copied: {forbidden}"
        );
    }
    assert!(!workspace.join(".qq/task-home").exists());
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), 2);
}
