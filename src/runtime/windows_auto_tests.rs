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
    let fixture = super::tests::RuntimeFixture::new();
    let metadata_control = fixture.path("data");
    let (tool_versions, rust_bin) = tokio::task::spawn_blocking(move || {
        let rust_bin = windows_sdk::discover(&metadata_control, Vec::new())
            .unwrap()
            .into_iter()
            .next()
            .expect("a conventionally installed server-default Rust toolchain");
        eprintln!(
            "Exact server-default Rust compiler bin: {}",
            rust_bin.display()
        );
        let versions = ["git", "node", "cargo"].map(|program| {
            let image = if program == "cargo" {
                rust_bin.join("cargo.exe")
            } else {
                PathBuf::from(program)
            };
            let output = std::process::Command::new(image)
                .arg("--version")
                .output()
                .unwrap_or_else(|error| panic!("host {program} must be installed: {error}"));
            assert!(output.status.success(), "host {program}: {output:?}");
            (program, String::from_utf8(output.stdout).unwrap())
        });
        (versions, rust_bin)
    })
    .await
    .unwrap();
    let source = fixture.path("work");
    let secret = fixture.path("host-secret");
    fs::write(&secret, "host bytes").unwrap();
    fs::write(
        source.join("package.json"),
        r#"{"private":true,"scripts":{"build":"node build.js","test":"node test.js","options":"node options.js"}}"#,
    )
    .unwrap();
    fs::write(
        source.join("build.js"),
        "const assert=require('assert');assert.deepStrictEqual(process.execArgv,[]);const runtime={node_options:process.env.NODE_OPTIONS??null,execArgv:process.execArgv};console.log(JSON.stringify(runtime));require('fs').writeFileSync('node-child-runtime.json',JSON.stringify(runtime));require('fs').writeFileSync('node-built.json', JSON.stringify({answer:require('./sibling.js')}));",
    )
    .unwrap();
    fs::write(source.join("sibling.js"), "module.exports=42;").unwrap();
    fs::write(source.join("test.js"), "const fs=require('fs');require('assert').strictEqual(JSON.parse(fs.readFileSync('node-built.json')).answer,42);fs.writeFileSync('node-test-passed','yes');").unwrap();
    fs::write(
        source.join("loader & scoped!.js"),
        "global.qqOptionsLoaded=true;\n",
    )
    .unwrap();
    fs::write(
        source.join("options.js"),
        r#"const assert=require('assert');assert.strictEqual(global.qqOptionsLoaded,true);assert.ok(process.env.NODE_OPTIONS.includes('--max-old-space-size=128'));require('fs').writeFileSync('node-options-preserved.json',JSON.stringify({options:process.env.NODE_OPTIONS,loaded:global.qqOptionsLoaded}));"#,
    ).unwrap();
    fs::write(
        source.join("npm-options.cmd"),
        "@echo off\r\nsetlocal DisableDelayedExpansion\r\nset NODE_OPTIONS=--require=\"./loader & scoped!.js\" --max-old-space-size=128\r\ncall npm run options --offline\r\nif errorlevel 1 exit /b %errorlevel%\r\nset NODE_OPTIONS>node-options-caller.txt\r\nexit /b %errorlevel%\r\n",
    ).unwrap();
    fs::create_dir(source.join("rust-proof")).unwrap();
    fs::create_dir(source.join("rust-proof/src")).unwrap();
    fs::write(
        source.join("rust-proof/Cargo.toml"),
        "[package]\nname='task-proof'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(source.join("rust-proof/src/main.rs"), "fn main(){std::fs::write(\"rust-built-output\",b\"42\").unwrap();} #[test] fn answer(){assert_eq!(6*7,42);}").unwrap();

    let mut auto_commands = vec![
        "echo task-output>\"result with spaces.txt\"".to_owned(),
        format!("echo damage>\"{}\"", secret.display()),
        "echo cache-retained>\".qq/task-home/cache-proof\"".to_owned(),
        "echo task-command-ran".to_owned(),
    ];
    for name in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "TMP",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        auto_commands.push(format!("echo private-{name}>\"%{name}%/proof-{name}\""));
    }
    for (program, _) in &tool_versions {
        auto_commands.push(format!(
            "call {program} --version>\"{program}-version.txt\""
        ));
    }
    auto_commands.push(
        "node -e \"require('fs').writeFileSync('node-output.txt','node task output')\"".to_owned(),
    );
    auto_commands.push("node -e \"const fs=require('fs'),p=require('path');try{fs.writeFileSync(p.join(p.dirname(process.execPath),'node_modules/npm/package.json'),'damage');fs.writeFileSync('sdk-write-denied','WRITE_ALLOWED')}catch(e){fs.writeFileSync('sdk-write-denied',e.code)}\"".to_owned());
    let metadata_image = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/windows_tool_access.exe");
    fs::copy(metadata_image, source.join("runtime-metadata.exe")).unwrap();
    let commands = [
        (
            "runtime-metadata",
            "runtime-metadata.exe --payload-metadata",
        ),
        ("npm-build", "call npm run build --offline"),
        ("npm-test", "call npm test --offline"),
        ("npm-custom-options", "call npm-options.cmd"),
        (
            "node-caller-env",
            r#"node -e "require('assert').strictEqual(process.env.NODE_OPTIONS,undefined);require('fs').writeFileSync('node-caller-env-clean','yes')""#,
        ),
        (
            "cargo-build",
            "cargo build --offline --manifest-path rust-proof/Cargo.toml",
        ),
        (
            "cargo-test",
            "cargo test --offline --manifest-path rust-proof/Cargo.toml",
        ),
        ("rust-artifact", r"rust-proof\target\debug\task-proof.exe"),
    ];
    // Each result is expanded on the next batch line. This avoids nested CMD
    // quoting and compound-line expansion while retaining ordinary tool calls.
    let mut batch = String::from(
        "@echo off\r\n>sdk-environment.txt set PATH\r\n>>sdk-environment.txt set COMSPEC\r\n>>sdk-environment.txt set SYSTEMROOT\r\n>>sdk-environment.txt cd\r\n",
    );
    for program in ["cmd", "node", "npm", "cargo"] {
        batch.push_str(&format!(
            ">>sdk-environment.txt echo resolved-{program}\r\nwhere.exe {program} >>sdk-environment.txt 2>&1\r\n"
        ));
    }
    for (name, command) in commands {
        batch.push_str(&format!(
            "{command} >{name}.stdout 2>{name}.stderr\r\n>{name}.exit echo %errorlevel%\r\n"
        ));
    }
    batch.push_str("exit /b 0\r\n");
    fs::write(source.join("sdk-workflow.cmd"), batch).unwrap();
    auto_commands.push("call sdk-workflow.cmd".to_owned());
    fs::write(
        source.join("auto-workflow.cmd"),
        format!("@echo off\r\n{}\r\n", auto_commands.join("\r\n")),
    )
    .unwrap();
    let script = "call auto-workflow.cmd";

    // Prove the same owned batch and installed tools work without confinement;
    // host setup failures must not be confused with sandbox admission failures.
    let control = fixture.path("host-sdk-control");
    fs::create_dir_all(control.join("rust-proof/src")).unwrap();
    for name in [
        "package.json",
        "build.js",
        "sibling.js",
        "test.js",
        "loader & scoped!.js",
        "options.js",
        "npm-options.cmd",
        "sdk-workflow.cmd",
        "runtime-metadata.exe",
        "rust-proof/Cargo.toml",
        "rust-proof/src/main.rs",
    ] {
        fs::copy(source.join(name), control.join(name)).unwrap();
    }
    let system = std::env::var_os("SYSTEMROOT").expect("Windows system root");
    let mut command = tokio::process::Command::new(
        std::path::Path::new(&system)
            .join("System32")
            .join("cmd.exe"),
    );
    {
        use std::os::windows::process::CommandExt as _;
        command
            .as_std_mut()
            .raw_arg("/D /S /C \"call sdk-workflow.cmd\"");
    }
    let host_bin = dunce::simplified(&rust_bin).to_owned();
    assert_eq!(host_bin.canonicalize().unwrap(), rust_bin);
    command.env(
        "PATH",
        std::env::join_paths(
            std::iter::once(host_bin)
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap(),
    );
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        command.current_dir(&control).kill_on_drop(true).output(),
    )
    .await
    .expect("owned host SDK control must finish")
    .unwrap();
    log_sdk_results("host", &control, &commands);
    assert!(output.status.success(), "host SDK batch failed: {output:?}");
    for (name, _) in commands {
        assert_eq!(
            fs::read_to_string(control.join(format!("{name}.exit")))
                .unwrap()
                .trim(),
            "0",
            "host {name} must succeed before Auto qualification"
        );
    }
    assert_eq!(
        fs::read_to_string(control.join("node-test-passed")).unwrap(),
        "yes"
    );
    assert_eq!(
        fs::read_to_string(control.join("rust-built-output")).unwrap(),
        "42"
    );

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
    log_sdk_results("Auto", &source, &commands);
    let child_runtime: Value =
        serde_json::from_slice(&fs::read(source.join("node-child-runtime.json")).unwrap()).unwrap();
    assert_eq!(
        child_runtime["node_options"],
        "--preserve-symlinks --preserve-symlinks-main"
    );
    assert_eq!(child_runtime["execArgv"], json!([]));
    let custom_options: Value =
        serde_json::from_slice(&fs::read(source.join("node-options-preserved.json")).unwrap())
            .unwrap();
    assert_eq!(custom_options["loaded"], true);
    assert_eq!(
        custom_options["options"],
        "--require=\"./loader & scoped!.js\" --max-old-space-size=128 --preserve-symlinks --preserve-symlinks-main"
    );
    assert_eq!(
        fs::read_to_string(source.join("node-options-caller.txt"))
            .unwrap()
            .trim_end(),
        "NODE_OPTIONS=--require=\"./loader & scoped!.js\" --max-old-space-size=128"
    );
    assert_eq!(
        fs::read_to_string(source.join("node-caller-env-clean")).unwrap(),
        "yes"
    );
    let sdk_write_error = fs::read_to_string(source.join("sdk-write-denied")).unwrap();
    assert!(
        matches!(sdk_write_error.as_str(), "EACCES" | "EPERM"),
        "shared SDK must be readable but not writable: {sdk_write_error}"
    );
    for (name, _) in commands {
        assert_eq!(
            fs::read_to_string(source.join(format!("{name}.exit")))
                .unwrap()
                .trim(),
            "0",
            "plain Auto {name} must succeed; diagnostic flags cannot satisfy this assertion"
        );
    }
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

fn log_sdk_results(label: &str, directory: &std::path::Path, commands: &[(&str, &str)]) {
    let read = |name: &str| {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        fs::File::open(directory.join(name))?
            .take(16_384)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
    };
    eprintln!("{label} SDK environment: {:?}", read("sdk-environment.txt"));
    for (name, _) in commands {
        eprintln!(
            "{label} {name}: exit={:?}; stdout={:?}; stderr={:?}",
            read(&format!("{name}.exit")),
            read(&format!("{name}.stdout")),
            read(&format!("{name}.stderr"))
        );
    }
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
