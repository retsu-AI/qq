use super::*;
use futures_util::StreamExt;
use qq_client::{ClientPort as _, ClientRequest, ClientUpdate, ConnectionState};
use qq_protocol::{ApprovalMode, CommandRequest, InputPart, SessionEvent};

fn steering_count(request: &ModelRequest) -> usize {
    request
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .map(|block| match block {
            qq_provider::ContentBlock::Text { text } => {
                text.matches("write corrected artifact").count()
            }
            _ => 0,
        })
        .sum()
}

struct SteeredProvider {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for SteeredProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request.clone());
        drop(requests);
        let (name, arguments) = match turn {
            0 => ("read_file", serde_json::json!({"path":"stale.txt"})),
            1 => {
                let (path, content) = if steering_count(&request) == 1 {
                    ("result.txt", "steered")
                } else {
                    ("stale.txt", "incorrect")
                };
                (
                    "write_file",
                    serde_json::json!({"path":path,"content":content}),
                )
            }
            2 => (
                "update_goal",
                serde_json::json!({"wait":{"seconds":60,"reason":"await external verification"}}),
            ),
            _ => {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "artifact ready".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
        };
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        Box::pin(
            stream::once(async move {
                if turn == 0 {
                    entered.notify_one();
                    release.notified().await;
                }
                vec![
                    Ok(ProviderEvent::ToolCallStarted {
                        id: format!("call-{turn}"),
                        name: name.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: format!("call-{turn}"),
                        json: arguments.to_string(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: format!("call-{turn}"),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]
            })
            .flat_map(stream::iter),
        )
    }
}

async fn next_update(tui: &mut qq_client::TuiClient, stage: &str) -> ClientUpdate {
    tokio::time::timeout(Duration::from_secs(10), tui.recv())
        .await
        .unwrap_or_else(|_| panic!("bounded TUI update during {stage}"))
        .expect("client remains alive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_goal_reconnect_applies_steering_without_restarting_work() {
    let fixture = RuntimeFixture::new();
    fs::write(fixture.path("work/stale.txt"), "original").unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(
        Runtime::new(
            SteeredProvider {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
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
        factory: fixture.factory_with_credentials(CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        )),
    });
    let options = || ServerOptions::new(ServerPaths::new(fixture.path("server")));
    let StartOutcome::Started(server) =
        qq_server::start(handler.clone(), handler.server_identity(None), options())
            .await
            .unwrap()
    else {
        panic!("fresh server")
    };
    let connection = server.connection().to_server_connection();
    let http = qq_client::SessionClient::new(connection.clone()).unwrap();
    let (workspace_id, _) = http.resolve_workspace(&fixture.path("work")).await.unwrap();
    let selection = ModelSelection {
        model_is_fallback: false,
        model: Some("test/model".to_owned()),
        max_output_tokens: Some(256),
        organization: None,
    };
    let created = http
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: selection.clone(),
                approval_mode: ApprovalMode::Auto,
                profile: qq_protocol::AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Default::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    let set = http.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
        "type":"set_goal", "session_id":session_id, "objective":"prepare an artifact", "budget":{"max_goal_runs":3,"deadline_ms":deadline}
    })).unwrap()).await.unwrap();
    let CommandOutcome::GoalUpdated { goal: original, .. } = set.outcome else {
        panic!("goal")
    };
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let resolver = Arc::new(Mutex::new(Some(connection.clone())));
    let lookup = Arc::clone(&resolver);
    let mut tui = qq_client::TuiClient::start(
        connection,
        fixture.path("work"),
        selection,
        qq_client::InitialSession::Open(session_id),
        move || {
            let connection = lookup.lock().unwrap().clone();
            std::future::ready(connection)
        },
    )
    .unwrap();
    let mut initial = None;
    loop {
        match next_update(&mut tui, "bootstrap").await {
            ClientUpdate::Snapshot(snapshot) => initial = Some(snapshot),
            ClientUpdate::Connection(ConnectionState::Live) => break,
            ClientUpdate::SnapshotFailed(error) => panic!("{}", error.message()),
            _ => {}
        }
    }
    let initial = initial.unwrap();
    let initial_goal = initial.focused.as_ref().unwrap().goal.as_ref().unwrap();
    assert_eq!(initial_goal.goal_id, original.goal_id);
    assert_eq!(
        initial_goal.counters.runs, 0,
        "the held first run has not settled"
    );
    *resolver.lock().unwrap() = None;
    server.shutdown().await.unwrap();
    let StartOutcome::Started(server) =
        qq_server::start(handler.clone(), handler.server_identity(None), options())
            .await
            .unwrap()
    else {
        panic!("replacement server")
    };
    let replacement = server.connection().to_server_connection();
    assert_ne!(replacement, http.connection().clone());
    *resolver.lock().unwrap() = Some(replacement);
    let mut reset = None;
    loop {
        match next_update(&mut tui, "reconnect").await {
            ClientUpdate::ResetSnapshot(snapshot) => reset = Some(snapshot),
            ClientUpdate::Connection(ConnectionState::Live) if reset.is_some() => break,
            ClientUpdate::SnapshotFailed(error) => panic!("{}", error.message()),
            _ => {}
        }
    }
    let reset = reset.unwrap();
    assert_eq!(reset.sessions.len(), 1);
    assert_eq!(reset.focused.as_ref().unwrap().summary.id, session_id);
    assert_eq!(
        reset.focused.as_ref().unwrap().goal.as_ref(),
        Some(initial_goal)
    );
    let run_id = reset
        .focused
        .as_ref()
        .unwrap()
        .summary
        .active_run_id
        .unwrap();
    let steering_id = CommandId::generate().unwrap();
    tui.try_send(ClientRequest::Command(CommandRequest {
        command_id: steering_id,
        command: SessionCommand::SteerRun {
            run_id,
            input: vec![InputPart::text("write corrected artifact")],
            interrupt: false,
        },
    }))
    .unwrap();
    loop {
        if let ClientUpdate::CommandResult { command_id, result } =
            next_update(&mut tui, "steer receipt").await
            && command_id == steering_id
        {
            result.unwrap();
            break;
        }
    }
    release.notify_one();
    let mut applied = 0;
    let mut writes = 0;
    loop {
        if let ClientUpdate::Event(event) = next_update(&mut tui, "artifact completion").await {
            match event.event {
                SessionEvent::SteeringApplied { .. } => applied += 1,
                SessionEvent::ToolCallStarted { tool_call } if tool_call.name == "write_file" => {
                    assert_eq!(
                        applied, 1,
                        "steering commits before the corrected write starts"
                    );
                    writes += 1;
                }
                SessionEvent::ToolApprovalRequested { .. } => {
                    panic!("Auto unexpectedly requested approval")
                }
                SessionEvent::RunFinished {
                    run_id: finished,
                    outcome,
                    ..
                } if finished == run_id => {
                    assert_eq!(outcome, qq_protocol::RunOutcome::Completed);
                    break;
                }
                _ => {}
            }
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(writes, 1, "reconnect must not duplicate the file effect");
    assert_eq!(
        fs::read_to_string(fixture.path("work/result.txt")).unwrap(),
        "steered"
    );
    assert_eq!(
        fs::read_to_string(fixture.path("work/stale.txt")).unwrap(),
        "original"
    );
    let snapshot = handler
        .sessions()
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    let focused = snapshot.focused.unwrap();
    let goal = focused.goal.unwrap();
    assert_eq!(goal.goal_id, original.goal_id);
    assert_eq!(goal.budget, original.budget);
    assert_eq!(goal.counters.runs, 1);
    assert!(goal.wait.is_some());
    assert_eq!(focused.runs.len(), 1);
    assert_eq!(focused.runs[0].goal.unwrap().goal_id, goal.goal_id);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(steering_count(&requests[1]), 1);
    }
    drop(tui);
    server.shutdown().await.unwrap();
    handler.shutdown().await.unwrap();
}
