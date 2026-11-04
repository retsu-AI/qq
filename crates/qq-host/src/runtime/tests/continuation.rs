use super::*;
use qq_client::{ClientPort as _, ClientUpdate};

async fn next_update(tui: &mut qq_client::TuiClient, stage: &str) -> ClientUpdate {
    tokio::time::timeout(Duration::from_secs(10), tui.recv())
        .await
        .unwrap_or_else(|_| panic!("bounded TUI update during {stage}"))
        .expect("client remains alive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_workspace_recovery_retries_validated_initial_session_creation() {
    let fixture = RuntimeFixture::new();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(version: 1, model: "custom/other", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: { "other": (name: "Other") }) })"#,
    )
    .unwrap();
    let runtime = Arc::new(
        Runtime::new(
            CapturingProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "test/model",
            256,
        )
        .unwrap(),
    );
    let durable = SessionRuntime::open(
        SessionRuntimeOptions::new(fixture.path("sessions.sqlite3")),
        Arc::new(FixedRuntimeLoader { runtime }),
    )
    .await
    .unwrap();
    let handler = Arc::new(RuntimeHandler {
        durable,
        factory: fixture.factory(),
    });
    let options = || ServerOptions::new(ServerPaths::new(fixture.path("server")));
    let StartOutcome::Started(server) =
        qq_server::start(handler.clone(), handler.server_identity(None), options())
            .await
            .unwrap()
    else {
        panic!("fresh server")
    };
    let resolver = Arc::new(Mutex::new(None));
    let lookup = Arc::clone(&resolver);
    let selection = ModelSelection {
        model_is_fallback: false,
        model: Some("custom/test-model".to_owned()),
        max_output_tokens: Some(256),
        organization: None,
    };
    let mut tui = qq_client::TuiClient::start(
        server.connection().to_server_connection(),
        fixture.path("work"),
        selection,
        qq_client::InitialSession::Existing,
        move || std::future::ready(lookup.lock().unwrap().clone()),
    )
    .unwrap();
    let mut saw_empty = false;
    let mut saw_invalid_catalog = false;
    while !saw_empty || !saw_invalid_catalog {
        match next_update(&mut tui, "initial empty workspace").await {
            ClientUpdate::Snapshot(snapshot) => {
                assert!(snapshot.sessions.is_empty());
                saw_empty = true;
            }
            ClientUpdate::Models { selected, .. } => {
                assert!(selected.is_none());
                saw_invalid_catalog = true;
            }
            ClientUpdate::SnapshotFailed(error) => panic!("{}", error.message()),
            _ => {}
        }
    }
    fs::write(
        fixture.path("global/config.ron"),
        r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
    )
    .unwrap();
    server.shutdown().await.unwrap();
    let StartOutcome::Started(server) =
        qq_server::start(handler.clone(), handler.server_identity(None), options())
            .await
            .unwrap()
    else {
        panic!("replacement server")
    };
    *resolver.lock().unwrap() = Some(server.connection().to_server_connection());
    let created = loop {
        match next_update(&mut tui, "empty workspace recovery").await {
            ClientUpdate::Snapshot(snapshot) if !snapshot.sessions.is_empty() => break snapshot,
            ClientUpdate::SnapshotFailed(error) => panic!("{}", error.message()),
            _ => {}
        }
    };
    assert_eq!(created.sessions.len(), 1);
    assert_eq!(
        created.focused.as_ref().unwrap().summary.id,
        created.sessions[0].id
    );

    drop(tui);
    server.shutdown().await.unwrap();
    handler.shutdown().await.unwrap();
}
