//! Real HTTP parsing and timeout failures feeding Strict durable settlement.
use super::*;
use futures_util::StreamExt;

#[tokio::test]
async fn strict_typesafe_transport_failures_stay_unavailable_without_semantic_repair() {
    for failure in ["malformed", "network", "timeout", "oversized"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = if failure == "network" {
            drop(listener);
            None
        } else {
            Some(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    assert!(request.len() <= 64 * 1024);
                    if let Some(end) = request.windows(4).position(|p| p == b"\r\n\r\n") {
                        let length: usize = std::str::from_utf8(&request[..end])
                            .unwrap()
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                if failure == "timeout" {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    return;
                }
                let body = if failure == "oversized" {
                    "x".repeat(65_537)
                } else {
                    "not-json".into()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                // The oversized-body client may close as soon as its bound is known.
                let _ = socket.write_all(response.as_bytes()).await;
            }))
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reviewer = TypeSafeCheckpointReviewer {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_millis(100))
                .build()
                .unwrap(),
            endpoint: format!("http://{address}/").into(),
            mode: qq_config::JevReviewMode::Strict,
        };
        let model = Arc::new(
            Runtime::new(
                CapturingProvider {
                    requests: Arc::clone(&requests),
                },
                "test/model",
                256,
            )
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(reviewer)),
        );
        let fixture = RuntimeFixture::new();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(fixture.path("strict.sqlite3")),
            Arc::new(FixedRuntimeLoader { runtime: model }),
        )
        .await
        .unwrap();
        let resolved = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: fixture.path("work").display().to_string(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
            panic!("workspace")
        };
        let created = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".into()),
                        ..ModelSelection::default()
                    },
                    approval_mode: ApprovalMode::ReadOnly,
                    profile: AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut stream = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text("verify")],
                    limits: qq_protocol::RunLimits {
                        max_model_turns: Some(8),
                        ..qq_protocol::RunLimits::default()
                    },
                    correlation: qq_protocol::Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let events = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut events = Vec::new();
            loop {
                let event = stream.next().await.unwrap().unwrap();
                let finished = matches!(event.event, qq_protocol::SessionEvent::RunFinished { .. });
                events.push(event);
                if finished {
                    break events;
                }
            }
        })
        .await
        .expect("local reviewer failure must settle");
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "{failure}: no semantic repair turn"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    &e.event,
                    qq_protocol::SessionEvent::CheckpointReviewed {
                        outcome: qq_protocol::CheckpointOutcome::Unavailable,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(!events.iter().any(|e| matches!(
            &e.event,
            qq_protocol::SessionEvent::CheckpointReviewed {
                outcome: qq_protocol::CheckpointOutcome::Contradicted,
                ..
            }
        )));
        assert!(
            matches!(&events.last().unwrap().event, qq_protocol::SessionEvent::RunFinished {
            outcome: qq_protocol::RunOutcome::Failed { failure }, verification: Some(v), ..
        } if failure.kind == RunFailureKind::VerificationUnavailable && v.state == qq_protocol::VerificationState::Unavailable),
            "{failure}"
        );
        runtime.shutdown().await.unwrap();
        if let Some(server) = server {
            server.await.unwrap();
        }
    }
}
