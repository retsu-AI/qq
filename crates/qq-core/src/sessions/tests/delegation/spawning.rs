use super::*;
use crate::test_support::within;

#[tokio::test]
async fn spawn_agent_runs_a_read_only_child_and_returns_its_final_text() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"/review Survey the widget inventory","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![(
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"x","new":"y"}]}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let skill = harness.directory.path().join(".qq/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "User-selected guidance only.\n").unwrap();
    let run_id =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate the survey").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let child_session = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.clone())
            }
            _ => None,
        })
        .expect("the spawn call must create a child session");
    assert_eq!(child_session.model.as_deref(), Some("test/child"));
    assert_eq!(child_session.status, SessionStatus::Queued);
    assert_eq!(child_session.queued_prompts, 1);
    assert_eq!(child_session.title, "/review Survey the widget inventory");
    let spawn_call = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallRequested { tool_call }
                if tool_call.run_id == run_id && tool_call.name == "spawn_agent" =>
            {
                Some(tool_call.id)
            }
            _ => None,
        })
        .expect("the parent requested a spawn_agent call");
    assert_eq!(
        child_session.spawned_by,
        Some(SpawnOrigin {
            run_id,
            tool_call_id: Some(spawn_call),
            depth: 1,
        })
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(child_session.id),
            8,
            8,
        ))
        .await
        .unwrap();
    assert_eq!(
        snapshot.focused.unwrap().summary.spawned_by,
        child_session.spawned_by
    );
    let created_event = observed
        .iter()
        .find(|event| {
            matches!(
                &event.event,
                SessionEvent::SessionCreated { session } if session.id == child_session.id
            )
        })
        .unwrap();
    let queued_event = observed
        .iter()
        .find(|event| {
            matches!(
                &event.event,
                SessionEvent::PromptQueued { session, .. } if session.id == child_session.id
            )
        })
        .unwrap();
    assert_eq!(
        created_event.cursor.sequence + 1,
        queued_event.cursor.sequence
    );
    assert_eq!(created_event.caused_by, queued_event.caused_by);
    assert!(created_event.caused_by.is_some());
    assert_eq!(created_event.run_id, queued_event.run_id);
    assert!(observed.iter().any(|event| matches!(
        &event.event,
            SessionEvent::PromptQueued { session, .. }
            if session.id == child_session.id
                && session.title == "/review Survey the widget inventory"
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    let follow_up = submit_prompt_to(
        &harness.runtime,
        child_session.id,
        "/review user-authored follow-up",
    )
    .await;
    let follow_up_events = collect_until_run_finished(&mut harness.events, follow_up).await;
    assert!(matches!(
        finished_outcome(&follow_up_events, follow_up),
        Some(RunOutcome::Completed)
    ));
    let child_reqs = child_requests.lock().unwrap().clone();
    assert!(
        !child_reqs[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent"),
        "child sessions must not have spawn_agent declared"
    );
    let child_tools: Vec<&str> = child_reqs[0]
        .tools()
        .iter()
        .map(qq_provider::ToolSpec::name)
        .collect();
    assert!(
        !child_tools
            .iter()
            .any(|name| matches!(*name, "edit_file" | "write_file" | "shell")),
        "read-only child was offered mutating schemas: {child_tools:?}"
    );
    assert!(child_tools.contains(&"read_file"));
    assert_eq!(
        child_reqs[0].messages(),
        [Message::user("/review Survey the widget inventory")]
    );
    assert!(
        !child_reqs[0]
            .system()
            .unwrap()
            .contains("User-selected guidance only."),
        "a model-created child task must not select runtime guidance"
    );
    assert!(matches!(
        child_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content == approval::POLICY_DENIED_RESULT
    ));
    assert!(
        child_reqs[2]
            .system()
            .unwrap()
            .contains("User-selected guidance only."),
        "an explicit user prompt in a child session may select runtime guidance"
    );
    let spawned_system = child_reqs[0].system().unwrap();
    assert!(
        spawned_system.contains("\n\nSub-agent:\n"),
        "{spawned_system}"
    );
    assert!(spawned_system.contains("You cannot change files or run commands"));
    assert!(!spawned_system.contains("- Do the work."));
    let follow_up_system = child_reqs[2].system().unwrap();
    assert!(!follow_up_system.contains("Sub-agent:"));
    assert!(!follow_up_system.contains("- Do the work."));
    assert!(follow_up_system.contains("This run cannot change files or run commands"));
    assert!(
        !child_reqs[2]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent"),
        "child sessions remain depth-capped after a user follow-up"
    );
    drop(child_reqs);
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert!(
        parent_reqs[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );
    assert!(!parent_reqs[0].system().unwrap().contains("Sub-agent:"));
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: false, .. }]
            if content.contains("working in the background")
    ));
    assert_eq!(delivered_answers(&parent_reqs), [("done".to_owned(), true)]);
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));

    let escalate = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: child_session.id,
                mode: ApprovalMode::Auto,
            },
        )
        .await;
    assert!(matches!(
        escalate,
        Err(SessionRuntimeError::ChildAuthorityEscalation)
    ));
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(child_session.id),
            8,
            8,
        ))
        .await
        .unwrap();
    assert_eq!(
        snapshot.focused.unwrap().summary.approval_mode,
        ApprovalMode::ReadOnly
    );
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: child_session.id,
                mode: ApprovalMode::ReadOnly,
            },
        )
        .await
        .expect("same authority is accepted");
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: harness.session_id,
                mode: ApprovalMode::Full,
            },
        )
        .await
        .expect("root sessions are unaffected");
}

