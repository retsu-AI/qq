use super::*;

#[tokio::test]
async fn empty_attachment_range_fails_as_invalid_input_and_session_remains_usable() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("empty.txt"), "").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(MutableResolvedLoader {
            resolved_model: Arc::new(StdMutex::new(test_resolved_model(
                "test/model",
                "wire-a",
                64,
                None,
            ))),
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
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
                input: vec![InputPart::WorkspaceFile {
                    path: "empty.txt".to_owned(),
                    expected_hash: None,
                    range: Some(qq_protocol::LineRange { start: 1, end: 1 }),
                }],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let observed = collect_through_finished(&mut events).await;
    let failed_requests = requests.lock().unwrap().len();
    let follow_up = submit_prompt_to(&runtime, session_id, "still usable").await;
    let follow_up_events = collect_through_finished(&mut events).await;
    runtime.shutdown().await.unwrap();

    let failure = observed.iter().find_map(|event| match &event.event {
        SessionEvent::RunFinished {
            outcome: RunOutcome::Failed { failure },
            ..
        } => Some(failure),
        _ => None,
    });
    let failure = failure.expect("nonexistent attachment line must fail the run");
    assert_eq!(failure.kind, RunFailureKind::InvalidCommand);
    assert!(failure.message.contains("file has 0 lines"), "{failure:?}");
    assert_eq!(failed_requests, 0);
    assert!(follow_up_events.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Completed, .. }
            if *run_id == follow_up
    )));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn attachment_ranges_preserve_lines_clip_eof_and_reject_nonexistent_starts() {
    use qq_protocol::LineRange;

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(MutableResolvedLoader {
            resolved_model: Arc::new(StdMutex::new(test_resolved_model(
                "test/model",
                "wire-a",
                64,
                None,
            ))),
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let first_through_eof = Some(LineRange {
        start: 1,
        end: u32::MAX,
    });
    let second_through_eof = Some(LineRange {
        start: 2,
        end: u32::MAX,
    });
    for (content, range, expected) in [
        ("", None, Some(("", "\n"))),
        ("", first_through_eof, None),
        ("a", first_through_eof, Some((" lines=\"1-1/1\"", "a\n"))),
        ("a\n", first_through_eof, Some((" lines=\"1-1/1\"", "a\n"))),
        ("\n", first_through_eof, Some((" lines=\"1-1/1\"", "\n"))),
        (
            "\r\n",
            first_through_eof,
            Some((" lines=\"1-1/1\"", "\r\n")),
        ),
        (
            "a\n\n",
            second_through_eof,
            Some((" lines=\"2-2/2\"", "\n")),
        ),
        (
            "a\r\nb\r\n",
            second_through_eof,
            Some((" lines=\"2-2/2\"", "b\r\n")),
        ),
        (
            "a\rb",
            first_through_eof,
            Some((" lines=\"1-1/1\"", "a\rb\n")),
        ),
        ("a\n", second_through_eof, None),
        (
            "a",
            Some(LineRange {
                start: u32::MAX,
                end: u32::MAX,
            }),
            None,
        ),
    ] {
        std::fs::write(directory.path().join("case.txt"), content).unwrap();
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let before = requests.lock().unwrap().len();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::WorkspaceFile {
                        path: "case.txt".to_owned(),
                        expected_hash: None,
                        range,
                    }],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished(&mut events).await;
        let outcome = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("terminal outcome");
        let captured = requests.lock().unwrap();
        match expected {
            Some((header, body)) => {
                assert_eq!(outcome, &RunOutcome::Completed, "{content:?}, {range:?}");
                assert_eq!(captured.len(), before + 1);
                let message = captured.last().unwrap().messages().last().unwrap();
                let actual = message
                    .content()
                    .iter()
                    .find_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    actual,
                    format!(
                        "\n<attached-file path=\"case.txt\"{header}>\n````\n{body}````\n</attached-file>\n"
                    )
                );
            }
            None => {
                assert!(
                    matches!(outcome, RunOutcome::Failed { failure }
                    if failure.kind == RunFailureKind::InvalidCommand
                        && failure.message.contains("range starts at line")),
                    "{outcome:?}"
                );
                assert_eq!(captured.len(), before);
            }
        }
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn steering_is_applied_at_the_next_boundary_and_replays_in_context() {
    // Two tool turns then a text turn. The approval wait on turn one is
    // the hold point: steering queued there must enter the request for
    // turn two, after turn one's tool result.
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "shell",
        r#"{"command":"true"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let receipt = steer(
        &harness.runtime,
        harness.run_id,
        "also check the tests",
        false,
    )
    .await
    .unwrap();
    let CommandOutcome::SteeringQueued { message_id, .. } = receipt.outcome else {
        panic!("steering must be queued")
    };
    // Replaying the exact command returns the same receipt and queues
    // nothing new.
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::ShellPrefix {
                prefix: "true".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    let observed = collect_through_finished_generously(&mut harness.events).await;
    let queued = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SteeringQueued { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("steering queued event");
    assert_eq!(queued.id, message_id);
    assert!(queued.steering);
    assert_eq!(queued.state, MessageState::Queued);
    assert_eq!(queued.output, "also check the tests");
    let applied = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SteeringApplied {
                message_id: applied,
                turn_ordinal,
                ..
            } if *applied == message_id => Some(*turn_ordinal),
            _ => None,
        })
        .expect("steering applied event");
    assert_eq!(applied, 2);
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SteeringSuperseded { .. })),
    );
    let finished = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(finished, RunOutcome::Completed);

    // The provider saw the steering as a user message after turn one's
    // tool result and before turn two's request; turn three carries it too.
    let requests = harness.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let second = requests[1].messages();
    let position = second
        .iter()
        .position(|message| {
            message.role() == Role::User
                && message
                    .content()
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "also check the tests"))
        })
        .expect("steering must be in the second request");
    assert_eq!(position, second.len() - 1);
    assert!(matches!(
        second[position - 1].content().first(),
        Some(ContentBlock::ToolResult { .. })
    ));
    assert!(requests[2].messages().iter().any(|message| {
        message.content().iter().any(
            |block| matches!(block, ContentBlock::Text { text } if text == "also check the tests"),
        )
    }));

    // Durable context assembly for the next run interleaves it the same
    // way, and the snapshot shows the row complete.
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            32,
        ))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let row = focused
        .messages
        .iter()
        .find(|message| message.id == message_id)
        .unwrap();
    assert!(row.steering);
    assert_eq!(row.state, MessageState::Complete);
    assert_eq!(row.turn_ordinal, 2);
    // The runtime owns the store exclusively; read through it.
    let store = harness.runtime.inner.store.clone();
    let session_id = harness.session_id;
    let context = store
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction().unwrap();
            load_model_context(&transaction, session_id, u64::MAX)
        })
        .await
        .unwrap();
    let steering_index = context
        .iter()
        .position(|message| {
            message.role() == Role::User
                && message
                    .content()
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "also check the tests"))
        })
        .expect("steering replays in durable context");
    assert!(matches!(
        context[steering_index - 1].content().first(),
        Some(ContentBlock::ToolResult { .. })
    ));
    assert_eq!(context[steering_index + 1].role(), Role::Assistant);
    harness.runtime.shutdown().await.unwrap();
    drop(store);
    assert_assembly_matches_reference(
        &harness._directory.path().join("sessions.sqlite3"),
        harness.session_id,
    );
}

