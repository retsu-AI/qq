use super::*;

#[tokio::test]
async fn direct_and_server_commands_resolve_identical_slash_guidance() {
    let fixture = RuntimeFixture::new();
    fs::create_dir_all(fixture.path("work/.qq/skills/review")).unwrap();
    fs::write(
        fixture.path("work/.qq/skills/review/SKILL.md"),
        "Review cancellation and persistence invariants.\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model_runtime = Arc::new(
        Runtime::new(
            CapturingProvider {
                requests: Arc::clone(&requests),
            },
            "test/model",
            256,
        )
        .unwrap(),
    );
    let durable = SessionRuntime::open(
        SessionRuntimeOptions::new(fixture.path("sessions.sqlite3")),
        Arc::new(FixedRuntimeLoader {
            runtime: model_runtime,
        }),
    )
    .await
    .unwrap();
    let handler = Arc::new(RuntimeHandler {
        durable,
        factory: fixture.factory(),
    });
    let resolved = handler
        .sessions()
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: fixture.path("work").display().to_string(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        panic!("unexpected receipt")
    };
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let created = handler
            .sessions()
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model_is_fallback: false,
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: qq_protocol::ApprovalMode::Ask,
                    profile: qq_protocol::AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        sessions.push(session_id);
    }

    let direct = handler
        .sessions()
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: sessions[0],
                input: vec![qq_protocol::InputPart::text(
                    "/review focus on cancellation".to_owned(),
                )],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: direct_run, ..
    } = direct.outcome
    else {
        panic!("unexpected receipt")
    };
    let direct_identity =
        completed_prompt_identity(handler.sessions(), workspace_id, sessions[0], direct_run).await;

    let server = match qq_server::start(
        handler.clone(),
        handler.server_identity(None),
        ServerOptions::new(ServerPaths::new(fixture.path("server"))),
    )
    .await
    .unwrap()
    {
        StartOutcome::Started(server) => server,
        StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
    };
    let server_command = CommandRequest {
        command_id: CommandId::generate().unwrap(),
        command: SessionCommand::SubmitPrompt {
            session_id: sessions[1],
            input: vec![qq_protocol::InputPart::text(
                "/review focus on cancellation".to_owned(),
            )],
            limits: qq_protocol::RunLimits::default(),
            correlation: qq_protocol::Correlation::default(),
            output: None,
        },
    };
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(server.connection().endpoint("/v1/sessions/prompts"))
        .bearer_auth(server.connection().expose_bearer_token())
        .json(&server_command)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let receipt = response
        .json::<qq_protocol::CommandReceipt>()
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: server_run, ..
    } = receipt.outcome
    else {
        panic!("unexpected receipt")
    };
    let server_identity =
        completed_prompt_identity(handler.sessions(), workspace_id, sessions[1], server_run).await;

    assert_eq!(direct_identity, server_identity);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system(), requests[1].system());
        assert!(
            requests[0]
                .system()
                .unwrap()
                .contains("Review cancellation and persistence invariants.")
        );
    }
    server.shutdown().await.unwrap();
    handler.shutdown().await.unwrap();
}

struct CollectingSink {
    seen: Vec<qq_protocol::SessionEventEnvelope>,
    stop_at: Option<u64>,
    stall_first: Option<tokio::sync::oneshot::Receiver<()>>,
    disconnects: usize,
}