#[tokio::test]
async fn a_child_sessions_summarizer_uses_the_childs_own_prompt_prefix() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Survey the widget inventory","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![("read_file", r#"{"path":"a.txt"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(harness.directory.path().join("a.txt"), "widgets\n").unwrap();
    let run_id =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate the survey").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let child_session = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.id)
            }
            _ => None,
        })
        .expect("a child session");

    let compaction = compact_session(&harness.runtime, child_session).await;
    let observed = collect_until_run_finished(&mut harness.events, compaction).await;
    assert_eq!(
        finished_outcome(&observed, compaction),
        Some(RunOutcome::Completed)
    );
    let requests = child_requests.lock().unwrap();
    let summarizer = requests.last().unwrap();
    let prompt = &requests[requests.len() - 2];
    assert!(
        request_texts(summarizer)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"))
    );
    assert!(prompt.system().unwrap().contains("Sub-agent:"));
    assert_eq!(summarizer.system(), prompt.system());
    assert_eq!(summarizer.tools(), prompt.tools());
    let tools: Vec<&str> = summarizer.tools().iter().map(|tool| tool.name()).collect();
    assert!(tools.contains(&"read_file"));
    assert!(
        !tools.contains(&"edit_file"),
        "a read child's list stays read-only"
    );
}

#[tokio::test]
async fn multi_turn_child_returns_only_its_final_answer() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Inspect the note","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(TurnTextProvider {
        requests: Arc::clone(&child_requests),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(harness.directory.path().join("note.txt"), "evidence\n").unwrap();
    let parent_run = submit_prompt_to(
        &harness.runtime,
        harness.session_id,
        "delegate the inspection",
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, parent_run).await;

    let child_session_id = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.id)
            }
            _ => None,
        })
        .unwrap();
    {
        let parent_requests = parent_requests.lock().unwrap();
        assert_eq!(
            delivered_answers(&parent_requests),
            [("done".to_owned(), true)]
        );
    }

    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(child_session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let assistant = snapshot
        .focused
        .unwrap()
        .messages
        .into_iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .map(|message| message.output)
        .collect::<Vec<_>>();
    assert_eq!(assistant, ["Let me look. ", "done"]);
}

#[tokio::test]
async fn child_final_refusal_is_returned_to_the_parent() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Attempt the task","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(RefusalProvider);
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate the task").await;
    collect_until_run_finished(&mut harness.events, parent_run).await;

    let parent_requests = parent_requests.lock().unwrap();
    assert_eq!(
        delivered_answers(&parent_requests),
        [("cannot complete that task".to_owned(), true)]
    );
}

#[tokio::test]
async fn failed_child_run_insert_leaves_no_idle_orphan() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"transactional research","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![parent],
        8,
    )
    .await;
    harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_spawned_child_run BEFORE INSERT ON runs
                     WHEN EXISTS (
                         SELECT 1 FROM sessions
                         WHERE id = NEW.session_id AND parent_id IS NOT NULL
                     )
                     BEGIN
                         SELECT RAISE(ABORT, 'injected child run failure');
                     END;",
                )
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
        })
        .await
        .unwrap();
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate atomically").await;
    let observed = collect_until_run_finished(&mut harness.events, parent_run).await;

    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::SessionCreated { .. })),
        "a rolled-back spawn must publish no child event"
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            4,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].id, harness.session_id);
    let parent_requests = parent_requests.lock().unwrap();
    assert!(matches!(
        parent_requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("sub-agent")
    ));
}