#[tokio::test]
async fn interrupting_steer_withdraws_the_pending_approval_and_continues() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "shell",
        r#"{"command":"true"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let receipt = steer(
        &harness.runtime,
        harness.run_id,
        "stop, do it differently",
        true,
    )
    .await
    .unwrap();
    let CommandOutcome::SteeringQueued { message_id, .. } = receipt.outcome else {
        panic!("steering must be queued")
    };
    let observed = collect_through_finished_generously(&mut harness.events).await;
    let interrupted = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunInterrupted { turn_ordinal, .. } => Some(*turn_ordinal),
            _ => None,
        })
        .expect("interrupt event");
    assert_eq!(interrupted, 1);
    let settled = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call: call } if call.id == tool_call.id => {
                Some(call.clone())
            }
            _ => None,
        })
        .expect("the pending call settles");
    assert_eq!(settled.state, ToolCallState::Interrupted);
    assert!(settled.is_error);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SteeringApplied { message_id: applied, turn_ordinal: 2, .. }
            if *applied == message_id
    )));
    let finished = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(finished, RunOutcome::Completed);
    // Answering the withdrawn approval afterwards is refused: it is no
    // longer pending.
    let late = respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(
        matches!(
            late,
            Err(SessionRuntimeError::ApprovalNotPending) | Err(SessionRuntimeError::RunNotFound)
        ),
        "{late:?}"
    );
    let requests = harness.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let second = requests[1].messages();
    // Assistant call, interrupted tool result, then the steering.
    let last = second.last().unwrap();
    assert_eq!(last.role(), Role::User);
    assert!(second[second.len() - 2]
        .content()
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { content, is_error: true, .. } if content == INTERRUPTED_TOOL_RESULT)));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn steering_bounds_and_refusals_are_typed() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "shell",
        r#"{"command":"true"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    for index in 0..crate::runtime::MAX_PENDING_STEERING {
        steer(
            &harness.runtime,
            harness.run_id,
            &format!("note {index}"),
            false,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        steer(&harness.runtime, harness.run_id, "one too many", false).await,
        Err(SessionRuntimeError::SteeringQueueFull)
    );
    assert_eq!(
        steer(&harness.runtime, harness.run_id, "   ", false).await,
        Err(SessionRuntimeError::EmptyPrompt)
    );
    assert_eq!(
        steer(&harness.runtime, RunId::from_bytes([9; 16]), "x", false).await,
        Err(SessionRuntimeError::RunNotFound)
    );
    // A queued (not yet running) run cannot be steered.
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("follow-up")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: queued_run, ..
    } = queued.outcome
    else {
        panic!("queued")
    };
    assert_eq!(
        steer(&harness.runtime, queued_run, "too early", false).await,
        Err(SessionRuntimeError::RunNotSteerable)
    );
    // Cancelling the run supersedes everything still queued.
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun {
                run_id: harness.run_id,
            },
        )
        .await
        .unwrap();
    let _ = tool_call;
    let observed = collect_through_finished_generously(&mut harness.events).await;
    let superseded = observed
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::SteeringSuperseded { .. }))
        .count();
    assert_eq!(
        superseded,
        usize::from(crate::runtime::MAX_PENDING_STEERING)
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { outcome: RunOutcome::Cancelled, run_id, .. } if *run_id == harness.run_id
    )));
    // Steering a finished run reports the terminal outcome, idempotently.
    let done = steer(&harness.runtime, harness.run_id, "late", false)
        .await
        .unwrap();
    assert!(matches!(
        done.outcome,
        CommandOutcome::RunAlreadyFinished {
            outcome: RunOutcome::Cancelled,
            ..
        }
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn plan_identity_correlation_and_profile_persist_and_survive_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let initial = test_resolved_model("test/model", "wire-a", 64, None);
    let configured = Arc::new(StdMutex::new(initial));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(MutableResolvedLoader {
        resolved_model: Arc::clone(&configured),
        requests,
    });
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        loader.clone(),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let correlation = Correlation::new(
        [("thread".to_owned(), "t-42".to_owned())]
            .into_iter()
            .collect(),
    )
    .unwrap();
    let created = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(64),
                    organization: None,
                },
                approval_mode: ApprovalMode::Auto,
                profile: AgentProfileId::new("review").unwrap(),
                correlation: correlation.clone(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_correlation =
        Correlation::new([("job".to_owned(), "j-1".to_owned())].into_iter().collect()).unwrap();
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("work")],
                limits: RunLimits::default(),
                correlation: run_correlation.clone(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let observed = collect_through_finished(&mut events).await;
    let started_plan = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted { plan, .. } => plan.clone(),
            _ => None,
        })
        .expect("run started carries plan identity");
    assert_eq!(started_plan.profile.as_str(), "review");
    assert_eq!(
        started_plan.descriptor_version,
        crate::plan::DESCRIPTOR_VERSION
    );
    // The loader received the session's profile.
    let requests_seen = loader.requests.lock().unwrap().len();
    assert_eq!(requests_seen, 1);

    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions[0].profile.as_str(), "review");
    assert_eq!(snapshot.sessions[0].correlation, correlation);
    let run = snapshot
        .focused
        .as_ref()
        .unwrap()
        .runs
        .iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert_eq!(run.plan.as_deref(), Some(&*started_plan));
    assert_eq!(run.correlation, run_correlation);

    // Refresh the configuration: a later run compiles a new plan with a
    // new digest, and the earlier run's identity is untouched.
    configured.lock().unwrap().provider_model = "wire-b".to_owned();
    let second = submit_prompt_to(&runtime, session_id, "again").await;
    let observed = collect_through_finished(&mut events).await;
    let second_plan = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunStarted {
                plan, run_id: id, ..
            } if *id == second => plan.clone(),
            _ => None,
        })
        .unwrap();
    assert_ne!(second_plan.digest, started_plan.digest);
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);

    let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database_path), loader)
        .await
        .unwrap();
    let snapshot = reopened
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let runs = &snapshot.focused.as_ref().unwrap().runs;
    let first = runs.iter().find(|run| run.id == run_id).unwrap();
    assert_eq!(first.plan.as_deref(), Some(&*started_plan));
    let stored_descriptor: String = reopened
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT plan_descriptor_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    let descriptor: crate::plan::AgentPlanDescriptor =
        serde_json::from_str(&stored_descriptor).unwrap();
    assert_eq!(descriptor.digest().unwrap(), started_plan.digest);
    assert_eq!(descriptor.model.provider_model, "wire-a");
    reopened.shutdown().await.unwrap();
}

