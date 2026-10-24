//! Attribute installed Cargo startup using the production Auto worker.
use super::*;
use futures_util::StreamExt as _;
use qq_auth::CredentialPaths;
use qq_core::command_owner::{CommandLauncher, CommandOwner};
use qq_protocol::{CommandId, CommandOutcome, SessionCommand, SessionEvent};
use qq_provider::test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{fs, time::Duration};

#[tokio::test]
async fn installed_cargo_startup_through_real_auto_worker() {
    let fixture = super::tests::RuntimeFixture::new();
    let source = fixture.path("work");
    let secret = fixture.path("host-secret");
    fs::write(&secret, "host bytes").unwrap();
    let manifest =
        fs::read(std::env::var_os("QQ_CARGO_IMAGES").expect("host-selected compiler manifest"))
            .unwrap();
    assert!(manifest.len() <= 1024 * 1024);
    let paths: Value = serde_json::from_slice(&manifest).unwrap();
    eprintln!("EXACT selected compiler manifest: {paths}");
    fs::write(source.join("compiler-images.json"), &manifest).unwrap();
    let examples = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples");
    fs::copy(
        examples.join("windows_cargo_images.exe"),
        source.join("compiler-images.exe"),
    )
    .unwrap();
    fs::create_dir_all(source.join("rust-proof/src")).unwrap();
    fs::write(
        source.join("rust-proof/Cargo.toml"),
        "[package]\nname='task-proof'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(source.join("rust-proof/src/main.rs"), "fn main(){std::fs::write(\"rust-built-output\",b\"42\").unwrap();} #[test]fn answer(){assert_eq!(6*7,42);}").unwrap();
    let commands = [
        ("images", "compiler-images.exe compiler-images.json"),
        (
            "dll-init",
            "compiler-images.exe --load compiler-images.json",
        ),
        ("cargo-version", "cargo --version"),
        ("rustc-version", "rustc --version"),
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
    let mut batch = String::from(
        "@echo off\r\n>sdk-environment.txt set PATH\r\n>>sdk-environment.txt set COMSPEC\r\n>>sdk-environment.txt cd\r\n",
    );
    batch.push_str(&format!("type \"{}\" >private-read.stdout 2>private-read.stderr\r\n>private-read.exit echo %errorlevel%\r\n", secret.display()));
    for (name, command) in commands {
        batch.push_str(&format!(
            "{command} >{name}.stdout 2>{name}.stderr\r\n>{name}.exit echo %errorlevel%\r\n"
        ));
    }
    batch.push_str("exit /b 0\r\n");
    fs::write(source.join("cargo-startup.cmd"), &batch).unwrap();
    let control = fixture.path("host-cargo-control");
    fs::create_dir_all(control.join("rust-proof/src")).unwrap();
    for name in [
        "compiler-images.json",
        "compiler-images.exe",
        "cargo-startup.cmd",
        "rust-proof/Cargo.toml",
        "rust-proof/src/main.rs",
    ] {
        fs::copy(source.join(name), control.join(name)).unwrap();
    }
    let system = PathBuf::from(std::env::var_os("SYSTEMROOT").unwrap())
        .join("System32")
        .join("cmd.exe");
    let bin = PathBuf::from(paths["cargo"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let mut host = tokio::process::Command::new(&system);
    use std::os::windows::process::CommandExt as _;
    host.as_std_mut()
        .raw_arg("/D /S /C \"call cargo-startup.cmd\"");
    let host = tokio::time::timeout(
        Duration::from_secs(90),
        host.current_dir(&control)
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(bin)
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    log_results("HOST", &control, &commands);
    assert!(host.status.success(), "{host:?}");
    for (name, _) in commands {
        assert_eq!(
            fs::read_to_string(control.join(format!("{name}.exit")))
                .unwrap()
                .trim(),
            "0",
            "host {name}"
        );
    }
    assert_eq!(
        fs::read_to_string(control.join("private-read.stdout"))
            .unwrap()
            .trim(),
        "host bytes",
        "same private canary is readable in host control"
    );
    fs::write(source.join("auto-startup.cmd"),format!("@echo off\r\necho task-output>task-write.txt\r\necho damage>\"{}\"\r\ncall cargo-startup.cmd\r\nexit /b 0\r\n",secret.display())).unwrap();
    let script = "call auto-startup.cmd";
    let complete = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![
            response(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":json!({"command":script}).to_string()}}),
                complete.clone(),
            ]),
            response(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"exec","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"exec","arguments":json!({"program":"cargo","args":["--version"]}).to_string()}}),
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
    let mut observed = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let done = matches!(event.event, SessionEvent::RunFinished { .. });
            observed.push(event);
            if done {
                return;
            }
        }
        panic!("event stream closed before terminal outcome");
    })
    .await;
    let selected = tokio::task::spawn_blocking(selected_debugger_status)
        .await
        .unwrap();
    for row in selected {
        eprintln!("{row}");
    }
    handler.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();
    let events = observed;
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
                    .take(512 * 1024)
                    .collect::<String>()
            );
        }
    }

    finished.expect("default Auto must finish the native editing task");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "Auto must not request approval"
    );
    eprintln!(
        "DIAGNOSTIC ordinary AppContainer control: task WRITE_RESTRICTED omitted, not production containment"
    );
    log_results("CONTROL", &source, &commands);
    assert_eq!(fs::read_to_string(secret).unwrap(), "host bytes");
    assert_eq!(
        fs::read_to_string(source.join("task-write.txt"))
            .unwrap()
            .trim(),
        "task-output"
    );
    log_results("AUTO", &source, &commands);
    let objects = fs::read_to_string(source.join("images.stdout")).unwrap();
    assert!(objects.contains("self-event unnamed create succeeded; signal=Ok(())"));
    assert!(
        objects.lines().any(
            |line| line.starts_with("self-event name=qq-crypto-self-event-")
                && line.ends_with("reopen: Ok(())")
        ),
        "own package named event must reopen: {objects}"
    );
    assert!(
        objects.lines().any(|line| line
            .starts_with("self-event name=Local\\qq-crypto-self-event-")
            && line.ends_with("reopen: Ok(())")),
        "Local-prefixed event must reopen with the scoped namespace grant: {objects}"
    );
    eprintln!(
        "AUTO private-read exit={:?}",
        fs::read_to_string(source.join("private-read.exit"))
    );
    assert_ne!(
        fs::read_to_string(source.join("private-read.exit"))
            .unwrap()
            .trim(),
        "0"
    );
    assert!(
        fs::read(source.join("private-read.stdout"))
            .unwrap()
            .is_empty()
    );
    let record: String = rusqlite::Connection::open(fixture.path("data/sessions.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT value FROM metadata WHERE key LIKE 'qq_task_workspace:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    eprintln!("EXACT task authority: {record}");
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["scope"]["network"], "isolated");
    assert_eq!(record["scope"]["resources"]["access"], "committed");
    let direct_exec = events
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "exec" => {
                Some(tool_call)
            }
            _ => None,
        })
        .expect("real Auto must execute direct Cargo through the durable exec tool");
    assert!(
        !direct_exec.is_error,
        "direct Cargo exec must succeed: {:?}",
        direct_exec.result
    );
    for (name, _) in commands {
        assert_eq!(
            fs::read_to_string(source.join(format!("{name}.exit")))
                .unwrap()
                .trim(),
            "0",
            "ordinary Auto {name} must succeed"
        );
    }
    assert_eq!(
        fs::read_to_string(source.join("rust-built-output")).unwrap(),
        "42"
    );
    assert!(
        fs::read_to_string(source.join("cargo-test.stdout"))
            .unwrap()
            .contains("test result: ok.")
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

fn log_results(label: &str, directory: &std::path::Path, commands: &[(&str, &str)]) {
    let read = |name: &str| {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        fs::File::open(directory.join(name))?
            .take(512 * 1024)
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

fn selected_debugger_status() -> Vec<String> {
    use std::io::Read as _;
    let mut rows = Vec::new();
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return rows;
    };
    for entry in entries.take(4096).flatten().filter(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("qq-windows-helper-")
    }) {
        let Ok(files) = fs::read_dir(entry.path()) else {
            continue;
        };
        for file in files.take(64).flatten().filter(|file| {
            file.file_name().to_string_lossy().starts_with("cdb-")
                && file.file_name().to_string_lossy().ends_with("-state.txt")
        }) {
            if rows.len() >= 16 {
                break;
            }
            let Ok(file) = fs::File::open(file.path()) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file.take(131073).read_to_end(&mut bytes).is_ok() && bytes.len() <= 131072 {
                rows.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    rows
}