#[tokio::test]
async fn parent_cancellation_linearizes_with_in_flight_child_creation() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path).await.unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (created_tx, created_rx) = tokio::sync::oneshot::channel();
    let create_store = store.clone();
    let create_parent = parent.clone();
    let create_task = tokio::spawn(async move {
        let store_id = create_store.store_id();
        create_store
            .call(Priority::Control, move |connection| {
                let _ = entered_tx.send(());
                release_rx
                    .recv()
                    .map_err(|_| SessionRuntimeError::Unavailable)?;
                let result = create_child_run(
                    connection,
                    store_id,
                    ChildRunParent {
                        workspace_id: create_parent.identity.workspace_id,
                        session_id: create_parent.identity.session_id,
                        run_id: create_parent.identity.run_id,
                        tool_call_id: None,
                        depth: 0,
                        root_run_id: create_parent.identity.run_id,
                    },
                    ChildAdmission {
                        reasoning_effort: None,
                        profile: AgentProfileId::default(),
                        model: ModelSelection {
                            model_is_fallback: false,
                            model: Some("test/child".to_owned()),
                            max_output_tokens: Some(256),
                            organization: None,
                        },
                        task: "queued child task".to_owned(),
                        limits: RunLimits::default(),
                        approval_mode: ApprovalMode::ReadOnly,
                        purpose: SessionPurpose::Task,
                        detached: false,
                    },
                );
                let _ = created_tx.send(result.as_ref().ok().map(|created| {
                    (
                        created.session_id,
                        created.run_id,
                        created.committed_through,
                    )
                }));
                result
            })
            .await
    });
    entered_rx.await.unwrap();
    create_task.abort();
    assert!(matches!(create_task.await, Err(error) if error.is_cancelled()));
    release_tx.send(()).unwrap();
    let (_, child_run, _) = created_rx
        .await
        .unwrap()
        .expect("the detached store job should commit the child");

    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun {
                run_id: parent.identity.run_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .run_outcome(child_run)
            .await
            .unwrap()
            .map(|(outcome, _)| outcome),
        Some(RunOutcome::Cancelled)
    );
    assert!(store.claim_next_run(true).await.unwrap().is_none());
    store
        .finish_run(
            &parent,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(store.unfinished_run_ids().await.unwrap().is_empty());

    let (_, _, cancelling_parent) = create_claimed_parent(&store, directory.path()).await;
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun {
                run_id: cancelling_parent.identity.run_id,
            },
        )
        .await
        .unwrap();
    let rejected = store
        .create_child_run(
            &cancelling_parent,
            ToolCallId::from_bytes([0x5a; 16]),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                task: "too late".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: false,
            },
        )
        .await;
    assert!(matches!(rejected, Err(SessionRuntimeError::RunNotFound)));
    store
        .finish_run(
            &cancelling_parent,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(store.unfinished_run_ids().await.unwrap().is_empty());
}