/// F05 regression: the bytes a prompt attached are what every later request
/// replays, whatever happened to the file since. Also covers dedup of a
/// re-attached file, reopen, the reference-assembly oracle, and the delete
/// cascade.
#[tokio::test]
async fn attached_files_are_reconstructed_as_the_model_first_saw_them() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "ALPHA_ORIGINAL\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "line1\nline2\nline3\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(MutableResolvedLoader {
        resolved_model: Arc::new(StdMutex::new(test_resolved_model(
            "test/model",
            "wire-a",
            64,
            None,
        ))),
        requests: Arc::clone(&requests),
    });
    let database = directory.path().join("sessions.sqlite3");
    let runtime =
        SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader.clone())
            .await
            .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let attach = |text: &str, files: Vec<InputPart>| {
        let mut input = vec![InputPart::text(text)];
        input.extend(files);
        SessionCommand::SubmitPrompt {
            session_id,
            input,
            limits: RunLimits::default(),
            correlation: Correlation::default(),
            output: None,
        }
    };
    let file = |path: &str, range: Option<(u32, u32)>| InputPart::WorkspaceFile {
        path: path.to_owned(),
        expected_hash: None,
        range: range.map(|(start, end)| qq_protocol::LineRange { start, end }),
    };
    let prompt_text = |request: &ModelRequest, index: usize| -> String {
        let user_messages: Vec<&Message> = request
            .messages()
            .iter()
            .filter(|message| message.role() == Role::User)
            .collect();
        user_messages[index]
            .content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap()
    };

    // Turn 1: whole file plus a range of another.
    runtime
        .command(
            CommandId::generate().unwrap(),
            attach(
                "inspect",
                vec![file("a.txt", None), file("b.txt", Some((2, 2)))],
            ),
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    let first_prompt = prompt_text(&requests.lock().unwrap()[0], 0);
    assert!(first_prompt.contains("ALPHA_ORIGINAL"), "{first_prompt}");
    assert!(
        first_prompt.contains("<attached-file path=\"b.txt\" lines=\"2-2/3\">\n````\nline2\n````"),
        "{first_prompt}"
    );

    // The files change underneath the session before the follow-up.
    std::fs::write(directory.path().join("a.txt"), "ALPHA_MODIFIED\n").unwrap();
    std::fs::remove_file(directory.path().join("b.txt")).unwrap();

    // Turn 2: a plain follow-up sees turn 1 exactly as it was sent.
    runtime
        .command(
            CommandId::generate().unwrap(),
            attach("continue", Vec::new()),
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(prompt_text(&captured[1], 0), first_prompt);
        assert!(!prompt_text(&captured[1], 0).contains("ALPHA_MODIFIED"));
        assert!(!prompt_text(&captured[1], 0).contains("@a.txt"));
        assert_eq!(prompt_text(&captured[1], 1), "continue");
    }

    // Turn 3: attaching the modified file stores a second blob (different
    // digest) while turn 1 still renders the original.
    runtime
        .command(
            CommandId::generate().unwrap(),
            attach("again", vec![file("a.txt", None)]),
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    // Turn 4: re-attaching the same bytes dedups against turn 3's blob.
    runtime
        .command(
            CommandId::generate().unwrap(),
            attach("once more", vec![file("a.txt", None)]),
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 4);
        let last = &captured[3];
        assert_eq!(prompt_text(last, 0), first_prompt);
        assert!(prompt_text(last, 2).contains("ALPHA_MODIFIED"));
        assert!(prompt_text(last, 3).contains("ALPHA_MODIFIED"));
    }
    let (blobs, references, evicted): (u32, u32, u32) = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok((
                connection.query_row(
                    "SELECT COUNT(*) FROM attachment_blobs WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )?,
                connection.query_row(
                    "SELECT COUNT(*) FROM message_attachments WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )?,
                connection.query_row(
                    "SELECT COUNT(*) FROM attachment_blobs WHERE evicted_at_ms IS NOT NULL",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        (blobs, references, evicted),
        (3, 4, 0),
        "a.txt original, b.txt range, a.txt modified; four prompt references"
    );
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);

    // Reopen: a fresh runtime assembles the same context from the store.
    let reopened =
        SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader.clone())
            .await
            .unwrap();
    let queued = reopened
        .command(
            CommandId::generate().unwrap(),
            attach("after reopen", Vec::new()),
        )
        .await
        .unwrap();
    let mut events = reopened
        .subscribe(SubscribeRequest {
            workspace_id,
            after: queued.committed_through,
        })
        .unwrap();
    collect_through_finished(&mut events).await;
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 5);
        assert_eq!(prompt_text(&captured[4], 0), first_prompt);
    }
    reopened.shutdown().await.unwrap();
    drop(events);
    drop(reopened);
    // The joined loader and the per-message reference agree on the
    // reconstructed attachments.
    assert_assembly_matches_reference(&database, session_id);

    // Deleting the session removes its attachments with the rest of its rows.
    let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader)
        .await
        .unwrap();
    reopened
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession { session_id },
        )
        .await
        .unwrap();
    reopened.shutdown().await.unwrap();
    drop(reopened);
    let connection = Connection::open(&database).unwrap();
    for table in ["attachment_blobs", "message_attachments"] {
        let count: u32 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} must be empty after the delete");
    }
}

