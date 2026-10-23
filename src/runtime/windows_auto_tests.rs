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
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    // Prove the host listener is reachable before the contained command tries it.
    drop(std::net::TcpStream::connect(address).unwrap());
    fs::write(source.join("verify.js"), format!(r#"
const fs = require('fs');
const net = require('net');
try {{
  fs.readFileSync({secret});
  throw new Error('host secret was readable');
}} catch (error) {{
  if (!['EACCES', 'EPERM'].includes(error.code)) throw error;
}}
const socket = net.connect({port}, '127.0.0.1');
socket.setTimeout(5000, () => {{ socket.destroy(); throw new Error('network denial was not confirmed'); }});
socket.once('connect', () => {{ socket.destroy(); throw new Error('host loopback was reachable'); }});
socket.once('error', () => fs.writeFileSync('node-output.txt', 'node task output; host read and loopback denied'));
"#, secret = serde_json::to_string(&secret).unwrap(), port = address.port())).unwrap();
    fs::write(
        source.join("Cargo.toml"),
        r#"[package]
name = "native-auto-fixture"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::create_dir(source.join("src")).unwrap();
    fs::write(
        source.join("src/lib.rs"),
        r#"
#[test]
fn writes_verified_artifact() {
    let value: u32 = (1..=100).sum();
    assert_eq!(value, 5050);
    std::fs::write("rust-tested.txt", value.to_string()).unwrap();
}
"#,
    )
    .unwrap();
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
    script.push_str(" & node verify.js & cargo test --offline >cargo-test-output.txt 2>&1");
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
    assert_eq!(fs::read_to_string(&secret).unwrap(), "host bytes");
    assert_eq!(
        fs::read_to_string(source.join("result with spaces.txt"))
            .unwrap()
            .trim_end(),
        "task-output"
    );
    assert!(!source.join(".qq/task-home").exists());
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
        "node task output; host read and loopback denied"
    );
    assert_eq!(
        fs::read_to_string(source.join("rust-tested.txt")).unwrap(),
        "5050"
    );
    let cargo_output = fs::read_to_string(source.join("cargo-test-output.txt")).unwrap();
    assert!(
        cargo_output.contains("1 passed; 0 failed"),
        "{cargo_output}"
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