#[tokio::test]
async fn replayed_parent_cancellation_rediscovers_its_running_child() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x5a; 16]),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                task: "running child task".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: false,
            },
        )
        .await
        .unwrap();
    let claimed_child = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claimed_child.identity.run_id, child.run_id);

    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::CancelRun {
        run_id: parent.identity.run_id,
    };
    let first = store.command(command_id, command.clone()).await.unwrap();
    let replay = store.command(command_id, command).await.unwrap();

    assert_eq!(replay.receipt, first.receipt);
    assert_eq!(first.cascade_cancels, [child.run_id]);
    assert_eq!(replay.cascade_cancels, [child.run_id]);
    store
        .finish_run(
            &claimed_child,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .finish_run(
            &parent,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(store.unfinished_run_ids().await.unwrap().is_empty());
}

#[tokio::test]
async fn restart_cancels_a_queued_child_owned_by_an_interrupted_parent() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
    let (workspace_id, root_session_id, parent) =
        create_claimed_parent(&store, directory.path()).await;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x5a; 16]),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                task: "queued child task".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: false,
            },
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    drop(store);

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let mut events = subscribe_after(&runtime, workspace_id, child.committed_through);
    let recovered = within(async {
        let mut recovered = Vec::new();
        while recovered
            .iter()
            .filter(|event: &&SessionEventEnvelope| {
                matches!(event.event, SessionEvent::RunFinished { .. })
            })
            .count()
            < 2
        {
            recovered.push(events.next().await.unwrap().unwrap());
        }
        recovered
    })
    .await
    .unwrap();
    assert!(recovered.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Cancelled,
            ..
        } if *run_id == child.run_id
    )));
    assert!(recovered.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            run_id,
            outcome: RunOutcome::Interrupted,
            ..
        } if *run_id == parent.identity.run_id
    )));
    assert!(requests.lock().unwrap().is_empty());
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(child.session_id),
            8,
            4,
        ))
        .await
        .unwrap();
    let child_snapshot = snapshot.focused.unwrap();
    assert_eq!(child_snapshot.summary.parent_id, Some(root_session_id));
    assert_eq!(child_snapshot.summary.status, SessionStatus::Idle);
    assert_eq!(child_snapshot.runs[0].outcome, Some(RunOutcome::Cancelled));
}

#[tokio::test]
async fn parallel_spawn_accounting_is_ordered_exact_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let mut options = SessionRuntimeOptions::new(database_path.clone());
    options.max_active_runs = 4;
    let runtime = SessionRuntime::open(options, Arc::new(AccountingSpawnLoader))
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated {
        session_id: parent_id,
    } = created.outcome
    else {
        panic!("unexpected receipt")
    };
    let mut events = subscribe_after(&runtime, workspace_id, created.committed_through);

    let parent_run = submit_prompt_to(&runtime, parent_id, "delegate twice").await;
    let observed = collect_until_run_finished(&mut events, parent_run).await;
    let child_ids = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::SessionCreated { session } if session.parent_id == Some(parent_id) => {
                Some(session.id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(child_ids.len(), 2);

    for child_id in &child_ids {
        let child_finished = observed
            .iter()
            .position(|event| {
                event.session_id == *child_id
                    && matches!(event.event, SessionEvent::RunFinished { .. })
            })
            .expect("child must finish");
        let parent_refreshed = &observed[child_finished + 1];
        assert_eq!(parent_refreshed.session_id, parent_id);
        assert!(matches!(
            parent_refreshed.event,
            SessionEvent::SessionUpdated { .. }
        ));
    }

    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(parent_id), 8, 8))
        .await
        .unwrap();
    let parent = snapshot
        .sessions
        .iter()
        .find(|session| session.id == parent_id)
        .unwrap();
    let accounting = parent.accounting.unwrap();
    let direct = accounting.direct.usage.unwrap();
    let text_turns = (direct.input_tokens - 2) / 5;
    assert!((1..=3).contains(&text_turns), "{direct:?}");
    assert_eq!(direct, usage(2 + 5 * text_turns, 3 + 7 * text_turns));
    assert_eq!(
        accounting.direct.estimated_cost_usd_nanos,
        Some(5 + 12 * text_turns)
    );
    let inclusive = accounting.inclusive.usage.unwrap();
    assert_eq!(inclusive.input_tokens - direct.input_tokens, 28);
    assert_eq!(inclusive.output_tokens - direct.output_tokens, 32);
    assert_eq!(
        accounting.inclusive.estimated_cost_usd_nanos.unwrap()
            - accounting.direct.estimated_cost_usd_nanos.unwrap(),
        60
    );
    let child_costs = child_ids
        .iter()
        .map(|child_id| {
            snapshot
                .sessions
                .iter()
                .find(|session| session.id == *child_id)
                .unwrap()
                .accounting
                .unwrap()
        })
        .map(|accounting| {
            assert_eq!(accounting.direct, accounting.inclusive);
            accounting.direct.estimated_cost_usd_nanos.unwrap()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(child_costs, std::collections::BTreeSet::from([24, 36]));

    drop(events);
    drop(runtime);
    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(AccountingSpawnLoader),
    )
    .await
    .unwrap();
    let reloaded = reopened
        .snapshot(SnapshotRequest::new(workspace_id, Some(parent_id), 8, 8))
        .await
        .unwrap();
    let reloaded_parent = reloaded
        .sessions
        .iter()
        .find(|session| session.id == parent_id)
        .unwrap();
    assert_eq!(reloaded_parent.accounting, parent.accounting);
}