/// Past the per-session attachment cap the oldest blob loses its bytes; the
/// prompt that carried it says so explicitly instead of reverting to `@path`.
#[tokio::test]
async fn evicted_attachments_render_an_explicit_stub_not_the_placeholder() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "ALPHA\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(MutableResolvedLoader {
            resolved_model: Arc::new(StdMutex::new(test_resolved_model(
                "test/model",
                "wire-a",
                64,
                None,
            ))),
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
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
                input: vec![
                    InputPart::text("inspect"),
                    InputPart::WorkspaceFile {
                        path: "a.txt".to_owned(),
                        expected_hash: None,
                        range: None,
                    },
                ],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    // Simulate the cap reclaiming the blob: the row stays, the bytes go.
    runtime
        .inner
        .store
        .call_write(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE attachment_blobs SET content = NULL, evicted_at_ms = 1
                 WHERE session_id = ?1",
                [session_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    collect_through_finished(&mut events).await;
    let first = {
        let captured = requests.lock().unwrap();
        captured[1]
            .messages()
            .iter()
            .find(|message| message.role() == Role::User)
            .unwrap()
            .content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        first,
        "inspect\n\n<attached-file path=\"a.txt\" evicted=\"true\">\n\
         [the attached content was evicted from session storage; \
         read the file again to see its current state]\n</attached-file>\n"
    );
    assert!(!first.contains("ALPHA"));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn workspace_file_parts_attach_at_start_and_stale_hashes_fail_before_provider_work() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.md"), "remember the tests\n").unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(MutableResolvedLoader {
        resolved_model: Arc::new(StdMutex::new(test_resolved_model(
            "test/model",
            "wire-a",
            64,
            None,
        ))),
        requests: Arc::clone(&requests),
    });
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        loader,
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![
                    InputPart::text("Summarize"),
                    InputPart::WorkspaceFile {
                        path: "notes.md".to_owned(),
                        expected_hash: None,
                        range: None,
                    },
                ],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let observed = collect_through_finished(&mut events).await;
    let queued_message = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    // The transcript row carries the placeholder, never file bytes.
    assert_eq!(queued_message.output, "Summarize\n@notes.md");
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { outcome: RunOutcome::Completed, run_id: id, .. } if *id == run_id
    )));
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let prompt = captured[0].messages().last().unwrap();
        let text = prompt
            .content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert!(text.starts_with("Summarize\n\n<attached-file path=\"notes.md\">"));
        assert!(text.contains("remember the tests"));
    }
    // The attachment recorded the file so an edit is not "unread".
    let files: Vec<(String, String)> = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            let mut statement = connection
                .prepare("SELECT path, content_hash FROM session_files WHERE session_id = ?1")?;
            let rows = statement
                .query_map([session_id.to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(
        files.len(),
        0,
        "attachment hashes stay in the run's live file state; \
        they are not durable session rows until a tool records them"
    );

    // A stale hash fails the run with a typed outcome and no provider call.
    let stale = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::WorkspaceFile {
                    path: "notes.md".to_owned(),
                    expected_hash: Some(ContentHash::from_bytes([7; 32])),
                    range: None,
                }],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued {
        run_id: stale_run, ..
    } = stale.outcome
    else {
        panic!("unexpected receipt")
    };
    let observed = collect_through_finished(&mut events).await;
    let outcome = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished {
                outcome, run_id, ..
            } if *run_id == stale_run => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    let RunOutcome::Failed { failure } = outcome else {
        panic!("stale attachment must fail the run: {outcome:?}")
    };
    assert_eq!(failure.kind, RunFailureKind::InvalidCommand);
    assert!(failure.message.contains("changed"), "{}", failure.message);
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "no provider request for the failed run"
    );

    // Malformed parts never reach durable admission.
    let escaped = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::WorkspaceFile {
                    path: "/etc/passwd".to_owned(),
                    expected_hash: None,
                    range: None,
                }],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await;
    assert!(matches!(
        escaped,
        Err(SessionRuntimeError::InvalidInput(
            qq_protocol::InputError::AbsolutePath { .. }
        ))
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_limits_are_validated_against_the_runtime_ceilings() {
    let (_directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, _directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    for limits in [
        RunLimits {
            max_children: Some(MAX_SPAWNED_CHILDREN_PER_RUN + 1),
            ..RunLimits::default()
        },
        RunLimits {
            max_concurrent_children: Some(MAX_CONCURRENT_CHILDREN_PER_RUN + 1),
            ..RunLimits::default()
        },
        RunLimits {
            max_tool_output_bytes: Some(0),
            ..RunLimits::default()
        },
    ] {
        let result = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::text("x")],
                    limits,
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await;
        assert_eq!(result, Err(SessionRuntimeError::InvalidRunLimits));
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_snapshot_preserves_prompt_identity_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "Keep provenance stable.\n",
    )
    .unwrap();
    let skill = directory.path().join(".qq/skills/stable");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "Retain exact run provenance.\n").unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = submit_prompt_to(&runtime, session_id, "/stable finish the work").await;
    collect_through_finished(&mut events).await;

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap();
    let run = snapshot
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    let prompt_identity = run
        .prompt_identity
        .expect("a sent prompt must retain its prompt identity");
    assert_eq!(prompt_identity.version, crate::AGENT_PROMPT_VERSION);
    assert_eq!(prompt_identity.instruction_hash.to_string().len(), 64);
    assert_eq!(
        prompt_identity
            .system_prompt_hash
            .expect("new runs must retain their full prompt hash")
            .to_string()
            .len(),
        64
    );
    assert_eq!(
        prompt_identity
            .tool_schema_hash
            .expect("new runs must retain their tool schema hash")
            .to_string()
            .len(),
        64
    );
    let guidance = prompt_identity
        .selected_guidance
        .as_deref()
        .expect("the selected skill must retain provenance");
    assert_eq!(guidance.kind, qq_protocol::GuidanceKind::Skill);
    assert_eq!(guidance.name, "stable");
    assert_eq!(guidance.source, ".qq/skills/stable/SKILL.md");
    assert_eq!(guidance.version, None);

    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);
    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let snapshot = reopened
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap();
    let run = snapshot
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert_eq!(run.prompt_identity, Some(prompt_identity));
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn resolved_model_and_request_limits_survive_config_mutation_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let mut initial = test_resolved_model(
        "test/model",
        "wire-model-a",
        64,
        Some(ModelPricing {
            input_usd_nanos_per_token: 1,
            output_usd_nanos_per_token: 2,
            cache_read_usd_nanos_per_token: Some(1),
            cache_write_usd_nanos_per_token: None,
            context_tier: None,
            provenance: "catalog-a".to_owned(),
        }),
    );
    initial.organization = Some("org-a".to_owned());
    initial.credential_profile = Some("profile-a".to_owned());
    initial.context_window = Some(32_768);
    initial.prompt_cache.cache_read_usage = true;
    let configured = Arc::new(StdMutex::new(initial.clone()));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let loader = Arc::new(MutableResolvedLoader {
        resolved_model: Arc::clone(&configured),
        requests: Arc::clone(&requests),
    });
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        loader.clone(),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let run_id = submit_prompt_to(&runtime, session_id, "work").await;
    let observed = collect_through_finished(&mut events).await;

    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ModelTurnCompleted { model, .. }
            if model.model.as_deref() == Some("test/model")
                && model.max_output_tokens == Some(64)
                && model.organization.as_deref() == Some("org-a")
    )));
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].model(), initial.provider_model);
        assert_eq!(captured[0].max_output_tokens(), initial.max_output_tokens);
    }

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    let persisted = snapshot
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap()
        .resolved_model
        .map(|model| *model)
        .unwrap();
    assert_eq!(persisted, initial);

    *configured.lock().unwrap() = test_resolved_model("test/changed", "wire-model-b", 32, None);
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);

    let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database_path), loader)
        .await
        .unwrap();
    let snapshot = reopened
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    let persisted_after_restart = snapshot
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap()
        .resolved_model
        .map(|model| *model)
        .unwrap();
    assert_eq!(persisted_after_restart, initial);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn escaped_slash_is_persisted_as_the_provider_visible_prompt() {
    let mut harness = scripted_runs_harness(ApprovalMode::Ask, vec![Vec::new(), Vec::new()]).await;

    submit_prompt(&harness, "//review literally").await;
    let first_events = collect_through_finished(&mut harness.events).await;
    assert!(first_events.iter().any(|event| matches!(
        &event.event,
        SessionEvent::PromptQueued { message, .. }
            if message.output == "/review literally"
    )));
    submit_prompt(&harness, "follow up").await;
    collect_through_finished(&mut harness.events).await;

    {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(
            requests[0].messages()[0],
            Message::user("/review literally")
        );
        assert_eq!(
            requests[1].messages()[0],
            Message::user("/review literally")
        );
        assert!(
            requests[1]
                .messages()
                .iter()
                .all(|message| message != &Message::user("//review literally"))
        );
    }

    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap()
        .focused
        .unwrap();
    let first_user = snapshot
        .messages
        .iter()
        .find(|message| message.role == qq_protocol::MessageRole::User)
        .unwrap();
    assert_eq!(first_user.output, "/review literally");
}

