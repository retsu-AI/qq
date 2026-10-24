//! Exercise the production Auto composition with real native command execution.
use super::*;
use futures_util::StreamExt as _;
use qq_auth::CredentialPaths;
use qq_core::command_owner::{CommandLauncher, CommandOwner};
use qq_protocol::{CommandId, CommandOutcome, SessionCommand, SessionEvent};
use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{fs, time::Duration};

#[tokio::test]
async fn default_auto_contains_shell_writes_and_publishes_detached_output() {
    let tool_versions = tokio::task::spawn_blocking(|| {
        ["git", "node", "cargo"].map(|program| {
            let output = std::process::Command::new(program)
                .arg("--version")
                .output()
                .unwrap_or_else(|error| panic!("host {program} must be installed: {error}"));
            assert!(output.status.success(), "host {program}: {output:?}");
            (program, String::from_utf8(output.stdout).unwrap())
        })
    })
    .await
    .unwrap();
    let fixture = super::tests::RuntimeFixture::new();
    let source = fixture.path("work");
    let secret = fixture.path("host-secret");
    fs::write(&secret, "host bytes").unwrap();
    fs::write(
        source.join("package.json"),
        r#"{"private":true,"scripts":{"build":"node build.js","test":"node test.js"}}"#,
    )
    .unwrap();
    fs::write(
        source.join("build.js"),
        "require('fs').writeFileSync('node-built.json', JSON.stringify({answer: 42}));",
    )
    .unwrap();
    fs::write(source.join("test.js"), "const fs=require('fs');require('assert').strictEqual(JSON.parse(fs.readFileSync('node-built.json')).answer,42);fs.writeFileSync('node-test-passed','yes');").unwrap();
    fs::create_dir(source.join("rust-proof")).unwrap();
    fs::create_dir(source.join("rust-proof/src")).unwrap();
    fs::write(
        source.join("rust-proof/Cargo.toml"),
        "[package]\nname='task-proof'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(source.join("rust-proof/src/main.rs"), "fn main(){std::fs::write(\"rust-built-output\",b\"42\").unwrap();} #[test] fn answer(){assert_eq!(6*7,42);}").unwrap();

    let mut script = format!(
        "echo task-output>\"result with spaces.txt\" & echo damage>\"{}\" & echo cache-retained>\".qq/task-home/cache-proof\" & echo task-command-ran",
        secret.display()
    );
    for name in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "TMP",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        script.push_str(&format!(" & echo private-{name}>\"%{name}%/proof-{name}\""));
    }
    for (program, _) in &tool_versions {
        script.push_str(&format!(" & {program} --version>\"{program}-version.txt\""));
    }
    script.push_str(
        " & node -e \"require('fs').writeFileSync('node-output.txt','node task output')\"",
    );
    script.push_str(" & node -e \"const fs=require('fs'),p=require('path');try{fs.writeFileSync(p.join(p.dirname(process.execPath),'node_modules/npm/package.json'),'damage');fs.writeFileSync('sdk-write-denied','WRITE_ALLOWED')}catch(e){fs.writeFileSync('sdk-write-denied',e.code)}\"");
    for (name, command) in [
        ("npm-build", "call npm run build --offline"),
        ("npm-test", "call npm test --offline"),
        (
            "cargo-build",
            "cargo build --offline --manifest-path rust-proof/Cargo.toml",
        ),
        (
            "cargo-test",
            "cargo test --offline --manifest-path rust-proof/Cargo.toml",
        ),
    ] {
        // Expand the result after the command, never while the outer compound
        // line is parsed. Keep the ordinary shell command and all assertions.
        script.push_str(&format!(" & cmd /D /V:ON /S /C \"{command} >{name}.stdout 2>{name}.stderr & >{name}.exit echo !errorlevel!\""));
    }
    script.push_str(" & rust-proof\\target\\debug\\task-proof.exe");

    let complete = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![
            response(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":json!({"command":script}).to_string()}}),
                complete.clone(),
            ]),
            response(vec![
                json!({"type":"response.output_text.delta","delta":"finished"}),
                complete,
            ]),
        ],
        Duration::from_secs(60),
    );
    fs::write(fixture.path("global/config.ron"), format!(
        r#"(version:1, model:"fake/model", providers:{{"fake":Custom(connection:(base_url:"{}/v1",api:OpenAiResponses,auth:NoAuth),models:{{"model":(name:"Fake")}})}})"#,
        provider.base_url
    )).unwrap();
    let image = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/command_owner.exe");
    let owner = Arc::new(
        CommandOwner::open(
            fixture.path("owner"),
            "windows-auto-test",
            CommandLauncher::new(image),
        )
        .await
        .unwrap(),
    );
    let factory = fixture.factory_with_credentials(CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(super::tests::MemoryKeyring::default()),
    ));
    // This is the same production constructor used by open_owned; only the
    // service transport is local to this fixture. The native owner, helper,
    // AppContainer token, jobs, persistence and publication are real.
    let handler = RuntimeHandler::open_with_commands(factory, None, Some(owner.clone()))
        .await
        .unwrap();
    let resolved = handler
        .sessions()
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: source.display().to_string(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        panic!("workspace not resolved")
    };
    assert_eq!(ApprovalMode::default(), ApprovalMode::Auto);
    let created = handler
        .sessions()
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("fake/model".to_owned()),
                    ..ModelSelection::default()
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Default::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session not created")
    };
    let mut events = handler
        .sessions()
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    handler
        .sessions()
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text(
                    "write the requested artifact".to_owned(),
                )],
                limits: Default::default(),
                correlation: Default::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(60), async {
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let done = matches!(event.event, SessionEvent::RunFinished { .. });
            observed.push(event);
            if done {
                return observed;
            }
        }
        panic!("event stream closed before terminal outcome");
    })
    .await;
    handler.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();
    let events = finished.expect("default Auto must finish the native editing task");
    for event in &events {
        if let SessionEvent::ToolCallFinished { tool_call } = &event.event {
            eprintln!(
                "Windows Auto native result: tool={} is_error={} result={}",
                tool_call.name,
                tool_call.is_error,
                tool_call
                    .result
                    .as_deref()
                    .unwrap_or("<absent>")
                    .chars()
                    .take(16_384)
                    .collect::<String>()
            );
        }
    }

    assert!(
        matches!(
            events.last().map(|event| &event.event),
            Some(SessionEvent::RunFinished {
                outcome: qq_protocol::RunOutcome::Completed,
                ..
            })
        ),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "Auto must not request per-command approval"
    );
    assert_eq!(fs::read_to_string(&secret).unwrap(), "host bytes");
    assert_eq!(
        fs::read_to_string(source.join("result with spaces.txt"))
            .unwrap()
            .trim_end(),
        "task-output"
    );
    assert!(!source.join(".qq/task-home").exists());
    for name in ["npm-build", "npm-test", "cargo-build", "cargo-test"] {
        eprintln!(
            "{name}: exit={:?}; stdout={:?}; stderr={:?}",
            fs::read_to_string(source.join(format!("{name}.exit"))),
            fs::read_to_string(source.join(format!("{name}.stdout"))),
            fs::read_to_string(source.join(format!("{name}.stderr")))
        );
    }
    let sdk_write_error = fs::read_to_string(source.join("sdk-write-denied")).unwrap();
    assert!(
        matches!(sdk_write_error.as_str(), "EACCES" | "EPERM"),
        "shared SDK must be readable but not writable: {sdk_write_error}"
    );
    assert_eq!(
        fs::read_to_string(source.join("node-test-passed")).unwrap(),
        "yes",
        "npm must build and test using private task configuration"
    );
    assert_eq!(
        fs::read_to_string(source.join("rust-built-output")).unwrap(),
        "42",
        "Cargo must build and execute its artifact"
    );
    assert!(
        fs::read_to_string(source.join("cargo-test.stdout"))
            .unwrap()
            .contains("test result: ok."),
        "Cargo must execute tests"
    );
    for (program, expected) in &tool_versions {
        assert_eq!(
            fs::read_to_string(source.join(format!("{program}-version.txt")))
                .unwrap()
                .trim(),
            expected.trim(),
            "Auto must run the host's installed {program} with a private task home",
        );
    }
    assert_eq!(
        fs::read_to_string(source.join("node-output.txt")).unwrap(),
        "node task output"
    );
    let connection = rusqlite::Connection::open(fixture.path("data/sessions.sqlite3")).unwrap();
    let record: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key LIKE 'qq_task_workspace:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["scope"]["network"], "isolated");
    assert_eq!(record["scope"]["resources"]["access"], "committed");
    assert_eq!(record["scope"]["resources"]["cpu_percent"], 90);
    let task = PathBuf::from(record["scope"]["workspace"]["path"].as_str().unwrap());
    assert!(task.starts_with(fixture.path("data/task-workspaces")));
    assert_eq!(
        fs::read_to_string(task.join(".qq/task-home/cache-proof"))
            .unwrap()
            .trim_end(),
        "cache-retained"
    );
    for (name, directory) in [
        ("HOME", ""),
        ("USERPROFILE", ""),
        ("TMPDIR", "tmp"),
        ("TEMP", "tmp"),
        ("TMP", "tmp"),
        ("APPDATA", "AppData/Roaming"),
        ("LOCALAPPDATA", "AppData/Local"),
    ] {
        assert_eq!(
            fs::read_to_string(
                task.join(".qq/task-home")
                    .join(directory)
                    .join(format!("proof-{name}"))
            )
            .unwrap()
            .trim_end(),
            format!("private-{name}"),
            "{name} must resolve to private task storage",
        );
    }
    let calls = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].body().contains("task-command-ran"),
        "{}",
        calls[1].body()
    );
}

fn response(events: Vec<Value>) -> (u16, Option<&'static str>, Vec<Vec<u8>>) {
    (
        200,
        Some("text/event-stream"),
        vec![
            events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>()
                .into_bytes(),
        ],
    )
}

#[test]
fn installed_toolchain_read_roots_refuse_junction_escapes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_owned();
    let toolchain = root.join("rustup").join("toolchains").join("selected");
    let bin = toolchain.join("bin");
    let lib = toolchain.join("lib");
    let outside = root.join("private-outside");
    for directory in [&bin, &lib, &outside] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(outside.join("credential-canary"), "private bytes").unwrap();
    assert_eq!(
        windows_toolchain_read_roots(&bin).unwrap(),
        [bin.canonicalize().unwrap(), lib.canonicalize().unwrap()]
    );
    for target in [&bin, &lib] {
        let saved = target.with_extension("saved");
        fs::rename(target, &saved).unwrap();
        let created = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(target)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");
        let error = windows_toolchain_read_roots(&bin).unwrap_err();
        fs::remove_dir(target).unwrap();
        fs::rename(&saved, target).unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
    assert_eq!(
        fs::read_to_string(outside.join("credential-canary")).unwrap(),
        "private bytes"
    );
}