#[tokio::test]
async fn configured_worker_model_wins_and_preserves_parent_selection_fields() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"configured worker research"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let resolutions = Arc::new(AtomicUsize::new(0));
    let loads = Arc::new(StdMutex::new(Vec::new()));
    let worker = ModelSelection {
        model_is_fallback: false,
        model: Some("test/worker".to_owned()),
        max_output_tokens: Some(123),
        organization: Some("worker-org".to_owned()),
    };
    let mut harness = spawn_harness_with_loader(
        Arc::new(ResolvingLoader {
            parent,
            child: Arc::new(StaticTextProvider),
            worker: Some(worker.clone()),
            resolutions: Arc::clone(&resolutions),
            loads: Arc::clone(&loads),
        }),
        8,
    )
    .await;

    let run_id = submit_prompt_to(
        &harness.runtime,
        harness.session_id,
        "use configured worker",
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let child = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session)
            }
            _ => None,
        })
        .expect("the configured worker must create a child");

    assert_eq!(child.model.as_deref(), Some("test/worker"));
    assert_eq!(resolutions.load(Ordering::Acquire), 1);
    assert!(
        loads
            .lock()
            .unwrap()
            .iter()
            .filter(|load| **load == worker)
            .count()
            >= 2,
        "the same complete selection must be preflighted and used by the child run"
    );
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn explicit_spawn_model_bypasses_configured_worker_resolution() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"specialized research","model":"test/explicit"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let resolutions = Arc::new(AtomicUsize::new(0));
    let loads = Arc::new(StdMutex::new(Vec::new()));
    let mut harness = spawn_harness_with_loader(
        Arc::new(ResolvingLoader {
            parent,
            child: Arc::new(StaticTextProvider),
            worker: Some(ModelSelection {
                model_is_fallback: false,
                model: Some("test/worker".to_owned()),
                max_output_tokens: Some(111),
                organization: Some("worker-org".to_owned()),
            }),
            resolutions: Arc::clone(&resolutions),
            loads,
        }),
        8,
    )
    .await;

    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "override worker").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let child = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session)
            }
            _ => None,
        })
        .expect("the explicit model must create a child");

    assert_eq!(child.model.as_deref(), Some("test/explicit"));
    assert_eq!(resolutions.load(Ordering::Acquire), 0);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn worker_resolution_failure_creates_no_child_state_and_parent_continues() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"research denied route"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness =
        spawn_harness_with_loader(Arc::new(RejectingWorkerLoader { parent }), 8).await;

    let run_id = submit_prompt_to(
        &harness.runtime,
        harness.session_id,
        "try configured worker",
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    assert!(
        observed.iter().all(|event| !matches!(
            &event.event,
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id)
        )),
        "resolution failure must not emit a child creation"
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            32,
            32,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    assert!(
        snapshot
            .sessions
            .iter()
            .all(|session| session.parent_id.is_none())
    );

    let requests = requests.lock().unwrap();
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("configured worker route is denied")
    ));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn rejected_spawn_validation_creates_no_child_state_and_names_the_check() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"research","model":"test/ghost"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let validations = Arc::new(StdMutex::new(Vec::new()));
    let loads = Arc::new(StdMutex::new(Vec::new()));
    let mut harness = spawn_harness_with_loader(
        Arc::new(ValidatingLoader {
            parent,
            child: Arc::new(StaticTextProvider),
            worker: None,
            rejection: Some(RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: "model \"ghost\" is not in provider \"test\"'s authenticated \
                          model list; available routes: test/child"
                    .to_owned(),
            }),
            validations: Arc::clone(&validations),
            loads: Arc::clone(&loads),
        }),
        8,
    )
    .await;

    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "spawn a ghost").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    assert_eq!(
        validations
            .lock()
            .unwrap()
            .iter()
            .map(|selection| selection.model.clone())
            .collect::<Vec<_>>(),
        [Some("test/ghost".to_owned())]
    );
    assert_eq!(
        loads
            .lock()
            .unwrap()
            .iter()
            .map(|selection| selection.model.clone())
            .collect::<Vec<_>>(),
        [Some("test/model".to_owned())],
        "a rejected route must never reach runtime loading"
    );
    assert!(observed.iter().all(|event| !matches!(
        &event.event,
        SessionEvent::SessionCreated { session }
            if session.parent_id == Some(harness.session_id)
    )));
    assert!(observed.iter().all(|event| !matches!(
        &event.event,
        SessionEvent::PromptQueued { session, .. } if session.id != harness.session_id
    )));
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            32,
            32,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    assert!(
        snapshot
            .sessions
            .iter()
            .all(|session| session.parent_id.is_none())
    );
    let parent_reqs = requests.lock().unwrap();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("the sub-agent model was rejected")
                && content.contains("authenticated model list")
                && content.contains("available routes: test/child")
    ));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn spawn_validation_covers_worker_and_parent_fallback_routes() {
    for worker in [
        Some(ModelSelection {
            model_is_fallback: false,
            model: Some("test/worker".to_owned()),
            max_output_tokens: Some(64),
            organization: None,
        }),
        None,
    ] {
        let expected = worker
            .as_ref()
            .and_then(|worker| worker.model.clone())
            .unwrap_or_else(|| "test/model".to_owned());
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![("spawn_agent", r#"{"task":"survey"}"#.to_owned())],
            turn: StdMutex::new(0),
        });
        let validations = Arc::new(StdMutex::new(Vec::new()));
        let mut harness = spawn_harness_with_loader(
            Arc::new(ValidatingLoader {
                parent,
                child: Arc::new(StaticTextProvider),
                worker,
                rejection: None,
                validations: Arc::clone(&validations),
                loads: Arc::new(StdMutex::new(Vec::new())),
            }),
            8,
        )
        .await;

        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;

        assert_eq!(
            validations
                .lock()
                .unwrap()
                .iter()
                .map(|selection| selection.model.clone())
                .collect::<Vec<_>>(),
            [Some(expected.clone())]
        );
        let child = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionCreated { session }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    Some(session.clone())
                }
                _ => None,
            })
            .expect("an accepted route must create the child");
        assert_eq!(child.model.as_deref(), Some(expected.as_str()));
        assert!(matches!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        ));
    }
}