#[tokio::test]
async fn prompt_identity_persistence_failure_starts_no_provider_work() {
    struct CountingLoader {
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for CountingLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                let runtime = Runtime::new(CountingProvider { provider_calls }, "test-model", 256)
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })?;
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }

    struct CountingProvider {
        provider_calls: Arc<AtomicUsize>,
    }

    impl Provider for CountingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            self.provider_calls.fetch_add(1, Ordering::AcqRel);
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: None,
            })]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        Arc::new(CountingLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    Connection::open(&database_path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_prompt_identity
             BEFORE UPDATE OF prompt_identity_json ON runs
             WHEN NEW.prompt_identity_json IS NOT NULL
             BEGIN
                 SELECT RAISE(FAIL, 'forced prompt identity failure');
             END;",
        )
        .unwrap();

    let run_id = submit_prompt_to(&runtime, session_id, "work").await;
    let observed = collect_through_finished(&mut events).await;

    assert_eq!(provider_calls.load(Ordering::Acquire), 0);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Failed { failure },
            ..
        } if *finished == run_id
            && failure.kind == RunFailureKind::Server
            && failure.message.contains("failed to persist prepared run state")
    )));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resolved_model_persistence_failure_starts_no_provider_work() {
    struct CountingLoader {
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for CountingLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                Runtime::new(CountingProvider { provider_calls }, "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct CountingProvider {
        provider_calls: Arc<AtomicUsize>,
    }

    impl Provider for CountingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            self.provider_calls.fetch_add(1, Ordering::AcqRel);
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: None,
            })]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        Arc::new(CountingLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    Connection::open(&database_path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_resolved_model
             BEFORE UPDATE OF resolved_model_json ON runs
             WHEN NEW.resolved_model_json IS NOT NULL
             BEGIN
                 SELECT RAISE(FAIL, 'forced resolved model failure');
             END;",
        )
        .unwrap();

    let run_id = submit_prompt_to(&runtime, session_id, "work").await;
    let observed = collect_through_finished(&mut events).await;

    assert_eq!(provider_calls.load(Ordering::Acquire), 0);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Failed { failure },
            ..
        } if *finished == run_id
            && failure.kind == RunFailureKind::Server
            && failure.message.contains("failed to persist prepared run state")
    )));
    let persisted: Option<String> = Connection::open(&database_path)
        .unwrap()
        .query_row(
            "SELECT resolved_model_json FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, None);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn known_first_turn_context_overflow_starts_no_provider_work() {
    struct TinyContextLoader {
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for TinyContextLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                struct PricedTinyContextProvider(Arc<AtomicUsize>);

                impl Provider for PricedTinyContextProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                            usage: None,
                        })]))
                    }
                }

                Runtime::new(PricedTinyContextProvider(provider_calls), "test-model", 1)
                    .map(|runtime| runtime.with_context_window(Some(1)))
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(TinyContextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    submit_prompt_to(&runtime, session_id, "work").await;
    let observed = collect_through_finished(&mut events).await;

    assert_eq!(provider_calls.load(Ordering::Acquire), 0);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Failed { failure },
            ..
        } if failure.kind == RunFailureKind::Policy
    )));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn saturated_prepared_rejection_settles_and_keeps_priced_accounting_known_zero() {
    struct PricedTinyContextLoader {
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for PricedTinyContextLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                struct PricedTinyContextProvider(Arc<AtomicUsize>);

                impl Provider for PricedTinyContextProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                            usage: None,
                        })]))
                    }
                }

                Runtime::new(PricedTinyContextProvider(provider_calls), "test-model", 1)
                    .map(|runtime| runtime.with_context_window(Some(1)))
                    .map(|runtime| {
                        loaded_runtime(
                            runtime,
                            &request.workspace,
                            Some(ModelPricing {
                                input_usd_nanos_per_token: 1_000,
                                output_usd_nanos_per_token: 2_000,
                                cache_read_usd_nanos_per_token: Some(100),
                                cache_write_usd_nanos_per_token: Some(300),
                                context_tier: None,
                                provenance: "priced-test".to_owned(),
                            }),
                        )
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(PricedTinyContextLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let queued = runtime
        .inner
        .store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("known overflow".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
        panic!("unexpected receipt")
    };
    // Saturate the control lane before the supervisor settles the policy
    // rejection: settlement waits for capacity instead of spinning.
    let saturated = saturate_control_lane(&runtime).await;
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    runtime.request_schedule();
    tokio::time::sleep(Duration::from_millis(50)).await;
    saturated.release().await;
    let observed = collect_until(&mut events, finished_for(run_id)).await;

    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(observed.iter().all(|event| !matches!(
        event.event,
        SessionEvent::RunStarted { run_id: started, .. } if started == run_id
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id: finished,
            outcome: RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    ..
                }
            },
            ..
        } if *finished == run_id
    )));
    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 2,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert_eq!(
        focused.summary.accounting.unwrap().direct,
        AccountingTotal {
            usage: Some(usage(0, 0)),
            estimated_cost_usd_nanos: Some(0),
        }
    );
    assert!(focused.runs[0].resolved_model.is_some());
    assert!(!*runtime.inner.failed.borrow());
}

