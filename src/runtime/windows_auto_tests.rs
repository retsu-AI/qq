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
            let system = std::env::var_os("SYSTEMROOT").expect("Windows system root");
            let resolved =
                std::process::Command::new(PathBuf::from(system).join("System32/where.exe"))
                    .arg(program)
                    .output()
                    .unwrap();
            assert!(
                resolved.status.success(),
                "host executable lookup for {program}: {resolved:?}"
            );
            (
                program,
                String::from_utf8(output.stdout).unwrap(),
                String::from_utf8(resolved.stdout).unwrap(),
            )
        })
    })
    .await
    .unwrap();
    let fixture = super::tests::RuntimeFixture::new();
    let source = fixture.path("work");
    let secret = fixture.path("host-secret");
    fs::write(&secret, "host bytes").unwrap();
    let probe_image = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/windows_tool_inputs.exe");
    fs::copy(probe_image, source.join("tool-input-probe.exe")).unwrap();
    let mut targets = Vec::new();
    for (program, _, resolved) in &tool_versions {
        if *program == "git" {
            continue;
        }
        for original in resolved
            .lines()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            use std::os::windows::fs::MetadataExt as _;
            let canonical = fs::canonicalize(original).unwrap();
            let metadata = fs::metadata(original).unwrap();
            eprintln!(
                "Windows Auto host tool: {}",
                json!({
                    "program": program, "original": original, "canonical": canonical,
                    "file": metadata.is_file(), "length": metadata.len(), "attributes": metadata.file_attributes(),
                })
            );
            for path in [PathBuf::from(original), canonical] {
                if !targets
                    .iter()
                    .any(|entry: &Value| entry["path"] == json!(path))
                {
                    targets.push(json!({"program": program, "path": path}));
                }
            }
        }
    }
    fs::write(
        source.join("tool-input-manifest.json"),
        serde_json::to_vec(&json!({"targets": targets, "secret": secret})).unwrap(),
    )
    .unwrap();
    let mut script = format!(
        ".\\tool-input-probe.exe & echo task-output>\"result with spaces.txt\" & echo damage>\"{}\" & echo cache-retained>\".qq/task-home/cache-proof\" & echo task-command-ran",
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
    for (program, _, _) in &tool_versions {
        fs::write(
            source.join(format!("version-probe-{program}.cmd")),
            format!(
                "@echo off\r\nwhere.exe {program} >{program}-resolved.txt 2>{program}-resolve-stderr.txt\r\n>{program}-resolve-exit.txt echo %ERRORLEVEL%\r\n{program} --version >{program}-version.txt 2>{program}-stderr.txt\r\n>{program}-exit.txt echo %ERRORLEVEL%\r\nexit /b 0\r\n"
            ),
        )
        .unwrap();
        script.push_str(&format!(" & call version-probe-{program}.cmd"));
    }
    script.push_str(
        " & node -e \"require('fs').writeFileSync('node-output.txt','node task output')\"",
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
    // LPAC token, jobs, persistence and publication are real.
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
    for event in &events {
        if let SessionEvent::ToolCallFinished { tool_call } = &event.event {
            let result = tool_call.result.as_deref().unwrap_or("<no result>");
            let bounded = result.chars().take(16 * 1024).collect::<String>();
            eprintln!(
                "Windows Auto native result: {}",
                json!({
                    "tool": tool_call.name,
                    "is_error": tool_call.is_error,
                    "result": bounded,
                    "truncated": result.chars().count() > 16 * 1024,
                })
            );
        }
    }
    assert_eq!(fs::read_to_string(&secret).unwrap(), "host bytes");
    assert_eq!(
        fs::read_to_string(source.join("result with spaces.txt"))
            .unwrap()
            .trim_end(),
        "task-output"
    );
    assert!(!source.join(".qq/task-home").exists());
    let connection = rusqlite::Connection::open(fixture.path("data/sessions.sqlite3")).unwrap();
    let record: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key LIKE 'qq_task_workspace:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let record: Value = serde_json::from_str(&record).unwrap();
    let task = PathBuf::from(record["scope"]["workspace"]["path"].as_str().unwrap());
    assert!(task.starts_with(fixture.path("data/task-workspaces")));
    for (program, _, _) in &tool_versions {
        let name = format!("version-probe-{program}.cmd");
        let path = task.join(&name);
        let imported = fs::read(&path).unwrap_or_else(|error| {
            panic!("persisted task must contain owned probe script {path:?}: {error}")
        });
        eprintln!(
            "Windows Auto imported probe: {name} ({} bytes)",
            imported.len()
        );
        assert_eq!(
            imported,
            fs::read(source.join(&name)).unwrap(),
            "task import must preserve owned probe script {name}",
        );
    }
    let absolute_result = fs::read_to_string(task.join("tool-input-result.json"));
    eprintln!("Windows Auto absolute tool inputs: {absolute_result:?}");
    let absolute_result: Value = serde_json::from_str(
        &absolute_result.expect("absolute task-owned probe must publish its result"),
    )
    .unwrap();
    assert_eq!(
        absolute_result["private_read"]["kind"], "PermissionDenied",
        "same-token absolute probe must not read private canary"
    );
    let diagnostics = tool_versions
        .iter()
        .map(|(program, _, host_resolved)| {
            let read = |suffix: &str| {
                fs::read_to_string(source.join(format!("{program}-{suffix}.txt")))
                    .unwrap_or_else(|error| format!("<missing diagnostic: {error}>"))
            };
            json!({
                "program": program,
                "host_resolved": host_resolved.trim(),
                "payload_resolved": read("resolved"),
                "resolve_exit": read("resolve-exit"),
                "resolve_stderr": read("resolve-stderr"),
                "exit": read("exit"),
                "stdout": read("version"),
                "stderr": read("stderr"),
            })
        })
        .collect::<Vec<_>>();
    let read_roots = record["scope"]["read_only"]
        .as_array()
        .unwrap()
        .iter()
        .map(|grant| grant["path"].clone())
        .collect::<Vec<_>>();
    eprintln!(
        "Windows Auto tool inputs: {}",
        json!({"read_roots": read_roots, "tools": diagnostics})
    );
    for (program, expected, _) in &tool_versions {
        assert_eq!(
            fs::read_to_string(source.join(format!("{program}-exit.txt")))
                .unwrap_or_else(|error| {
                    panic!(
                        "installed {program} probe did not publish its exit status: {error}; diagnostics: {diagnostics:#?}",
                    )
                })
                .trim(),
            "0",
            "Auto must execute installed {program} successfully: {diagnostics:#?}; read roots: {read_roots:?}",
        );
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
    assert_eq!(record["scope"]["network"], "isolated");
    assert_eq!(record["scope"]["resources"]["access"], "committed");
    assert_eq!(record["scope"]["resources"]["cpu_percent"], 90);
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