#[tokio::test]
async fn explicit_route_outside_the_advertised_list_spawns_when_validation_accepts() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/discovered"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;

    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "spawn discovered").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let child = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.clone())
            }
            _ => None,
        })
        .expect("a validated route outside the advertised list must spawn");
    assert_eq!(child.model.as_deref(), Some("test/discovered"));
    let parent_reqs = requests.lock().unwrap();
    assert!(matches!(
        delivered_answers(&parent_reqs).as_slice(),
        [(content, true)] if content == "done"
    ));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn child_sessions_cannot_spawn_and_dispatch_rejects_the_attempt() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&requests),
        script: vec![("spawn_agent", r#"{"task":"go deeper"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let harness = spawn_harness(Vec::new(), vec![provider], 8).await;
    let created = create_session(
        &harness.runtime,
        harness.workspace_id,
        Some(harness.session_id),
    )
    .await;
    let CommandOutcome::SessionCreated {
        session_id: child_id,
    } = created.outcome
    else {
        panic!("unexpected receipt")
    };
    let mut events = subscribe_after(
        &harness.runtime,
        harness.workspace_id,
        created.committed_through,
    );
    let run_id = submit_prompt_to(&harness.runtime, child_id, "try to spawn").await;
    let observed = collect_until_run_finished(&mut events, run_id).await;

    let child_reqs = requests.lock().unwrap();
    assert!(
        !child_reqs[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent"),
        "the child run must not declare spawn_agent"
    );
    assert!(matches!(
        child_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content == crate::SPAWN_UNAVAILABLE_RESULT
    ));
    drop(child_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    assert!(
        !observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::SessionCreated { session } if session.parent_id == Some(child_id)
        )),
        "no grandchild session may appear"
    );
}

#[tokio::test]
async fn a_failed_child_returns_a_tool_error_and_the_parent_continues() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"doomed research","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(
        vec![("test/child", Arc::new(FailingProvider))],
        vec![parent],
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id: done, outcome: RunOutcome::Paused { .. }, .. }
            if *done != run_id
    )));
    let parent_reqs = parent_requests.lock().unwrap();
    assert!(matches!(
        delivered_answers(&parent_reqs).as_slice(),
        [(content, false)]
            if content.contains("the sub-agent run paused") && content.contains("offline")
    ));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}