#[tokio::test]
async fn known_later_turn_context_overflow_starts_no_second_provider_request() {
    struct LaterTurnLoader {
        provider_calls: Arc<AtomicUsize>,
    }

    impl RuntimeLoader for LaterTurnLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider_calls = Arc::clone(&self.provider_calls);
            Box::pin(async move {
                Runtime::new(LaterTurnProvider { provider_calls }, "test-model", 1)
                    .map(|runtime| runtime.with_context_window(Some(100_000)))
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct LaterTurnProvider {
        provider_calls: Arc<AtomicUsize>,
    }

    impl Provider for LaterTurnProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let call = self.provider_calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                Box::pin(stream::iter(vec![
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "list".to_owned(),
                        name: "tree".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "list".to_owned(),
                        json: r#"{"path":".","depth":1}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "list".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 99_998,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 1,
                            reasoning_tokens: None,
                        }),
                    }),
                ]))
            } else {
                Box::pin(stream::iter(vec![Ok(
                    qq_provider::ProviderEvent::Completed { usage: None },
                )]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(LaterTurnLoader {
            provider_calls: Arc::clone(&provider_calls),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    submit_prompt_to(&runtime, session_id, "work").await;
    let observed = collect_through_finished(&mut events).await;

    assert_eq!(provider_calls.load(Ordering::Acquire), 1);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Failed { failure },
            ..
        } if failure.kind == RunFailureKind::Policy
    )));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn instruction_hash_tracks_selected_path_and_bytes_deterministically() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("AGENTS.md"), "same\n").unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();

    let first =
        completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "first").await;
    let second =
        completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "second").await;
    assert_eq!(
        first,
        "6aba264a3fed8588d4e09f84ce073452fb551b53e8c3beae1b4aaf6bbb55a0c4"
    );
    assert_eq!(second, first);

    std::fs::remove_file(directory.path().join("AGENTS.md")).unwrap();
    std::fs::write(directory.path().join("CLAUDE.md"), "same\n").unwrap();
    let fallback =
        completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "fallback")
            .await;
    assert_eq!(
        fallback,
        "6d0da1256387dfa8d1521b942048efc90d6fa610b0d8f3e9d9dc7d0e4733d73e"
    );
    assert_ne!(fallback, first);

    std::fs::remove_file(directory.path().join("CLAUDE.md")).unwrap();
    let empty =
        completed_instruction_hash(&runtime, workspace_id, session_id, &mut events, "empty").await;
    assert_eq!(
        empty,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_session_model_applies_to_the_next_run_but_not_the_active_one() {
    let mut harness = session_management_harness().await;
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("first run".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    // The run is now claimed and parked at its tool approval.
    let (before_switch, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    assert!(before_switch.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunContextUpdated {
            context_tokens: 7,
            ..
        }
    )));
    assert!(before_switch.iter().any(|event| matches!(
        event.event,
        SessionEvent::SessionContextUpdated {
            context_tokens: Some(7),
            ..
        }
    )));

    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetSessionModel {
                session_id: harness.session_id,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model-b".to_owned()),
                    max_output_tokens: Some(512),
                    organization: None,
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        &receipt.outcome,
        CommandOutcome::SessionModelSet { session_id, model }
            if *session_id == harness.session_id
                && model.model.as_deref() == Some("test/model-b")
    ));
    let connection = Connection::open(harness.directory.path().join("sessions.sqlite3")).unwrap();
    let stored_basis: Option<String> = connection
        .query_row(
            "SELECT context_occupancy_json FROM sessions WHERE id = ?1",
            [harness.session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_basis, None);
    drop(connection);
    let updated = harness.events.next().await.unwrap().unwrap();
    assert!(matches!(
        &updated.event,
        SessionEvent::SessionUpdated { session }
            if session.model.as_deref() == Some("test/model-b")
                && session.active_run_id == Some(run_id)
                && session.context_tokens.is_none()
    ));

    respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    let finished_old_model = collect_through_finished(&mut harness.events).await;
    assert!(
        finished_old_model
            .iter()
            .any(|event| matches!(event.event, SessionEvent::RunContextUpdated { .. }))
    );
    assert!(
        finished_old_model
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::SessionContextUpdated { .. }))
    );
    assert!(finished_old_model.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { session, .. } if session.context_tokens.is_none()
    )));

    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("second run".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    collect_through_finished(&mut harness.events).await;

    // The active run kept its claimed model; only the next run loads the
    // repointed one.
    assert_eq!(
        *harness.models.lock().unwrap(),
        vec![
            Some("test/model".to_owned()),
            Some("test/model-b".to_owned())
        ]
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    assert_eq!(snapshot.focused.unwrap().summary.context_tokens, Some(7));

    // The same validation as CreateSession applies.
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetSessionModel {
                    session_id: harness.session_id,
                    model: ModelSelection::default(),
                },
            )
            .await
            .unwrap_err(),
        SessionRuntimeError::InvalidModelSelection
    );
}