impl qq_client::observer::EventSink for CollectingSink {
    fn deliver(
        &mut self,
        event: &qq_protocol::SessionEventEnvelope,
    ) -> qq_client::observer::SinkFuture<'_> {
        let event = event.clone();
        Box::pin(async move {
            if let Some(gate) = self.stall_first.take() {
                let _ = gate.await;
            }
            let sequence = event.cursor.sequence;
            self.seen.push(event);
            if self.stop_at == Some(sequence) {
                qq_client::observer::ObserverStep::Stop
            } else {
                qq_client::observer::ObserverStep::Continue
            }
        })
    }

    fn disconnected(
        &mut self,
        _error: &qq_client::ClientError,
        _resume_from: &qq_protocol::EventCursor,
    ) -> qq_client::observer::ObserverStep {
        self.disconnects += 1;
        qq_client::observer::ObserverStep::Continue
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observers_fall_behind_restart_and_converge_without_delaying_commits() {
    let fixture = RuntimeFixture::new();
    fs::create_dir_all(fixture.path("work")).unwrap();
    let model_runtime = Arc::new(
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
        Arc::new(FixedRuntimeLoader {
            runtime: model_runtime,
        }),
    )
    .await
    .unwrap();
    let handler = Arc::new(RuntimeHandler {
        durable,
        factory: fixture.factory(),
    });
    let server = match qq_server::start(
        handler.clone(),
        handler.server_identity(None),
        ServerOptions::new(ServerPaths::new(fixture.path("server"))),
    )
    .await
    .unwrap()
    {
        StartOutcome::Started(server) => server,
        StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
    };
    let client = qq_client::SessionClient::new(server.connection().clone()).unwrap();
    let (workspace_id, origin) = client
        .resolve_workspace(&fixture.path("work"))
        .await
        .unwrap();
    let created = client
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: qq_protocol::ApprovalMode::Ask,
                profile: qq_protocol::AgentProfileId::default(),
                reasoning_effort: None,
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };

    let live_client = client.clone();
    let live_origin = origin;
    let live = tokio::spawn(async move {
        let mut stream = live_client.events(workspace_id, live_origin).await.unwrap();
        let mut seen = Vec::new();
        while let Some(Ok(event)) = futures_util::StreamExt::next(&mut stream).await {
            let done = matches!(&event.event, qq_protocol::SessionEvent::RunFinished { .. })
                && seen.len() > 300;
            seen.push(event);
            if done {
                break;
            }
        }
        seen
    });

    let (release, gate) = tokio::sync::oneshot::channel();
    let mut stalled = CollectingSink {
        seen: Vec::new(),
        stop_at: None,
        stall_first: Some(gate),
        disconnects: 0,
    };
    let stalled_client = client.clone();
    let stalled_origin = origin;
    let stalled_task = tokio::spawn(async move {
        qq_client::observer::run(&stalled_client, workspace_id, stalled_origin, &mut stalled).await
    });

    for i in 0..40 {
        loop {
            match client
                .submit(
                    session_id,
                    vec![qq_protocol::InputPart::text(format!("prompt {i}"))],
                    qq_protocol::RunLimits::default(),
                    qq_protocol::Correlation::default(),
                )
                .await
            {
                Ok(receipt) => {
                    assert!(matches!(
                        receipt.outcome,
                        CommandOutcome::PromptQueued { .. }
                    ));
                    break;
                }
                Err(qq_client::ClientError::ServerMessage { status: 503, .. }) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("submit failed: {error}"),
            }
        }
    }
    let live_events = tokio::time::timeout(Duration::from_secs(30), live)
        .await
        .unwrap()
        .unwrap();
    assert!(
        live_events.len() > 128,
        "{} events is not more than one page",
        live_events.len()
    );
    stalled_task.abort();
    drop(release);

    let midpoint = live_events[live_events.len() / 2].cursor.sequence;
    let mut first = CollectingSink {
        seen: Vec::new(),
        stop_at: Some(midpoint),
        stall_first: None,
        disconnects: 0,
    };
    let (exit, resume_from) = tokio::time::timeout(
        Duration::from_secs(30),
        qq_client::observer::run(&client, workspace_id, origin, &mut first),
    )
    .await
    .unwrap();
    assert_eq!(exit, qq_client::observer::ObserverExit::Stopped);
    assert_eq!(resume_from.sequence, midpoint - 1);
    first.seen.pop();
    let last_live = live_events.last().unwrap().cursor.sequence;
    let mut second = CollectingSink {
        seen: Vec::new(),
        stop_at: Some(last_live),
        stall_first: None,
        disconnects: 0,
    };
    let (exit, _) = tokio::time::timeout(
        Duration::from_secs(30),
        qq_client::observer::run(&client, workspace_id, resume_from, &mut second),
    )
    .await
    .unwrap();
    assert_eq!(exit, qq_client::observer::ObserverExit::Stopped);
    let mut replayed = first.seen;
    replayed.extend(second.seen);
    assert_eq!(replayed.len(), live_events.len());
    for (replayed, live) in replayed.iter().zip(&live_events) {
        assert_eq!(
            serde_json::to_vec(replayed).unwrap(),
            serde_json::to_vec(live).unwrap(),
            "replay must be byte-identical to live delivery"
        );
    }
    assert_eq!(first.disconnects + second.disconnects, 0);

    let foreign = qq_protocol::EventCursor {
        store_id: qq_protocol::StoreId::generate().unwrap(),
        workspace_id,
        sequence: 1,
    };
    let mut sink = CollectingSink {
        seen: Vec::new(),
        stop_at: None,
        stall_first: None,
        disconnects: 0,
    };
    let (exit, _) = tokio::time::timeout(
        Duration::from_secs(10),
        qq_client::observer::run(&client, workspace_id, foreign, &mut sink),
    )
    .await
    .unwrap();
    assert_eq!(exit, qq_client::observer::ObserverExit::CursorRejected);
    assert!(sink.seen.is_empty());

    server.shutdown().await.unwrap();
    handler.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_client_delivers_workspace_capabilities_and_refreshes_them() {
    use qq_client::{ClientPort as _, ClientRequest, ClientUpdate};

    let fixture = RuntimeFixture::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir_all(fixture.path("work/.qq/skills/qq-verify")).unwrap();
    fs::write(
        fixture.path("work/.qq/skills/qq-verify/SKILL.md"),
        "---\ndescription: Run the gates.\n---\n# Verify\n",
    )
    .unwrap();
    fs::write(
        fixture.path("work/.qq/config.ron"),
        r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
    )
    .unwrap();
    let model_runtime = Arc::new(
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
        Arc::new(FixedRuntimeLoader {
            runtime: model_runtime,
        }),
    )
    .await
    .unwrap();
    let handler = Arc::new(RuntimeHandler {
        durable,
        factory: fixture.factory(),
    });
    let server = match qq_server::start(
        handler.clone(),
        handler.server_identity(None),
        ServerOptions::new(ServerPaths::new(fixture.path("server"))),
    )
    .await
    .unwrap()
    {
        StartOutcome::Started(server) => server,
        StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
    };
    let request = LoadRequest::new(fixture.path("work"));
    handler
        .factory
        .inner
        .config
        .grant_pending_trust(&request)
        .unwrap();
    let mut tui = qq_client::TuiClient::start(
        server.connection().to_server_connection(),
        fixture.path("work"),
        ModelSelection {
            model_is_fallback: false,
            model: Some("custom/test-model".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        },
        qq_client::InitialSession::Existing,
        || std::future::ready(None),
    )
    .unwrap();
    async fn next_capabilities(
        tui: &mut qq_client::TuiClient,
    ) -> Arc<qq_protocol::ServerCapabilities> {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), tui.recv())
                .await
                .expect("client update")
                .expect("client alive")
            {
                ClientUpdate::Capabilities(capabilities) => return capabilities,
                ClientUpdate::SnapshotFailed(failure) => panic!("{}", failure.message()),
                _ => {}
            }
        }
    }
    let initial = next_capabilities(&mut tui).await;
    let profiles = initial
        .profiles
        .as_deref()
        .expect("workspace-scoped document");
    assert_eq!(profiles.len(), 1);
    assert!(profiles[0].id.is_default());
    let skills = &initial.workspace_tools.as_ref().unwrap().skills;
    assert_eq!(skills.indexed, 1);
    assert_eq!(skills.entries.len(), 1);
    assert_eq!(skills.entries[0].name, "qq-verify");
    assert_eq!(skills.entries[0].kind, qq_protocol::GuidanceKind::Skill);
    assert_eq!(skills.entries[0].source, ".qq/skills/qq-verify/SKILL.md");
    assert_eq!(skills.entries[0].description, "Run the gates.");
    assert!(skills.entries[0].disclosed);
    assert!(initial.steering.boundary);

    fs::write(
        fixture.path("work/.qq/config.ron"),
        r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) }, profiles: { "reviewer": Profile(approval_mode: read_only) })"#,
    )
    .unwrap();
    handler
        .factory
        .inner
        .config
        .grant_pending_trust(&request)
        .unwrap();
    tui.try_send(ClientRequest::Capabilities).unwrap();
    let refreshed = next_capabilities(&mut tui).await;
    let profiles = refreshed.profiles.as_deref().unwrap();
    let reviewer = profiles
        .iter()
        .find(|profile| profile.id.as_str() == "reviewer")
        .expect("configured profile advertised");
    assert_eq!(reviewer.approval_mode, qq_protocol::ApprovalMode::ReadOnly);

    drop(tui);
    server.shutdown().await.unwrap();
    handler.shutdown().await.unwrap();
}

