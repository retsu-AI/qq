use super::*;

#[tokio::test]
async fn session_resume_preserves_ask_and_its_unattended_policy_without_an_override() {
    let fixture = fixture(MutatingProvider::new).await;
    let mut initial = options(&fixture.workspace);
    initial.approval = HeadlessApproval::Ask;
    let stored = prepare_session(&fixture.sessions, &initial).await.unwrap();
    let mut resumed = options(&fixture.workspace);
    resumed.session = Some(stored.session_id);
    resumed.approval = HeadlessApproval::Auto;
    let (status, stdout, stderr) = run_to_end(&fixture, resumed, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    assert!(
        !fixture.workspace.join("note.txt").exists(),
        "stored Ask was widened to Auto"
    );
    assert!(!fixture.workspace.join("shelled.txt").exists());
    let records = parse_records(&stdout);
    assert_eq!(
        records[0]["approval"], "ask",
        "trial must name the effective policy"
    );
    let resolutions: Vec<_> = event_records(&records)
        .into_iter()
        .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
        .collect();
    assert_eq!(resolutions.len(), 2, "{stdout}");
    assert!(
        resolutions
            .iter()
            .all(|record| record["envelope"]["event"]["resolution"] == "denied"),
        "{stdout}"
    );
    assert_eq!(
        workspace_snapshot(&fixture).await.sessions[0].approval_mode,
        ApprovalMode::Ask
    );
}

#[tokio::test]
async fn session_resume_permission_selection_follows_an_explicit_override() {
    let fixture = fixture(|| TextProvider).await;
    for stored_mode in [
        HeadlessApproval::ReadOnly,
        HeadlessApproval::Ask,
        HeadlessApproval::Auto,
        HeadlessApproval::Full,
    ] {
        for explicit in [false, true] {
            let mut initial = options(&fixture.workspace);
            initial.approval = stored_mode;
            let stored = prepare_session(&fixture.sessions, &initial).await.unwrap();
            let mut resumed = options(&fixture.workspace);
            resumed.session = Some(stored.session_id);
            resumed.approval = if stored_mode == HeadlessApproval::Auto {
                HeadlessApproval::Ask
            } else {
                HeadlessApproval::Auto
            };
            resumed.approval_explicit = explicit;
            let effective = prepare_session(&fixture.sessions, &resumed).await.unwrap();
            let expected = if explicit {
                resumed.approval
            } else {
                stored_mode
            };
            assert_eq!(effective.approval, expected);
            let snapshot = workspace_snapshot(&fixture).await;
            let session = snapshot
                .sessions
                .iter()
                .find(|session| session.id == stored.session_id)
                .unwrap();
            assert_eq!(session.approval_mode, approval_mode(expected));
        }
    }
}

#[tokio::test]
async fn session_resume_read_only_uses_its_effective_denial_hint() {
    let fixture = fixture(MutatingProvider::new).await;
    let stored = prepare_session(&fixture.sessions, &options(&fixture.workspace))
        .await
        .unwrap();
    let mut resumed = options(&fixture.workspace);
    resumed.session = Some(stored.session_id);
    resumed.approval = HeadlessApproval::Auto;
    resumed.format = HeadlessFormat::Text;
    let (status, _, stderr) = run_to_end(&fixture, resumed, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    assert!(stderr.contains(DENIAL_HINT), "{stderr}");
    assert!(!fixture.workspace.join("note.txt").exists());
}

#[tokio::test]
async fn session_resume_submits_into_the_idle_session_and_applies_the_invocation() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let fixture = {
        let requests = Arc::clone(&requests);
        fixture(move || RecordingTextProvider {
            requests: Arc::clone(&requests),
        })
        .await
    };

    let first = HeadlessOptions {
        prompt: "first task".to_owned(),
        ..options(&fixture.workspace)
    };
    let (status, stdout, _) = run_to_end(&fixture, first, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    let session_id: SessionId = records[0]["session_id"].as_str().unwrap().parse().unwrap();
    let first_run_id = records[0]["run_id"].as_str().unwrap().to_owned();

    let second = HeadlessOptions {
        prompt: "second task".to_owned(),
        session: Some(session_id),
        approval: HeadlessApproval::Auto,
        approval_explicit: true,
        model: ModelSelection {
            model_is_fallback: false,
            model: Some("test/model".to_owned()),
            max_output_tokens: Some(128),
            organization: None,
        },
        ..options(&fixture.workspace)
    };
    let (status, stdout, stderr) = run_to_end(&fixture, second, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    let records = parse_records(&stdout);
    assert_eq!(records[0]["type"], "trial");
    assert_eq!(records[0]["session_id"], session_id.to_string());
    assert_ne!(records[0]["run_id"], first_run_id);
    assert_eq!(records[0]["approval"], "auto");
    let events = event_records(&records);
    assert!(events.iter().all(|record| {
        let kind = &record["envelope"]["event"]["type"];
        kind != "session_created" && kind != "session_updated"
    }));
    assert_eq!(
        events[0]["envelope"]["event"]["type"], "prompt_queued",
        "{:?}",
        events[0]
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| record["envelope"]["event"]["type"] == "prompt_queued")
            .count(),
        1
    );

    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let history = captured[1].messages();
        assert!(history.len() >= 3, "{history:?}");
        assert_eq!(history[0], Message::user("first task"));
        assert_eq!(history[history.len() - 1], Message::user("second task"));
    }

    let workspace = workspace_snapshot(&fixture).await;
    assert_eq!(workspace.sessions.len(), 1, "no second session was created");
    let session = &workspace.sessions[0];
    assert_eq!(session.approval_mode, ApprovalMode::Auto);
    assert_eq!(session.status, SessionStatus::Idle);
}

#[tokio::test]
async fn session_resume_rejects_unknown_foreign_busy_and_child_sessions() {
    let fixture = fixture(|| TextProvider).await;

    let unknown = HeadlessOptions {
        session: Some(SessionId::generate().unwrap()),
        ..options(&fixture.workspace)
    };
    let (status, stdout, stderr) = run_to_end(&fixture, unknown, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::InvalidConfiguration);
    assert!(
        stderr.contains("does not exist in this workspace"),
        "{stderr}"
    );
    assert!(stdout.is_empty(), "no trial record before a session exists");
    assert!(workspace_snapshot(&fixture).await.sessions.is_empty());

    let other_workspace = fixture.workspace.parent().unwrap().join("other");
    std::fs::create_dir_all(&other_workspace).unwrap();
    let (status, stdout, _) = run_to_end(
        &fixture,
        HeadlessOptions {
            workspace: other_workspace.clone(),
            ..options(&other_workspace)
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(status, HeadlessStatus::Completed);
    let foreign: SessionId = parse_records(&stdout)[0]["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (status, _, stderr) = run_to_end(
        &fixture,
        HeadlessOptions {
            session: Some(foreign),
            ..options(&fixture.workspace)
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(status, HeadlessStatus::InvalidConfiguration);
    assert!(
        stderr.contains("does not exist in this workspace"),
        "{stderr}"
    );

    let resolved = send(
        &fixture.sessions,
        SessionCommand::ResolveWorkspace {
            path: fixture.workspace.display().to_string(),
        },
    )
    .await
    .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        panic!("unexpected receipt");
    };
    let created = send(
        &fixture.sessions,
        SessionCommand::CreateSession {
            workspace_id,
            parent_id: None,
            model: options(&fixture.workspace).model,
            approval_mode: ApprovalMode::ReadOnly,
            profile: qq_protocol::AgentProfileId::default(),
            reasoning_effort: None,
            correlation: qq_protocol::Correlation::default(),
        },
    )
    .await
    .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt");
    };
    let child = send(
        &fixture.sessions,
        SessionCommand::CreateSession {
            workspace_id,
            parent_id: Some(session_id),
            model: options(&fixture.workspace).model,
            approval_mode: ApprovalMode::ReadOnly,
            profile: qq_protocol::AgentProfileId::default(),
            reasoning_effort: None,
            correlation: qq_protocol::Correlation::default(),
        },
    )
    .await
    .unwrap();
    let CommandOutcome::SessionCreated {
        session_id: child_id,
    } = child.outcome
    else {
        panic!("unexpected receipt");
    };
    let (status, _, stderr) = run_to_end(
        &fixture,
        HeadlessOptions {
            session: Some(child_id),
            ..options(&fixture.workspace)
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(status, HeadlessStatus::InvalidConfiguration);
    assert!(stderr.contains("sub-agent"), "{stderr}");
}

struct HangsOnceProvider {
    turn: Arc<Mutex<usize>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for HangsOnceProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::pending())
        } else {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "recovered".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}

#[tokio::test]
async fn session_resume_after_an_unclean_exit_recovers_the_interrupted_run_first() {
    let turn = Arc::new(Mutex::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let loader: Arc<dyn RuntimeLoader> = {
        let turn = Arc::clone(&turn);
        let requests = Arc::clone(&requests);
        Arc::new(ProviderLoader(move || HangsOnceProvider {
            turn: Arc::clone(&turn),
            requests: Arc::clone(&requests),
        }))
    };
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let workspace = root.join("work");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let database = root.join("sessions.sqlite3");

    let first = SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader.clone())
        .await
        .unwrap();
    let handle = submit(
        &first,
        &HeadlessOptions {
            prompt: "hang".to_owned(),
            ..options(&workspace)
        },
        vec![qq_protocol::InputPart::text("hang")],
    )
    .await
    .unwrap();
    let mut events = first
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let envelope = events.next().await.unwrap().unwrap();
            if matches!(envelope.event, SessionEvent::RunStarted { .. }) {
                break;
            }
        }
        while requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(events);
    first.abandon_for_test().await.unwrap();
    drop(first);

    let second = SessionRuntime::open(SessionRuntimeOptions::new(database), loader)
        .await
        .unwrap();
    let fixture = Fixture {
        sessions: second,
        workspace: workspace.clone(),
        _directory: directory,
    };
    let (status, stdout, stderr) = run_to_end(
        &fixture,
        HeadlessOptions {
            prompt: "continue".to_owned(),
            session: Some(handle.session_id),
            ..options(&workspace)
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    let records = parse_records(&stdout);
    assert_eq!(records[0]["session_id"], handle.session_id.to_string());
    assert_ne!(records[0]["run_id"], handle.run_id.to_string());

    let snapshot = fixture
        .sessions
        .snapshot(SnapshotRequest::new(
            handle.workspace_id,
            Some(handle.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let runs = &snapshot.focused.unwrap().runs;
    let interrupted = runs.iter().find(|run| run.id == handle.run_id).unwrap();
    assert_eq!(interrupted.status, RunStatus::Interrupted);
    assert_eq!(runs.len(), 2);

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    let history = captured[1].messages();
    assert_eq!(history[0], Message::user("hang"));
    assert!(
        history.iter().any(|message| {
            message.content().iter().any(|block| {
                matches!(
                    block,
                    qq_provider::ContentBlock::Text { text }
                        if text.contains("Do not automatically retry tool calls")
                )
            })
        }),
        "{history:?}"
    );
    assert_eq!(history[history.len() - 1], Message::user("continue"));
}