#[tokio::test]
async fn active_run_cannot_restore_occupancy_after_same_route_shape_change() {
    let mut harness = session_management_harness().await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetSessionModel {
                session_id: harness.session_id,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: None,
                    organization: None,
                },
            },
        )
        .await
        .unwrap();
    let updated = harness.events.next().await.unwrap().unwrap();
    assert!(matches!(updated.event, SessionEvent::SessionUpdated { .. }));
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("first run".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let connection = Connection::open(harness.directory.path().join("sessions.sqlite3")).unwrap();
    let stored_basis: Option<String> = connection
        .query_row(
            "SELECT context_occupancy_json FROM sessions WHERE id = ?1",
            [harness.session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stored_basis.is_some());
    drop(connection);

    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetSessionModel {
                session_id: harness.session_id,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(512),
                    organization: Some("changed-organization".to_owned()),
                },
            },
        )
        .await
        .unwrap();
    let updated = harness.events.next().await.unwrap().unwrap();
    assert!(matches!(updated.event, SessionEvent::SessionUpdated { .. }));

    respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    let finished = collect_through_finished(&mut harness.events).await;
    assert!(
        finished
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::SessionContextUpdated { .. }))
    );

    let connection = Connection::open(harness.directory.path().join("sessions.sqlite3")).unwrap();
    let stored_basis: Option<String> = connection
        .query_row(
            "SELECT context_occupancy_json FROM sessions WHERE id = ?1",
            [harness.session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_basis, None);
}

