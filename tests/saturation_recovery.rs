#![forbid(unsafe_code)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_util::StreamExt;
use qq_client::{ClientError, SessionClient};
use qq_core::{
    LoadedRuntime, PublishedEventStream, Runtime, RuntimeLoadFuture, RuntimeLoadRequest,
    RuntimeLoader, SessionRuntime, SessionRuntimeOptions,
};
use qq_protocol::{
    ApprovalMode, CommandId, CommandOutcome, CommandRequest, Correlation, InputPart,
    LocalServerConnection, ModelSelection, RunLimits, SessionCommand, SessionEvent,
    SnapshotRequest, SubscribeRequest,
};
use qq_server::{CommandFuture, ServerHandler, ServerHandlerError, SnapshotFuture};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Loader(Arc<AtomicUsize>);

impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let calls = Arc::clone(&self.0);
        Box::pin(async move {
            struct Provider(Arc<AtomicUsize>);
            impl qq_provider::Provider for Provider {
                fn stream(&self, _: qq_provider::ModelRequest) -> qq_provider::ProviderStream {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Box::pin(futures_util::stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta {
                            text: "one execution".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
            Ok(LoadedRuntime::from_runtime(
                Runtime::new(Provider(calls), "test-model", 256).unwrap(),
                request.profile,
                request.workspace.into(),
            )
            .await
            .unwrap())
        })
    }
}

struct Handler(SessionRuntime);

impl ServerHandler for Handler {
    fn command(&self, request: CommandRequest) -> CommandFuture {
        let runtime = self.0.clone();
        Box::pin(async move {
            runtime
                .command(request.command_id, request.command)
                .await
                .map_err(|error| ServerHandlerError::InvalidRequest(error.to_string()))
        })
    }

    fn snapshot(&self, request: SnapshotRequest) -> SnapshotFuture {
        let runtime = self.0.clone();
        Box::pin(async move {
            runtime
                .snapshot(request)
                .await
                .map_err(|error| ServerHandlerError::InvalidRequest(error.to_string()))
        })
    }

    fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<PublishedEventStream, ServerHandlerError> {
        self.0
            .subscribe_published(request)
            .map_err(|_| ServerHandlerError::Internal)
    }
}

#[tokio::test]
async fn saturation_lost_receipt_retry_and_sse_reconnect_execute_the_durable_command_once() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(workspace.join("sessions.sqlite3")),
        Arc::new(Loader(Arc::clone(&calls))),
    )
    .await
    .unwrap();
    let server = match qq_server::start(
        Arc::new(Handler(runtime.clone())),
        qq_server::ServerIdentity::new(runtime.store_id(), Some("saturation")),
        qq_server::ServerOptions::new(qq_server::ServerPaths::new(workspace.join("server"))),
    )
    .await
    .unwrap()
    {
        qq_server::StartOutcome::Started(server) => server,
        qq_server::StartOutcome::Existing(_) => panic!("unexpected existing server"),
    };
    let client = SessionClient::new(server.connection().clone()).unwrap();
    let (workspace_id, _) = client.resolve_workspace(&workspace).await.unwrap();
    let created = client
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    ..ModelSelection::default()
                },
                approval_mode: ApprovalMode::ReadOnly,
                profile: Default::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session not created")
    };
    let mut original = client
        .events(workspace_id, created.committed_through)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy_client = client.clone();
    let proxy = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut wire = Vec::new();
        let body_start = loop {
            let byte = socket.read_u8().await.unwrap();
            wire.push(byte);
            assert!(wire.len() <= 16 * 1024);
            if wire.ends_with(b"\r\n\r\n") {
                break wire.len();
            }
        };
        let headers = std::str::from_utf8(&wire).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        assert!(length <= qq_protocol::MAX_REQUEST_BYTES);
        wire.resize(body_start + length, 0);
        socket.read_exact(&mut wire[body_start..]).await.unwrap();
        let request: CommandRequest = serde_json::from_slice(&wire[body_start..]).unwrap();
        let committed = proxy_client
            .command(request.command_id, request.command)
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
        committed
    });
    let interrupted = SessionClient::new(
        LocalServerConnection::new(
            address,
            server.connection().expose_bearer_token().to_owned(),
            server.connection().server_info().clone(),
        )
        .unwrap(),
    )
    .unwrap();
    let id = CommandId::generate().unwrap();
    let command = SessionCommand::SubmitPrompt {
        session_id,
        input: vec![InputPart::text("execute exactly once")],
        limits: RunLimits::default(),
        correlation: Correlation::default(),
        output: None,
    };
    assert_eq!(
        interrupted.command(id, command.clone()).await.unwrap_err(),
        ClientError::Unavailable
    );
    let committed = proxy.await.unwrap();
    let first = original.next().await.unwrap().unwrap();
    assert_eq!(first.caused_by, Some(id));
    drop(original);
    let retried = client.command(id, command.clone()).await.unwrap();
    assert_eq!(retried, committed);
    let CommandOutcome::PromptQueued { run_id, .. } = retried.outcome else {
        panic!("prompt not queued")
    };
    let mut resumed = client
        .events(workspace_id, created.committed_through)
        .await
        .unwrap();
    let mut cursor = created.committed_through;
    let mut queued = 0;
    loop {
        let event = resumed.next().await.unwrap().unwrap();
        assert_eq!(event.cursor.sequence, cursor.sequence + 1);
        cursor = event.cursor;
        if matches!(event.event, SessionEvent::PromptQueued { .. }) {
            queued += 1;
        }
        if event.run_id == Some(run_id) && matches!(event.event, SessionEvent::RunFinished { .. }) {
            break;
        }
    }
    assert_eq!(queued, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(client.command(id, command).await.unwrap(), committed);
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 32, 32))
        .await
        .unwrap();
    assert_eq!(snapshot.cursor, cursor);
    assert_eq!(
        snapshot
            .focused
            .unwrap()
            .messages
            .iter()
            .filter(|message| message.run_id == run_id)
            .count(),
        2
    );
    drop(resumed);
    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}