#[tokio::test]
async fn tui_client_opens_new_named_or_existing_sessions_and_refuses_bad_ids() {
    use qq_client::{ClientPort as _, ClientUpdate, InitialSession};

    let fixture = RuntimeFixture::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir_all(fixture.path("work")).unwrap();
    fs::create_dir_all(fixture.path("elsewhere")).unwrap();
    let model_runtime = Arc::new(
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
        Arc::new(FixedRuntimeLoader {
            runtime: model_runtime,
        }),
    )
    .await
    .unwrap();
    let handler = Arc::new(RuntimeHandler {
        durable,
        factory: fixture.factory(),
    });
    let server = match qq_server::start(
        handler.clone(),
        handler.server_identity(None),
        ServerOptions::new(ServerPaths::new(fixture.path("server"))),
    )
    .await
    .unwrap()
    {
        StartOutcome::Started(server) => server,
        StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
    };
    let selection = ModelSelection {
        model_is_fallback: false,
        model: Some("custom/test-model".to_owned()),
        max_output_tokens: Some(256),
        organization: None,
    };
    let start = |workspace: &str, initial: InitialSession| {
        qq_client::TuiClient::start(
            server.connection().to_server_connection(),
            fixture.path(workspace),
            selection.clone(),
            initial,
            || std::future::ready(None),
        )
        .unwrap()
    };
    async fn first_snapshot(
        tui: &mut qq_client::TuiClient,
    ) -> Result<qq_protocol::WorkspaceSnapshot, String> {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), tui.recv())
                .await
                .expect("client update")
                .expect("client alive")
            {
                ClientUpdate::Snapshot(snapshot) => return Ok(snapshot),
                ClientUpdate::SnapshotFailed(failure) => {
                    return Err(failure.message().to_owned());
                }
                _ => {}
            }
        }
    }

    let mut first = start("work", InitialSession::New(selection.clone()));
    let one = first_snapshot(&mut first).await.unwrap();
    let first_id = one.focused.as_ref().expect("focused").summary.id;
    assert_eq!(one.sessions.len(), 1);
    drop(first);
    let mut second = start("work", InitialSession::New(selection.clone()));
    let two = first_snapshot(&mut second).await.unwrap();
    let second_id = two.focused.as_ref().expect("focused").summary.id;
    assert_ne!(second_id, first_id, "a new launch never reuses a session");
    assert_eq!(two.sessions.len(), 2);
    drop(second);

    let mut opened = start("work", InitialSession::Open(first_id));
    let snapshot = first_snapshot(&mut opened).await.unwrap();
    assert_eq!(snapshot.focused.as_ref().unwrap().summary.id, first_id);
    assert_eq!(snapshot.sessions.len(), 2);
    drop(opened);

    let mut attached = start("work", InitialSession::Existing);
    let snapshot = first_snapshot(&mut attached).await.unwrap();
    assert_eq!(snapshot.focused.as_ref().unwrap().summary.id, second_id);
    assert_eq!(snapshot.sessions.len(), 2);
    drop(attached);

    let unknown = qq_protocol::SessionId::from_bytes([9; 16]);
    let mut bad = start("work", InitialSession::Open(unknown));
    let failure = first_snapshot(&mut bad).await.unwrap_err();
    assert!(
        failure.contains(&unknown.to_string()) && failure.contains("does not exist"),
        "{failure}"
    );
    drop(bad);
    let mut foreign = start("elsewhere", InitialSession::Open(first_id));
    let failure = first_snapshot(&mut foreign).await.unwrap_err();
    assert!(
        failure.contains("does not exist in this workspace"),
        "{failure}"
    );
    drop(foreign);

    server.shutdown().await.unwrap();
    handler.shutdown().await.unwrap();
}