#[tokio::test]
async fn delete_session_is_refused_while_running_then_cascades_completely() {
    let mut harness = session_management_harness().await;
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("do work".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;

    // Refused while the run is active; the client cancels first.
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession {
                    session_id: harness.session_id,
                },
            )
            .await
            .unwrap_err(),
        SessionRuntimeError::SessionActive
    );

    // Approving for the session also writes a grant row to cascade over.
    respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    collect_through_finished(&mut harness.events).await;

    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::DeleteSession {
        session_id: harness.session_id,
    };
    let receipt = harness
        .runtime
        .command(command_id, command.clone())
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::SessionDeleted { session_id } if session_id == harness.session_id
    ));
    // Idempotent: the retry returns the original durable receipt.
    assert_eq!(
        harness.runtime.command(command_id, command).await.unwrap(),
        receipt
    );

    // Every session-owned row is gone in one transaction; the event log
    // keeps its rows so replays stay gapless.
    let connection = Connection::open(harness.directory.path().join("sessions.sqlite3")).unwrap();
    for table in [
        "sessions",
        "runs",
        "messages",
        "message_chunks",
        "tool_calls",
        "model_turns",
        "session_grants",
        "session_files",
        "session_compactions",
        "tool_spills",
    ] {
        let count: u32 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} must be empty after the delete");
    }
    let events: u32 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert!(events > 0, "the workspace event log is append-only");
    drop(connection);

    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: None,
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 8,
        })
        .await
        .unwrap();
    assert!(snapshot.sessions.is_empty());

    // A replay across the deletion stays contiguous and ends deleted.
    let mut replay = harness
        .runtime
        .subscribe(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after: EventCursor {
                store_id: harness.store_id,
                workspace_id: harness.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    let mut expected_sequence = 0;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), replay.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        expected_sequence += 1;
        assert_eq!(event.cursor.sequence, expected_sequence);
        if matches!(event.event, SessionEvent::SessionDeleted { session_id }
            if session_id == harness.session_id)
        {
            break;
        }
    }

    // Deleting again with a fresh command is an ordinary not-found.
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession {
                    session_id: harness.session_id,
                },
            )
            .await
            .unwrap_err(),
        SessionRuntimeError::SessionNotFound
    );
}

/// F07: a store at `MAX_COMMANDS` refuses new work but still admits the
/// commands that stop, resolve, or remove what it holds; otherwise a full
/// store could neither be cancelled nor cleaned up to make room.
#[tokio::test]
async fn control_and_cleanup_commands_are_admitted_past_the_command_limit() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    // A receipt recorded before the cap, for the replay check below.
    let pre_cap_id = CommandId::generate().unwrap();
    let pre_cap = harness
        .runtime
        .command(
            pre_cap_id,
            SessionCommand::SetApprovalMode {
                session_id: harness.session_id,
                mode: ApprovalMode::Ask,
            },
        )
        .await
        .unwrap();
    // Fill the counter, not the table: the bound reads `metadata.command_count`.
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE metadata SET value = ?1 WHERE key = 'command_count'",
                [MAX_COMMANDS.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    // New work is refused ...
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("more")],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await;
    assert!(
        matches!(refused, Err(SessionRuntimeError::CommandLimitReached)),
        "{refused:?}"
    );
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: harness.session_id,
                mode: ApprovalMode::Auto,
            },
        )
        .await;
    assert!(matches!(
        refused,
        Err(SessionRuntimeError::CommandLimitReached)
    ));
    // ... a pre-cap receipt still replays exactly ...
    let replayed = harness
        .runtime
        .command(
            pre_cap_id,
            SessionCommand::SetApprovalMode {
                session_id: harness.session_id,
                mode: ApprovalMode::Ask,
            },
        )
        .await
        .unwrap();
    assert_eq!(replayed, pre_cap);
    // ... and control still lands: the held approval resolves and the run
    // completes.
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    // Cleanup lands too: the idle session can be deleted and the workspace
    // pruned, then the runtime shuts down.
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: harness.session_id,
            },
        )
        .await
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::PruneSessions {
                workspace_id: harness.workspace_id,
            },
        )
        .await
        .unwrap();
    harness.runtime.shutdown().await.unwrap();
}

/// The control lane has its own ceiling: past the headroom it is refused too,
/// so the receipt table stays bounded.
#[tokio::test]
async fn control_commands_are_bounded_by_the_headroom() {
    let harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE metadata SET value = ?1 WHERE key = 'command_count'",
                [MAX_COMMANDS_WITH_CONTROL_HEADROOM.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun {
                run_id: harness.run_id,
            },
        )
        .await;
    assert!(matches!(
        refused,
        Err(SessionRuntimeError::CommandLimitReached)
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn model_fallback_provenance_survives_commands_and_run_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, session_id, first) = create_claimed_parent(&store, directory.path()).await;
    store
        .finish_run(
            &first,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    for model_is_fallback in [true, false] {
        let selection = ModelSelection {
            model_is_fallback,
            model: Some("test/model".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        };
        let changed = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetSessionModel {
                    session_id,
                    model: selection.clone(),
                },
            )
            .await
            .unwrap();
        let events = store
            .events_after(
                first.identity.workspace_id,
                changed.receipt.committed_through.sequence - 1,
                10,
            )
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(&event.event, SessionEvent::SessionUpdated { session } if session.model_is_fallback == model_is_fallback)));
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::text("retain choice".to_owned())],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let claimed = store.reserve_next_run(false).await.unwrap().unwrap();
        assert_eq!(claimed.model, selection);
        assert_eq!(claimed.session_model, selection);
        store
            .finish_reserved_run(&claimed, RunOutcome::Cancelled)
            .await
            .unwrap();
    }
    store.close().await.unwrap();
}
