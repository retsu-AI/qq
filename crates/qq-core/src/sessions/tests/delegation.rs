use super::*;
use crate::CheckpointPhase;

#[tokio::test]
async fn child_checkpoint_inheritance_preserves_profile_but_not_user_followups() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (workspace_id, session_id, parent) = create_claimed_parent(&store, directory.path()).await;
    let profile = AgentProfileId::new("plain").unwrap();
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x5a; 16]),
            ChildAdmission {
                profile: profile.clone(),
                model: parent.model.clone(),
                task: "child task".into(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claimed.identity.run_id, child.run_id);
    assert_eq!(claimed.profile, profile);
    assert_eq!(claimed.checkpoint, Some(CheckpointSelection::Disabled));
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: child.session_id,
                input: vec![InputPart::text("user followup")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let followup = store.claim_next_run(true).await.unwrap().unwrap();
    assert!(followup.user_initiated);
    assert_eq!(followup.profile, profile);
    assert_eq!(
        followup.checkpoint, None,
        "user-selected configuration must take effect"
    );
    let public_child = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: Some(session_id),
                model: parent.model.clone(),
                approval_mode: ApprovalMode::ReadOnly,
                profile: profile.clone(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated {
        session_id: public_id,
    } = public_child.receipt.outcome
    else {
        panic!("session expected")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: public_id,
                input: vec![InputPart::text("public child task")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let public = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(public.identity.session_id, public_id);
    assert_eq!(public.profile, profile);
    assert_eq!(
        public.checkpoint, None,
        "parented public sessions use their own configuration"
    );
}

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
    // The child attempts a mutation first; read-only mode must deny it
    // without prompting, and the child then completes with "done".
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![(
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"x","new":"y"}]}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let skill = harness._directory.path().join(".qq/skills/review");
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
    // The child names the parent run and the exact `spawn_agent` call that
    // created it, so clients can render it under that call.
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
    // A snapshot taken later reports the same origin from the persisted row.
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
    // The task's first line names the child at prompt submission.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
            SessionEvent::PromptQueued { session, .. }
            if session.id == child_session.id
                && session.title == "/review Survey the widget inventory"
    )));
    // spawn_agent is read-only: no approval round trip in Ask mode.
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
    // A read-only child is never offered the schemas its policy would
    // deny; the denial below covers a model that guesses the name anyway.
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
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: false, .. }] if content == "done"
    ));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));

    // A client cannot raise the child above what its parent granted; the
    // refusal is typed and leaves the row untouched. Lowering (a no-op
    // here) and root sessions remain free to change.
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
async fn child_final_checkpoint_is_durable_before_parent_spawn_result() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let reviewed = Arc::new(StdMutex::new(Vec::new()));
    let mut harness = spawn_harness_with_loader(
        Arc::new(CheckpointQueueLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
            reviewed: Arc::clone(&reviewed),
        }),
        8,
    )
    .await;
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, parent_run).await;

    let child_run = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(run.id)
            }
            _ => None,
        })
        .expect("spawn creates a child run");
    let child_checkpoint = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEventEnvelope {
                    run_id: Some(run_id),
                    event: SessionEvent::CheckpointReviewed {
                        phase: qq_protocol::CheckpointPhase::FinalCandidate,
                        outcome: qq_protocol::CheckpointOutcome::Supported,
                        ..
                    },
                    ..
                } if *run_id == child_run
            )
        })
        .expect("child final checkpoint is durable");
    let child_finished = observed
        .iter()
        .position(|event| {
            matches!(
                event.event,
                SessionEvent::RunFinished { run_id, .. } if run_id == child_run
            )
        })
        .expect("child settles");
    let parent_spawn_result = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolCallFinished { tool_call }
                    if tool_call.run_id == parent_run && tool_call.name == "spawn_agent"
            )
        })
        .expect("parent receives the spawn result");
    let parent_tool_checkpoint = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEventEnvelope {
                    run_id: Some(run_id),
                    event: SessionEvent::CheckpointReviewed {
                        phase: qq_protocol::CheckpointPhase::ToolResult,
                        outcome: qq_protocol::CheckpointOutcome::Supported,
                        ..
                    },
                    ..
                } if *run_id == parent_run
            )
        })
        .expect("parent spawn result is checkpointed");
    let parent_final_checkpoint = observed
        .iter()
        .rposition(|event| {
            matches!(
                event,
                SessionEventEnvelope {
                    run_id: Some(run_id),
                    event: SessionEvent::CheckpointReviewed {
                        phase: qq_protocol::CheckpointPhase::FinalCandidate,
                        outcome: qq_protocol::CheckpointOutcome::Supported,
                        ..
                    },
                    ..
                } if *run_id == parent_run
            )
        })
        .expect("parent final candidate is checkpointed");
    let parent_finished = observed
        .iter()
        .position(|event| {
            matches!(
                event.event,
                SessionEvent::RunFinished { run_id, .. } if run_id == parent_run
            )
        })
        .expect("parent settles");

    assert!(child_checkpoint < child_finished);
    assert!(child_finished < parent_spawn_result);
    assert!(parent_spawn_result < parent_tool_checkpoint);
    assert!(parent_tool_checkpoint < parent_final_checkpoint);
    assert!(parent_final_checkpoint < parent_finished);
    assert_eq!(
        reviewed
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.phase == CheckpointPhase::FinalCandidate)
            .count(),
        2
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
    std::fs::write(harness._directory.path().join("note.txt"), "evidence\n").unwrap();
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
        assert!(matches!(
            parent_requests[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: false, .. }]
                if content == "done"
        ));
    }

    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(child_session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 8,
        })
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
    assert!(matches!(
        parent_requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: false, .. }]
            if content == "cannot complete that task"
    ));
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
        .inner
        .store
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
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 4,
        })
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

    // Hold the database operation after Store::call has handed it to the
    // worker, then drop its awaiting task. The worker must still commit,
    // reproducing the handoff window where no CancelChildOnDrop guard can
    // be installed by the cancelled spawn future.
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

    // The reverse database ordering rejects creation once cancellation is
    // durable, so no child can appear after its parent starts settling.
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
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: child.committed_through,
        })
        .unwrap();
    let recovered = tokio::time::timeout(Duration::from_secs(2), async {
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
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(child.session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 4,
        })
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
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();

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
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(parent_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 8,
        })
        .await
        .unwrap();
    let parent = snapshot
        .sessions
        .iter()
        .find(|session| session.id == parent_id)
        .unwrap();
    let accounting = parent.accounting.unwrap();
    assert_eq!(accounting.direct.usage, Some(usage(7, 10)));
    assert_eq!(accounting.direct.estimated_cost_usd_nanos, Some(17));
    assert_eq!(accounting.inclusive.usage, Some(usage(35, 42)));
    assert_eq!(accounting.inclusive.estimated_cost_usd_nanos, Some(77));
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
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(parent_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 8,
        })
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
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 32,
            message_limit: 32,
        })
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

    // The explicit argument passed through the spawn-time choke point...
    assert_eq!(
        validations
            .lock()
            .unwrap()
            .iter()
            .map(|selection| selection.model.clone())
            .collect::<Vec<_>>(),
        [Some("test/ghost".to_owned())]
    );
    // ...was rejected before any child runtime load...
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
    // ...and created no durable child state: no session, no prompt, no
    // run.
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
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 32,
            message_limit: 32,
        })
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    assert!(
        snapshot
            .sessions
            .iter()
            .all(|session| session.parent_id.is_none())
    );
    // The parent sees a bounded tool error naming the failed check and
    // continues to completion.
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

        // The resolved route — configured worker or parent fallback —
        // passed through the same spawn-time choke point the explicit
        // argument uses, and the child was created on it.
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
    // QueueLoader advertises only "test/child" in the schema route list;
    // "test/discovered" must still spawn because enforcement lives at
    // the spawn-time choke point (the served model list), not in the
    // schema enum.
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
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: false, .. }] if content == "done"
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
    let mut events = harness
        .runtime
        .subscribe(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after: created.committed_through,
        })
        .unwrap();
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

    // The child run failed, visibly, on its own session.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id: done, outcome: RunOutcome::Failed { .. }, .. }
            if *done != run_id
    )));
    // The parent saw a tool error and still completed.
    let parent_reqs = parent_requests.lock().unwrap();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("the sub-agent run failed") && content.contains("offline")
    ));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn cancelling_the_parent_run_cancels_its_in_flight_child() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        // No model override: the child must inherit the parent's model.
        script: vec![("spawn_agent", r#"{"task":"long research"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let hanging: Arc<dyn Provider> = Arc::new(HangingProvider);
    let mut harness = spawn_harness(Vec::new(), vec![parent, hanging], 8).await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate forever").await;

    // Wait until the child run is actually executing, then cancel the
    // parent.
    let mut observed = Vec::new();
    let (child_session, child_run) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            let started = match &event.event {
                SessionEvent::RunStarted {
                    session, run_id, ..
                } if *run_id != parent_run && session.parent_id == Some(harness.session_id) => {
                    Some((session.clone(), *run_id))
                }
                _ => None,
            };
            observed.push(event);
            if let Some(started) = started {
                break started;
            }
        }
    })
    .await
    .expect("timed out waiting for the child run to start");
    assert_eq!(
        child_session.model.as_deref(),
        Some("test/model"),
        "a child without a model argument inherits the parent's model"
    );
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: parent_run },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while finished_outcome(&observed, parent_run).is_none()
            || finished_outcome(&observed, child_run).is_none()
        {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("timed out waiting for the parent and child to settle");
    assert!(matches!(
        finished_outcome(&observed, parent_run),
        Some(RunOutcome::Cancelled)
    ));
    assert!(matches!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Cancelled)
    ));
}

#[tokio::test]
async fn cancelling_the_parent_interrupts_an_in_flight_child_tool() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"slow read-only work","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "__test_delay",
            r#"{"delay_ms":5000,"result":"too late"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate slow work").await;

    let mut observed = Vec::new();
    let (child_run, child_call) = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            let started = match &event.event {
                SessionEvent::ToolCallStarted { tool_call } if tool_call.name == "__test_delay" => {
                    Some((tool_call.run_id, tool_call.id))
                }
                _ => None,
            };
            observed.push(event);
            if let Some(started) = started {
                break started;
            }
        }
    })
    .await
    .expect("the child tool never started");
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: parent_run },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while finished_outcome(&observed, parent_run).is_none()
            || finished_outcome(&observed, child_run).is_none()
        {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("parent cancellation did not stop the child tool");
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.id == child_call && tool_call.state == ToolCallState::Interrupted
    )));
    assert!(matches!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Cancelled)
    ));
    assert!(matches!(
        finished_outcome(&observed, parent_run),
        Some(RunOutcome::Cancelled)
    ));
}

#[tokio::test]
async fn cancelling_the_parent_after_child_completion_preserves_the_child() {
    let second_turn_started = Arc::new(tokio::sync::Notify::new());
    let parent: Arc<dyn Provider> = Arc::new(SpawnThenHangProvider {
        turn: AtomicUsize::new(0),
        second_turn_started: Arc::clone(&second_turn_started),
    });
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate then wait").await;

    let mut observed = Vec::new();
    let (child_session_id, child_run) = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            let completed = match &event.event {
                SessionEvent::RunFinished {
                    session,
                    run_id,
                    outcome: RunOutcome::Completed,
                    ..
                } if session.parent_id == Some(harness.session_id) => Some((session.id, *run_id)),
                _ => None,
            };
            observed.push(event);
            if let Some(completed) = completed {
                break completed;
            }
        }
    })
    .await
    .expect("the child did not complete");
    tokio::time::timeout(Duration::from_secs(2), second_turn_started.notified())
        .await
        .expect("the parent did not consume the child answer");
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: parent_run },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while finished_outcome(&observed, parent_run).is_none() {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("the parent did not settle");

    assert!(matches!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Completed)
    ));
    assert!(matches!(
        finished_outcome(&observed, parent_run),
        Some(RunOutcome::Cancelled)
    ));
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::CancellationRequested { run_id, .. } if *run_id == child_run
    )));
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(child_session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 4,
        })
        .await
        .unwrap();
    assert_eq!(
        snapshot.focused.unwrap().runs[0].outcome,
        Some(RunOutcome::Completed)
    );
}

#[tokio::test]
async fn shutdown_settles_a_running_parent_and_its_in_flight_child() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![("spawn_agent", r#"{"task":"long research"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let hanging: Arc<dyn Provider> = Arc::new(HangingProvider);
    let mut harness = spawn_harness(Vec::new(), vec![parent, hanging], 8).await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate forever").await;

    let mut observed = Vec::new();
    let (child_session_id, child_run) = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            let child = match &event.event {
                SessionEvent::RunStarted {
                    session, run_id, ..
                } if *run_id != parent_run && session.parent_id == Some(harness.session_id) => {
                    Some((session.id, *run_id))
                }
                _ => None,
            };
            observed.push(event);
            if let Some(child) = child {
                break child;
            }
        }
    })
    .await
    .expect("the child must start before shutdown");

    tokio::time::timeout(Duration::from_secs(1), harness.runtime.shutdown())
        .await
        .expect("shutdown must settle the parent and child")
        .unwrap();

    let mut terminal_count = 0;
    tokio::time::timeout(Duration::from_secs(1), async {
        while finished_outcome(&observed, parent_run).is_none()
            || finished_outcome(&observed, child_run).is_none()
        {
            let event = harness.events.next().await.unwrap().unwrap();
            if matches!(event.event, SessionEvent::RunFinished { .. }) {
                terminal_count += 1;
            }
            observed.push(event);
        }
    })
    .await
    .expect("both accepted runs must publish terminal events");
    assert_eq!(terminal_count, 2);
    assert!(matches!(
        finished_outcome(&observed, parent_run),
        Some(RunOutcome::Cancelled)
    ));
    assert!(matches!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Cancelled)
    ));

    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(child_session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 4,
        })
        .await
        .unwrap();
    assert!(
        snapshot
            .sessions
            .iter()
            .all(|session| session.active_run_id.is_none())
    );
    assert_eq!(
        snapshot.focused.unwrap().runs[0].outcome,
        Some(RunOutcome::Cancelled)
    );
}

#[tokio::test]
async fn shutdown_waits_for_started_child_loader_work_without_creating_a_child() {
    struct HeldChildLoader {
        inner: QueueLoader,
        entered: Arc<tokio::sync::Notify>,
        release: StdMutex<Option<std::sync::mpsc::Receiver<()>>>,
        completed: Arc<AtomicBool>,
    }

    impl RuntimeLoader for HeldChildLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            if request.model.model.as_deref() != Some("test/child") {
                return self.inner.load(request);
            }
            let release = self
                .release
                .lock()
                .unwrap()
                .take()
                .expect("only one child load");
            let entered = Arc::clone(&self.entered);
            let completed = Arc::clone(&self.completed);
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    entered.notify_one();
                    release.recv().unwrap();
                    completed.store(true, Ordering::Release);
                })
                .await
                .unwrap();
                Runtime::new(StaticTextProvider, "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"prepare","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let entered = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(AtomicBool::new(false));
    let (release, release_rx) = std::sync::mpsc::channel();
    let mut harness = spawn_harness_with_loader(
        Arc::new(HeldChildLoader {
            inner: QueueLoader {
                routed: vec![("test/child", Arc::new(StaticTextProvider))],
                queue: StdMutex::new(vec![parent]),
            },
            entered: Arc::clone(&entered),
            release: StdMutex::new(Some(release_rx)),
            completed: Arc::clone(&completed),
        }),
        1,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let stopped = execution::observe_execution_stop(run_id);
    let runtime = harness.runtime.clone();
    let mut shutdown = tokio::spawn(async move { runtime.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), stopped)
        .await
        .unwrap()
        .unwrap();
    let premature = tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
        .await
        .is_ok();
    let completed_before_release = completed.load(Ordering::Acquire);
    release.send(()).unwrap();
    assert!(
        !premature,
        "shutdown cannot finish while admitted loader work is running"
    );
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!completed_before_release);
    assert!(completed.load(Ordering::Acquire));
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(&event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some())));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Cancelled)
    );
}

#[tokio::test]
async fn shutdown_closes_child_admission_before_scanning_unfinished_runs() {
    struct PausedChildLoader {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl RuntimeLoader for PausedChildLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.entered.notify_one();
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                release.notified().await;
                Runtime::new(StaticTextProvider, "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(PausedChildLoader {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let parent_run = RunId::generate().unwrap();
    let parent = ClaimedRun {
        checkpoint: None,
        identity: RunIdentity {
            workspace_id,
            session_id,
            run_id: parent_run,
            command_id: CommandId::generate().unwrap(),
            kind: RunKind::Prompt,
            child: false,
        },
        workspace: std::fs::canonicalize(directory.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
        user_initiated: true,
        literal_slash: false,
        session_model: ModelSelection {
            model_is_fallback: false,
            model: Some("test/model".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        },
        model: ModelSelection {
            model_is_fallback: false,
            model: Some("test/model".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        },
        messages: Vec::new(),
        context_compaction_attempted: 0,
        context_compaction_failed: false,
        context_compaction_remaining: false,
        compaction_cutoff_ordinal: None,
        context_compaction_oversized_unit_bytes: None,
        context_overflow_basis: None,
        context_occupancy: None,
        limits: RunLimits::default(),
        input: Vec::new(),
        resolved_input: None,
        profile: AgentProfileId::default(),
        approval_mode: ApprovalMode::default(),
        depth: 0,
        root_run_id: parent_run,
        purpose: SessionPurpose::Task,
        cancel_requested: false,
        file_state: Vec::new(),
        pending_steering: Vec::new(),
        output: None,
    };
    let child = tokio::spawn(spawn_child_run(
        Arc::clone(&runtime.inner),
        parent,
        subagents::SpawnBudget {
            slots: Arc::new(Semaphore::new(1)),
            write_slot: Arc::new(Semaphore::new(1)),
            write_children: false,
            spawned: Arc::new(AtomicUsize::new(0)),
            max_children: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
            tasks: Arc::new(subagents::ChildTasks::default()),
        },
        SpawnRequest {
            call_id: ToolCallId::from_bytes([0x5a; 16]),
            task: "research".to_owned(),
            model: None,
            authority: qq_protocol::ChildAuthority::Read,
            budget: crate::runtime::ChildBudget::default(),
            purpose: SessionPurpose::Task,
        },
    ));
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("child admission must reach its pre-commit load");

    tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("shutdown must complete while pre-commit child work is paused")
        .unwrap();
    release.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(1), child)
        .await
        .expect("released child admission must observe shutdown")
        .unwrap();
    assert!(outcome.is_error);
    assert!(outcome.content.contains("shutting down"));

    let snapshot = runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 1,
        })
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].id, session_id);
    assert_eq!(snapshot.sessions[0].active_run_id, None);
}

#[tokio::test]
async fn a_write_child_runs_supervised_and_the_reviewer_adjudicates_each_action() {
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![(
            "write_file",
            r#"{"path":"child.txt","content":"written by the child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let (mut harness, parent_requests) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"Create child.txt with a greeting","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        Some(reviewer),
        ApprovalMode::Auto,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
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
        .expect("the write spawn creates a child");
    assert_eq!(child_session.approval_mode, ApprovalMode::Supervised);
    // The child's write was held, adjudicated, and executed.
    let resolutions = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution,
            } if tool_call.session_id == child_session.id => {
                Some((tool_call.name.clone(), *resolution))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        resolutions,
        vec![(
            "write_file".to_owned(),
            ApprovalResolution::ApprovedByReviewer
        )]
    );
    assert_eq!(
        std::fs::read_to_string(harness._directory.path().join("child.txt")).unwrap(),
        "written by the child"
    );
    // The reviewer saw the child's brief, its arguments, and its origin.
    let requests = consulted.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_name, "write_file");
    assert_eq!(requests[0].mode, ApprovalMode::Supervised);
    assert!(matches!(
        requests[0].origin,
        ReviewOrigin::Child { depth: 1, .. }
    ));
    assert_eq!(
        requests[0].task_brief.as_deref(),
        Some("Create child.txt with a greeting")
    );
    assert!(requests[0].arguments.contains("child.txt"));
    drop(requests);
    // The child kept the mutating schemas: supervised holds, it does not
    // withhold.
    let child_reqs = child_requests.lock().unwrap().clone();
    assert!(
        child_reqs[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "write_file")
    );
    // The spawn itself was a mutating call under the parent's Auto policy
    // and executed without a prompt.
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalRequested { tool_call, .. } if tool_call.run_id == run_id
    )));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: false, .. }] if content == "done"
    ));
}

#[tokio::test]
async fn a_reviewer_denial_is_final_for_a_supervised_child_and_is_durable() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict {
        decision: ReviewDecision::Deny {
            reason: "not needed for the brief".to_owned(),
        },
        spend: ReviewSpend {
            usage: Some(TokenUsage {
                input_tokens: 40,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 8,
                reasoning_tokens: None,
            }),
            cost_usd_nanos: Some(0),
        },
    });
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![(
            "write_file",
            r#"{"path":"forbidden.txt","content":"x"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"Read the docs","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        Some(reviewer),
        ApprovalMode::Auto,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let denied = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::DeniedByReviewer,
            } => Some(tool_call.clone()),
            _ => None,
        })
        .expect("the reviewer denial is published");
    assert_eq!(denied.state, ToolCallState::Denied);
    assert!(
        denied
            .result
            .as_deref()
            .unwrap()
            .contains("not needed for the brief")
    );
    assert!(!harness._directory.path().join("forbidden.txt").exists());
    // The child saw the denial as a tool error and finished; the parent
    // received the child's answer.
    let child_reqs = child_requests.lock().unwrap().clone();
    assert!(matches!(
        child_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("reviewer denied")
    ));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    // The scripted child reports no usage of its own, so the reviewer's
    // known spend cannot make the child's total known; the run settles
    // with unknown usage rather than a partial number.
    let child_run = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { session, usage, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(*usage)
            }
            _ => None,
        })
        .expect("the child run finished");
    assert_eq!(child_run, None);
}

#[test]
fn reviewer_spend_joins_run_accounting_and_unknown_spend_poisons_it() {
    let mut accounting = RunAccountingAccumulator::new(
        Some(ModelPricing {
            input_usd_nanos_per_token: 10,
            output_usd_nanos_per_token: 20,
            cache_read_usd_nanos_per_token: None,
            cache_write_usd_nanos_per_token: None,
            context_tier: None,
            provenance: "test".to_owned(),
        }),
        ContextOccupancyBasis {
            version: 1,
            shape: ContentHash::from_bytes([0; 32]),
            static_prefix: PreparedStaticPrefix::new(ContentHash::from_bytes([0; 32]), None),
            request_bytes: 0,
        },
    );
    accounting.record_turn(Some(usage(100, 10)));
    accounting.record_review(Some(usage(40, 8)), Some(560));
    let snapshot = accounting.snapshot();
    assert_eq!(snapshot.usage, Some(usage(140, 18)));
    assert_eq!(
        snapshot.estimated_cost_usd_nanos,
        Some(100 * 10 + 10 * 20 + 560)
    );
    // Occupancy is untouched: the reviewer's request is not this run's.
    assert_eq!(snapshot.context_tokens, Some(100));
    accounting.record_review(None, None);
    let snapshot = accounting.snapshot();
    assert_eq!(snapshot.usage, None);
    assert_eq!(snapshot.estimated_cost_usd_nanos, None);
}

#[tokio::test]
async fn write_children_are_refused_without_the_roster_flag_or_a_reviewer() {
    // Reviewer present but the roster forbids writers: QueueLoader plans
    // carry the default roster.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"))
        .with_approval_reviewer(reviewer);
    let runtime = SessionRuntime::open(
        options,
        Arc::new(QueueLoader {
            routed: vec![("test/child", child)],
            queue: StdMutex::new(vec![parent]),
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
    let run_id = submit_prompt_to(&runtime, session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some()
    )));
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("write sub-agents are not enabled")
    ));
    assert!(consulted.lock().unwrap().is_empty());
    runtime.shutdown().await.unwrap();

    // Roster permits writers but no reviewer is installed.
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let (mut harness, parent_requests) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        None,
        ApprovalMode::Auto,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some()
    )));
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("require a configured reviewer_model")
    ));
}

#[tokio::test]
async fn a_write_spawn_is_a_mutating_call_under_the_parents_policy() {
    // Under ReadOnly the delegation itself is denied without a prompt.
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let (mut harness, parent_requests) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        Some(reviewer),
        ApprovalMode::ReadOnly,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some()
    )));
    // A read-only root is not even offered spawn_agent's write authority
    // ... but a guessed call is still denied by policy, never executed.
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content == approval::POLICY_DENIED_RESULT
    ));

    // Under Ask the human approves the delegation itself.
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        Some(reviewer),
        ApprovalMode::Ask,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let (_, held) = collect_until_approval_requested(&mut harness.events).await;
    assert_eq!(held.name, "spawn_agent");
    assert_eq!(held.run_id, run_id);
    respond_approval(
        &harness.runtime,
        run_id,
        held.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session }
            if session.parent_id.is_some() && session.approval_mode == ApprovalMode::Supervised
    )));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn steering_charges_a_completed_but_unconsumed_child_exactly_once() {
    struct MeteredScript {
        inner: ScriptedRunProvider,
        usage: Vec<TokenUsage>,
        turn: AtomicUsize,
    }
    impl Provider for MeteredScript {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            let usage = provider_usage_of(self.usage[turn.min(self.usage.len() - 1)]);
            Box::pin(self.inner.stream(request).map(move |event| match event {
                Ok(qq_provider::ProviderEvent::Completed { .. }) => {
                    Ok(qq_provider::ProviderEvent::Completed { usage: Some(usage) })
                }
                event => event,
            }))
        }
    }
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent = Arc::new(MeteredScript {
        inner: ScriptedRunProvider {
            requests: Arc::clone(&parent_requests),
            script: vec![(
                "spawn_agent",
                r#"{"task":"research","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        },
        usage: vec![usage(2, 0), usage(10, 0)],
        turn: AtomicUsize::new(0),
    });
    let child = Arc::new(MeteredScript {
        inner: ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: Vec::new(),
            turn: StdMutex::new(0),
        },
        usage: vec![usage(30, 0)],
        turn: AtomicUsize::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let (delivered, release) = subagents::hold_child_delivery(harness.session_id);
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("delegate")],
                limits: RunLimits {
                    max_total_tokens: Some(40),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("expected run");
    };
    tokio::time::timeout(Duration::from_secs(2), delivered)
        .await
        .unwrap()
        .unwrap();
    steer(
        &harness.runtime,
        run_id,
        "continue with your own answer",
        true,
    )
    .await
    .unwrap();
    // The interrupt wins the biased select even though the reply is ready.
    let _ = release.send(());
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let exhaustion = exhaustion_of(&observed, run_id);
    assert_eq!(exhaustion.limit, BudgetLimitKind::TotalTokens);
    assert!(
        exhaustion.message.contains("42 total tokens"),
        "{}",
        exhaustion.message
    );
    assert_eq!(parent_requests.lock().unwrap().len(), 2);
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_mutation_drains_before_steering_or_a_replacement_run_can_write() {
    for interruption in [0, 1, 2, 3, 4] {
        let shutting_down = interruption == 3;
        let deadline = interruption == 4;
        let cancel_parent = interruption != 0;
        let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
        let child = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "write_file",
                r#"{"path":"child.txt","content":"child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        });
        let (mut harness, parent_requests) = write_child_harness(
            child,
            vec![
                (
                    "spawn_agent",
                    r#"{"task":"write","model":"test/child","authority":"write"}"#.to_owned(),
                ),
                (
                    "write_file",
                    r#"{"path":"parent.txt","content":"parent"}"#.to_owned(),
                ),
            ],
            Some(reviewer),
            ApprovalMode::Full,
        )
        .await;
        let workspace = std::fs::canonicalize(harness._directory.path()).unwrap();
        let (applying, release) = crate::tools::hold_tool_apply(&workspace);
        let parent_run = if deadline {
            let receipt = harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id: harness.session_id,
                        input: vec![InputPart::text("delegate".to_owned())],
                        limits: RunLimits {
                            max_duration_ms: Some(1_000),
                            ..RunLimits::default()
                        },
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
                panic!("prompt")
            };
            run_id
        } else {
            submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await
        };
        tokio::time::timeout(Duration::from_secs(2), applying)
            .await
            .unwrap()
            .unwrap();
        let mut observed = Vec::new();
        let child_run = loop {
            let event = harness.events.next().await.unwrap().unwrap();
            let child = event.run_id.filter(|run| {
                *run != parent_run && matches!(event.event, SessionEvent::RunStarted { .. })
            });
            observed.push(event);
            if let Some(run) = child {
                break run;
            }
        };
        let mut stopping = Some(execution::observe_execution_stop(child_run));
        if interruption == 2 {
            steer(&harness.runtime, parent_run, "stop the child", true)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), stopping.take().unwrap())
                .await
                .unwrap()
                .unwrap();
        }
        let shutdown = if shutting_down {
            let runtime = harness.runtime.clone();
            Some(tokio::spawn(async move { runtime.shutdown().await }))
        } else {
            None
        };
        let last_run = if shutting_down {
            parent_run
        } else if deadline {
            submit_prompt_to(&harness.runtime, harness.session_id, "continue").await
        } else if cancel_parent {
            let queued = submit_prompt_to(&harness.runtime, harness.session_id, "continue").await;
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CancelRun { run_id: parent_run },
                )
                .await
                .unwrap();
            queued
        } else {
            steer(
                &harness.runtime,
                parent_run,
                "stop the child and continue",
                true,
            )
            .await
            .unwrap();
            parent_run
        };
        if deadline {
            // Both inherited child and parent deadlines must expire while the
            // write is held; neither terminal may release the checkout early.
            tokio::time::sleep(Duration::from_millis(1_200)).await;
        } else if interruption != 2 {
            tokio::time::timeout(Duration::from_secs(2), stopping.take().unwrap())
                .await
                .unwrap()
                .unwrap();
        }
        // The real write is past its cancellation check and holds the
        // apply lock. Completion must remain unavailable until it exits.
        let premature = tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                let event = harness.events.next().await.unwrap().unwrap();
                let stopped = event.run_id == Some(child_run)
                    && matches!(event.event, SessionEvent::RunFinished { .. });
                observed.push(event);
                if stopped {
                    break;
                }
            }
        })
        .await
        .is_ok();
        let requests_before_release = parent_requests.lock().unwrap().len();
        let shutdown_pending = shutdown
            .as_ref()
            .is_none_or(|shutdown| !shutdown.is_finished());
        release.send(()).unwrap();
        observed.extend(collect_until_run_finished(&mut harness.events, last_run).await);
        if let Some(shutdown) = shutdown {
            tokio::time::timeout(Duration::from_secs(2), shutdown)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            harness.runtime.shutdown().await.unwrap();
        }
        assert!(
            shutdown_pending,
            "shutdown cannot succeed before native cleanup exits"
        );
        assert!(
            !premature,
            "a child terminal event must not precede its native mutation teardown"
        );
        assert_eq!(
            requests_before_release, 1,
            "no parent or queued replacement may resume during child teardown"
        );
        let child_finished = observed
            .iter()
            .position(|event| {
                event.run_id == Some(child_run)
                    && matches!(event.event, SessionEvent::RunFinished { .. })
            })
            .unwrap();
        let parent_finished = observed
            .iter()
            .position(|event| {
                event.run_id == Some(parent_run)
                    && matches!(event.event, SessionEvent::RunFinished { .. })
            })
            .unwrap();
        assert!(child_finished < parent_finished);
        if deadline {
            assert!(matches!(finished_outcome(&observed, parent_run),
                Some(RunOutcome::BudgetExhausted { exhaustion }) if exhaustion.limit == BudgetLimitKind::Duration
            ));
        }
        assert_eq!(
            std::fs::read_to_string(harness._directory.path().join("child.txt")).unwrap(),
            "child"
        );
        if shutting_down {
            assert!(!harness._directory.path().join("parent.txt").exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(harness._directory.path().join("parent.txt")).unwrap(),
                "parent"
            );
        }
    }
}

#[tokio::test]
async fn interrupting_steering_owns_a_child_whose_creation_reply_is_pending() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let (mut harness, _) = write_child_harness(
        Arc::new(HangingProvider),
        vec![
            (
                "spawn_agent",
                r#"{"task":"wait","model":"test/child","authority":"write"}"#.to_owned(),
            ),
            (
                "write_file",
                r#"{"path":"parent.txt","content":"resumed"}"#.to_owned(),
            ),
        ],
        Some(reviewer),
        ApprovalMode::Full,
    )
    .await;
    let (created, release) = store::hold_child_creation(harness.session_id);
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let child_run = tokio::time::timeout(Duration::from_secs(2), created)
        .await
        .unwrap()
        .unwrap();
    steer(
        &harness.runtime,
        parent_run,
        "stop the child and continue",
        true,
    )
    .await
    .unwrap();
    let _ = release.send(());
    let mut observed = collect_until_run_finished(&mut harness.events, parent_run).await;
    harness.runtime.shutdown().await.unwrap();
    if finished_outcome(&observed, child_run).is_none() {
        observed.extend(collect_until_run_finished(&mut harness.events, child_run).await);
    }
    let finished_at = |run| {
        observed
            .iter()
            .position(|event| {
                event.run_id == Some(run) && matches!(event.event, SessionEvent::RunFinished { .. })
            })
            .unwrap()
    };
    assert!(
        finished_at(child_run) < finished_at(parent_run),
        "steering must own and stop an admitted child before the parent resumes"
    );
    assert_eq!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Cancelled)
    );
    assert_eq!(
        std::fs::read_to_string(harness._directory.path().join("parent.txt")).unwrap(),
        "resumed"
    );
}

#[tokio::test]
async fn store_saturation_does_not_release_a_running_write_child() {
    struct HeldChild {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl Provider for HeldChild {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async_stream! {
                entered.notify_one();
                release.notified().await;
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta { text: "child done".to_owned() });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }
    let entered = Arc::new(tokio::sync::Notify::new());
    let release_child = Arc::new(tokio::sync::Notify::new());
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let (mut harness, parent_requests) = write_child_harness(
        Arc::new(HeldChild {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release_child),
        }),
        vec![
            (
                "spawn_agent",
                r#"{"task":"hold the checkout","model":"test/child","authority":"write"}"#
                    .to_owned(),
            ),
            (
                "write_file",
                r#"{"path":"parent.txt","content":"parent resumed"}"#.to_owned(),
            ),
        ],
        Some(reviewer),
        ApprovalMode::Full,
    )
    .await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate then write").await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let mut observed = Vec::new();
    let (child_run, cursor) = loop {
        let event = tokio::time::timeout(Duration::from_secs(2), harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let child = matches!(event.event, SessionEvent::RunStarted { .. })
            && event.run_id != Some(parent_run);
        let run = event.run_id;
        let cursor = event.cursor;
        observed.push(event);
        if child {
            break (run.unwrap(), cursor);
        }
    };
    let (read_entered, release_read, read_attempted) = store::hold_outcome_read(child_run);
    harness.runtime.inner.notify(cursor);
    tokio::time::timeout(Duration::from_secs(2), read_entered)
        .await
        .unwrap()
        .unwrap();

    // Hold the real worker and fill its control lane. The child is live;
    // only the parent's outcome read is released into this saturation.
    let blocked_store = harness.runtime.inner.store.clone();
    let (worker_entered, worker_started) = oneshot::channel();
    let (release_worker, worker_release) = std::sync::mpsc::channel();
    let worker = tokio::spawn(async move {
        blocked_store
            .call(Priority::Output, move |_| {
                let _ = worker_entered.send(());
                worker_release
                    .recv()
                    .map_err(|_| SessionRuntimeError::Unavailable)
            })
            .await
    });
    worker_started.await.unwrap();
    let mut queued = Vec::new();
    for _ in 0..256 {
        let store = harness.runtime.inner.store.clone();
        let mut job = Box::pin(async move { store.call(Priority::Control, |_| Ok(())).await });
        assert!(futures_util::poll!(job.as_mut()).is_pending());
        queued.push(job);
    }
    release_read.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), read_attempted)
        .await
        .unwrap()
        .unwrap();
    release_worker.send(()).unwrap();
    worker.await.unwrap().unwrap();
    for job in queued {
        job.await.unwrap();
    }
    release_child.notify_one();
    observed.extend(collect_until_run_finished(&mut harness.events, parent_run).await);
    harness.runtime.shutdown().await.unwrap();
    let parent_reqs = parent_requests.lock().unwrap();
    assert!(
        matches!(
            parent_reqs[1].messages()[2].content(),
            [ContentBlock::ToolResult { content, is_error: false, .. }] if content == "child done"
        ),
        "store overload must not replace the live child's result with an admission error"
    );
    let child_finished = observed
        .iter()
        .position(|event| {
            event.run_id == Some(child_run)
                && matches!(event.event, SessionEvent::RunFinished { .. })
        })
        .unwrap();
    let parent_resumed = observed
        .iter()
        .position(|event| {
            event.run_id == Some(parent_run)
                && matches!(event.event, SessionEvent::ToolCallFinished { .. })
        })
        .unwrap();
    assert!(child_finished < parent_resumed);
    assert_eq!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Completed)
    );
    assert!(matches!(
        finished_outcome(&observed, parent_run),
        Some(RunOutcome::Completed)
    ));
    assert_eq!(
        std::fs::read_to_string(harness._directory.path().join("parent.txt")).unwrap(),
        "parent resumed"
    );
}

#[tokio::test]
async fn child_cancellation_persistence_failure_prevents_parent_continuation() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let (mut harness, requests) = write_child_harness(
        Arc::new(HangingProvider),
        vec![(
            "spawn_agent",
            r#"{"task":"wait","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        Some(reviewer),
        ApprovalMode::Full,
    )
    .await;
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let child_run = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if event.run_id != Some(parent_run)
                && matches!(event.event, SessionEvent::RunStarted { .. })
            {
                break event.run_id.unwrap();
            }
        }
    })
    .await
    .unwrap();
    store::fail_child_cancellation(child_run);
    let child_cancel = harness
        .runtime
        .inner
        .cancellations
        .lock()
        .unwrap()
        .get(&child_run)
        .unwrap()
        .subscribe();
    steer(&harness.runtime, parent_run, "continue", true)
        .await
        .unwrap();
    let unavailable = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = harness.events.next().await {
            match event {
                Err(SessionRuntimeError::Unavailable) => return true,
                Ok(event)
                    if event.run_id == Some(parent_run)
                        && matches!(event.event, SessionEvent::RunFinished { .. }) =>
                {
                    return false;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected event error: {error}"),
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(
        unavailable,
        "failed cancellation cannot become a resumable tool result"
    );
    assert!(
        *child_cancel.borrow(),
        "live execution is cancelled before persistence is attempted"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        steer(&harness.runtime, parent_run, "retry", true).await,
        Err(SessionRuntimeError::Unavailable)
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn unconfirmed_shell_exit_prevents_session_continuation() {
    let (mut harness, requests) = write_child_harness(
        Arc::new(StaticTextProvider),
        vec![("shell", crate::tools::PANIC_SHELL_ARGUMENTS.to_owned())],
        None,
        ApprovalMode::Full,
    )
    .await;
    let workspace = std::fs::canonicalize(harness._directory.path()).unwrap();
    let spawned = crate::tools::observe_shell_spawn(&workspace, true);
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "run a command").await;
    let pid = tokio::time::timeout(Duration::from_secs(2), spawned)
        .await
        .unwrap()
        .unwrap();
    let unavailable = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = harness.events.next().await {
            match event {
                Err(SessionRuntimeError::Unavailable) => return true,
                Ok(event)
                    if event.run_id == Some(run_id)
                        && matches!(event.event, SessionEvent::RunFinished { .. }) =>
                {
                    return false;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected event error: {error}"),
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(
        unavailable,
        "an unconfirmed reap must fail the session runtime closed"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    crate::tools::assert_panicked_process_exits(pid).await;
}

#[tokio::test]
async fn unreadable_child_outcome_fails_the_runtime_before_parent_continuation() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let (mut harness, parent_requests) = write_child_harness(
        Arc::new(HangingProvider),
        vec![(
            "spawn_agent",
            r#"{"task":"hold the checkout","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        Some(reviewer),
        ApprovalMode::Full,
    )
    .await;
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let (child_run, cursor) = loop {
        let event = tokio::time::timeout(Duration::from_secs(2), harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(event.event, SessionEvent::RunStarted { .. })
            && event.run_id != Some(parent_run)
        {
            break (event.run_id.unwrap(), event.cursor);
        }
    };
    let (read_entered, release_read, _read_attempted) = store::hold_outcome_read(child_run);
    harness.runtime.inner.notify(cursor);
    tokio::time::timeout(Duration::from_secs(2), read_entered)
        .await
        .unwrap()
        .unwrap();
    // Corrupt just the child accounting payload: the database still
    // accepts parent events, so an ordinary error result would resume it.
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET usage_json = 'invalid-json' WHERE id = ?1",
                [child_run.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let child_cancel = harness
        .runtime
        .inner
        .cancellations
        .lock()
        .unwrap()
        .get(&child_run)
        .unwrap()
        .subscribe();
    release_read.send(()).unwrap();
    let unavailable = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = harness.events.next().await {
            match event {
                Err(SessionRuntimeError::Unavailable) => return true,
                Ok(event)
                    if matches!(event.event, SessionEvent::RunFinished { .. })
                        && event.run_id == Some(parent_run) =>
                {
                    return false;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected event failure: {error}"),
            }
        }
        false
    })
    .await
    .unwrap();
    if !unavailable {
        // Clean up the still-running child on the failing implementation.
        harness.runtime.inner.cancel(child_run);
    }
    assert!(
        unavailable,
        "unreadable child ownership must fail closed, not return a resumable tool error"
    );
    assert!(
        *child_cancel.borrow(),
        "the live child receives cancellation"
    );
    assert_eq!(
        parent_requests.lock().unwrap().len(),
        1,
        "the parent makes no further provider request"
    );
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: parent_run }
            )
            .await,
        Err(SessionRuntimeError::Unavailable)
    );
}

#[tokio::test]
async fn write_children_serialize_on_the_per_run_write_permit() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let child: Arc<dyn Provider> = Arc::new(GaugedTextProvider {
        active: Arc::clone(&active),
        peak: Arc::clone(&peak),
    });
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::clone(&parent_requests),
        spawns: 3,
        arguments: |index| {
            format!(r#"{{"task":"task {index}","model":"test/child","authority":"write"}}"#)
        },
        turn: StdMutex::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"))
        .with_approval_reviewer(reviewer);
    options.max_active_runs = 8;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(WriteChildLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
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
    let run_id = submit_prompt_to(&runtime, session_id, "fan out writers").await;
    let observed = collect_until_run_finished(&mut events, run_id).await;

    let children = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::SessionCreated { session }
                if session.parent_id.is_some() && session.approval_mode == ApprovalMode::Supervised)
        })
        .count();
    assert_eq!(children, 3, "every writer eventually runs");
    assert_eq!(
        peak.load(Ordering::Acquire),
        1,
        "two writers never share the checkout"
    );
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_children_per_run_queue_behind_the_cap() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::clone(&parent_requests),
        spawns: usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1,
        arguments: |index| format!(r#"{{"task":"task {index}","model":"test/child"}}"#),
        turn: StdMutex::new(0),
    });
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let child: Arc<dyn Provider> = Arc::new(GaugedTextProvider {
        active: Arc::clone(&active),
        peak: Arc::clone(&peak),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCreated { .. }))
            .count(),
        usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1
    );
    assert!(peak.load(Ordering::Acquire) <= usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN));
    assert!(peak.load(Ordering::Acquire) >= 1);
    let parent_reqs = parent_requests.lock().unwrap();
    let results = parent_reqs[1].messages()[2].content();
    assert_eq!(
        results.len(),
        usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1
    );
    for block in results {
        assert!(matches!(
            block,
            ContentBlock::ToolResult { content, is_error: false, .. }
                if content == "child done"
        ));
    }
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn spawns_beyond_the_per_run_budget_return_a_tool_error() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::clone(&parent_requests),
        spawns: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN) + 1,
        arguments: |index| format!(r#"{{"task":"task {index}","model":"test/child"}}"#),
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![parent],
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out wide").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    // Exactly the budget's worth of children were created.
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCreated { .. }))
            .count(),
        usize::from(MAX_SPAWNED_CHILDREN_PER_RUN)
    );
    let parent_reqs = parent_requests.lock().unwrap();
    let results = parent_reqs[1].messages()[2].content();
    assert_eq!(results.len(), usize::from(MAX_SPAWNED_CHILDREN_PER_RUN) + 1);
    let errors = results
        .iter()
        .filter(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("already spawned")
            )
        })
        .count();
    let successes = results
        .iter()
        .filter(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, is_error: false, .. }
                    if content == "done"
            )
        })
        .count();
    assert_eq!(errors, 1);
    assert_eq!(successes, usize::from(MAX_SPAWNED_CHILDREN_PER_RUN));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn saturated_parents_awaiting_children_never_deadlock() {
    // Two parents fill the entire root permit pool and then both await a
    // child. If children drew from the same pool nothing could ever run
    // them; the separate child pool must let every run complete.
    let spawn_script = || -> Arc<dyn Provider> {
        Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "spawn_agent",
                r#"{"task":"shared research","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        })
    };
    let mut harness = spawn_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![spawn_script(), spawn_script()],
        2,
    )
    .await;
    let created = create_session(&harness.runtime, harness.workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id: second } = created.outcome else {
        panic!("unexpected receipt")
    };
    let first_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate one").await;
    let second_run = submit_prompt_to(&harness.runtime, second, "delegate two").await;

    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while finished_outcome(&observed, first_run).is_none()
            || finished_outcome(&observed, second_run).is_none()
        {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("saturated parents deadlocked instead of completing");
    assert!(matches!(
        finished_outcome(&observed, first_run),
        Some(RunOutcome::Completed)
    ));
    assert!(matches!(
        finished_outcome(&observed, second_run),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn nested_spend_reduces_the_next_root_child_allowance() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let root = submit_nested_budget(&harness, true).await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let later = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.model.as_deref() == Some("test/later-child") =>
            {
                Some(run)
            }
            _ => None,
        })
        .expect("the remaining budget can fund a later child");
    let limits = later.limits.as_ref().unwrap();
    // Two root turns, two child turns, and the 60-token grandchild.
    assert_eq!(limits.max_total_tokens, Some(36));
    assert_eq!(limits.max_cost_usd_nanos, Some(36_000));
    assert!(matches!(
        finished_outcome(&observed, root),
        Some(RunOutcome::Completed)
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_spend_session_deletion_waits_for_the_root_receipt() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let (delivered, release) = subagents::hold_child_delivery(harness.session_id);
    let root = submit_nested_budget(&harness, true).await;
    tokio::time::timeout(Duration::from_secs(2), delivered)
        .await
        .unwrap()
        .unwrap();
    let grandchild = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if let SessionEvent::SessionCreated { session } = event.event
                && session.model.as_deref() == Some("test/grandchild")
            {
                break session.id;
            }
        }
    })
    .await
    .unwrap();
    let deletion = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: grandchild,
            },
        )
        .await;
    assert_eq!(deletion, Err(SessionRuntimeError::SessionActive));
    release.send(()).unwrap();
    collect_until_run_finished(&mut harness.events, root).await;
    let deleted = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: grandchild,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(deleted.outcome, CommandOutcome::SessionDeleted { session_id } if session_id == grandchild)
    );
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_spend_session_deletion_checks_followup_owners_after_the_original_root_finished() {
    let parent = metered_delegation(
        vec![(
            "spawn_agent",
            r#"{"task":"initial task","model":"test/child"}"#.to_owned(),
        )],
        vec![Some(usage(1, 0))],
    );
    let mut harness = depth_harness(
        vec![
            (
                "test/child",
                metered_delegation(Vec::new(), vec![Some(usage(1, 0))]),
            ),
            (
                "test/followup",
                metered_delegation(
                    vec![(
                        "spawn_agent",
                        r#"{"task":"followup work","model":"test/grandchild"}"#.to_owned(),
                    )],
                    vec![Some(usage(1, 0))],
                ),
            ),
            (
                "test/grandchild",
                metered_delegation(Vec::new(), vec![Some(usage(10, 0))]),
            ),
        ],
        vec![parent],
        2,
        8,
    )
    .await;
    let root = submit_prompt_to(&harness.runtime, harness.session_id, "initial root").await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let child = observed
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
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetSessionModel {
                session_id: child,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/followup".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
            },
        )
        .await
        .unwrap();
    let (delivered, release) = subagents::hold_child_delivery(child);
    let followup = submit_prompt_to(&harness.runtime, child, "new unrelated task").await;
    tokio::time::timeout(Duration::from_secs(2), delivered)
        .await
        .unwrap()
        .unwrap();
    let grandchild = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if let SessionEvent::SessionCreated { session } = event.event
                && session.parent_id == Some(child)
            {
                break session.id;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession {
                    session_id: grandchild
                }
            )
            .await,
        Err(SessionRuntimeError::SessionActive),
    );
    release.send(()).unwrap();
    collect_until_run_finished(&mut harness.events, followup).await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: grandchild,
            },
        )
        .await
        .unwrap();
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_unknown_spend_prevents_a_later_root_child() {
    for cost_bound in [false, true] {
        let mut harness = nested_spend_harness(None).await;
        let root = submit_nested_budget(&harness, cost_bound).await;
        let observed = collect_until_run_finished(&mut harness.events, root).await;
        assert!(!observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::SessionCreated { session }
                if session.model.as_deref() == Some("test/later-child")
        )));
        assert_eq!(
            exhaustion_of(&observed, root).limit,
            if cost_bound {
                BudgetLimitKind::CostUnknown
            } else {
                BudgetLimitKind::TokensUnknown
            }
        );
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn nested_spend_receipt_excludes_later_prompts_in_owned_sessions() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let root = submit_nested_budget(&harness, true).await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let (session_id, delegated_run) = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.model.as_deref() == Some("test/child") =>
            {
                Some((session.id, run.id))
            }
            _ => None,
        })
        .unwrap();
    let followup = submit_prompt_to(&harness.runtime, session_id, "unrelated follow-up").await;
    collect_until_run_finished(&mut harness.events, followup).await;
    let (_, spend) = harness
        .runtime
        .inner
        .store
        .run_outcome(delegated_run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(spend.usage, Some(usage(62, 0)));
    assert_eq!(spend.cost_usd_nanos, Some(62_000));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_spend_receipt_rejects_incomplete_or_overflowing_descendants() {
    for corruption in ["missing_identity", "unfinished", "usage_overflow"] {
        let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
        let root = submit_nested_budget(&harness, true).await;
        let observed = collect_until_run_finished(&mut harness.events, root).await;
        let grandchild = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::PromptQueued { session, run, .. }
                    if session.model.as_deref() == Some("test/grandchild") =>
                {
                    Some(run.id)
                }
                _ => None,
            })
            .unwrap();
        harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let sql = match corruption {
                    "missing_identity" => "DELETE FROM messages WHERE run_id = ?1 AND ordinal = 1",
                    "unfinished" => {
                        "UPDATE runs SET outcome_json = NULL, status = 'running' WHERE id = ?1"
                    }
                    "usage_overflow" => "UPDATE runs SET usage_json = ?2 WHERE id = ?1",
                    _ => unreachable!(),
                };
                let changed = if corruption == "usage_overflow" {
                    connection.execute(
                        sql,
                        params![
                            grandchild.to_string(),
                            serde_json::to_string(&usage(u64::MAX, 0)).unwrap()
                        ],
                    )
                } else {
                    connection.execute(sql, [grandchild.to_string()])
                };
                changed
                    .map(|_| ())
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)
            })
            .await
            .unwrap();
        assert!(
            matches!(
                harness.runtime.inner.store.run_outcome(root).await,
                Err(SessionRuntimeError::AccountingUnavailable)
            ),
            "{corruption}"
        );
        // Restore the simulated unfinished row before ordinary shutdown.
        if corruption == "unfinished" {
            harness
                .runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    connection
                        .execute(
                            "UPDATE runs SET status = 'completed', outcome_json = ?2 WHERE id = ?1",
                            params![
                                grandchild.to_string(),
                                serde_json::to_string(&RunOutcome::Completed).unwrap()
                            ],
                        )
                        .map(|_| ())
                        .map_err(|_| SessionRuntimeError::CODEC)
                })
                .await
                .unwrap();
        }
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn nested_spend_receipt_distinguishes_never_started_from_unknown_cancelled_work() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let created = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "never starts".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
            },
        )
        .await
        .unwrap();
    store.cancel_child_run(created.run_id).await.unwrap();
    assert_eq!(
        store.run_outcome(created.run_id).await.unwrap(),
        Some((RunOutcome::Cancelled, SpawnAgentSpend::NONE))
    );
    store
        .finish_run(
            &parent,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.run_outcome(parent.identity.run_id).await.unwrap(),
        Some((RunOutcome::Cancelled, SpawnAgentSpend::UNKNOWN))
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn depth_two_lets_children_spawn_read_only_grandchildren_that_cannot_spawn() {
    let grandchild_requests = Arc::new(StdMutex::new(Vec::new()));
    let grandchild: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&grandchild_requests),
        // A grandchild guessing spawn_agent is refused at dispatch.
        script: vec![("spawn_agent", r#"{"task":"deeper still"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/child", delegating_child()),
            ("test/grandchild", grandchild),
        ],
        vec![parent],
        2,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let created = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::SessionCreated { session } if session.parent_id.is_some() => {
                Some(session.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(created.len(), 2, "child and grandchild");
    let child = created
        .iter()
        .find(|session| session.parent_id == Some(harness.session_id))
        .unwrap();
    let grandchild = created
        .iter()
        .find(|session| session.parent_id == Some(child.id))
        .unwrap();
    assert_eq!(child.spawned_by.as_ref().unwrap().depth, 1);
    assert_eq!(grandchild.spawned_by.as_ref().unwrap().depth, 2);
    assert_eq!(grandchild.approval_mode, ApprovalMode::ReadOnly);
    // The grandchild sits at the effective depth: no spawner, and its
    // guessed spawn call is refused at dispatch.
    let requests = grandchild_requests.lock().unwrap().clone();
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("deepest delegation level")
    ));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    // Inclusive accounting and the snapshot see the whole tree.
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 3);
}

#[tokio::test]
async fn depth_one_keeps_children_from_spawning_and_the_ceiling_is_enforced() {
    // Default max_depth = 1: the child has no spawner even though the
    // grandchild route is configured.
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"look deeper","model":"test/grandchild"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/child", child),
            ("test/grandchild", Arc::new(StaticTextProvider)),
        ],
        vec![parent],
        1,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let children = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::SessionCreated { session } if session.parent_id.is_some())
        })
        .count();
    assert_eq!(children, 1, "no grandchild at depth one");
    let requests = child_requests.lock().unwrap().clone();
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );

    // A publicly created session tree cannot exceed the runtime ceiling.
    let mut parent_id = harness.session_id;
    for _ in 0..MAX_CHILD_DEPTH {
        let created = create_session(&harness.runtime, harness.workspace_id, Some(parent_id)).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        parent_id = session_id;
    }
    let too_deep = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id: harness.workspace_id,
                parent_id: Some(parent_id),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await;
    assert!(matches!(
        too_deep,
        Err(SessionRuntimeError::ChildDepthExceeded)
    ));
}

#[tokio::test]
async fn max_depth_zero_disables_delegation_for_the_root_itself() {
    // The A0 control arm: the root is never offered spawn_agent, and a
    // guessed call is refused at dispatch without creating a child.
    let root_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&root_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![parent],
        0,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some()
    )));
    let requests = root_requests.lock().unwrap().clone();
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("deepest delegation level")
    ));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn a_tree_is_capped_at_the_descendant_limit() {
    // Each child spawns as many grandchildren as the per-run cap allows;
    // the tree still stops at MAX_DESCENDANTS_PER_ROOT sessions total.
    let fan_out: Arc<dyn Provider> = Arc::new(StatelessFanOut {
        spawns: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
        route: "test/leaf",
    });
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        spawns: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
        arguments: |index| format!(r#"{{"task":"branch {index}","model":"test/branch"}}"#),
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/branch", fan_out),
            ("test/leaf", Arc::new(StaticTextProvider)),
        ],
        vec![parent],
        2,
        16,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let created = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::SessionCreated { session } if session.parent_id.is_some())
        })
        .count();
    assert_eq!(
        created,
        usize::from(MAX_DESCENDANTS_PER_ROOT),
        "8 branches + 16 leaves, then refused"
    );
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn saturated_parents_at_every_depth_never_deadlock() {
    // Two roots fill the root pool and each spawns a child that spawns a
    // grandchild. Depth-one children fill their pool while awaiting
    // grandchildren; grandchildren must still run from their own pool.
    let spawn_child = || -> Arc<dyn Provider> {
        Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "spawn_agent",
                r#"{"task":"survey","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        })
    };
    let mut harness = depth_harness(
        vec![
            ("test/child", delegating_child()),
            ("test/grandchild", Arc::new(StaticTextProvider)),
        ],
        vec![spawn_child(), spawn_child()],
        2,
        2,
    )
    .await;
    let created = create_session(&harness.runtime, harness.workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id: second } = created.outcome else {
        panic!("unexpected receipt")
    };
    let first_run = submit_prompt_to(&harness.runtime, harness.session_id, "one").await;
    let second_run = submit_prompt_to(&harness.runtime, second, "two").await;
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while finished_outcome(&observed, first_run).is_none()
            || finished_outcome(&observed, second_run).is_none()
        {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("saturated parents at depth zero and one deadlocked");
    assert!(matches!(
        finished_outcome(&observed, first_run),
        Some(RunOutcome::Completed)
    ));
    assert!(matches!(
        finished_outcome(&observed, second_run),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn cancelling_the_root_settles_the_whole_subtree() {
    let child_started = Arc::new(tokio::sync::Notify::new());
    struct NotifyingHang {
        started: Arc<tokio::sync::Notify>,
    }
    impl Provider for NotifyingHang {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            self.started.notify_one();
            Box::pin(stream::pending())
        }
    }
    let grandchild: Arc<dyn Provider> = Arc::new(NotifyingHang {
        started: Arc::clone(&child_started),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/child", delegating_child()),
            ("test/grandchild", grandchild),
        ],
        vec![parent],
        2,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    tokio::time::timeout(Duration::from_secs(5), child_started.notified())
        .await
        .expect("the grandchild must start");
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    // Descendants settle in their own tasks; keep draining until both
    // child runs have finished.
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            observed.push(harness.events.next().await.unwrap().unwrap());
            let child_finishes = observed
                .iter()
                .filter(|event| {
                    matches!(&event.event, SessionEvent::RunFinished { session, .. } if session.parent_id.is_some())
                })
                .count();
            if child_finishes >= 2 && finished_outcome(&observed, run_id).is_some() {
                break;
            }
        }
    })
    .await
    .expect("every descendant settles after the root is cancelled");
    let finished = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunFinished {
                session, outcome, ..
            } => Some((session.parent_id.is_some(), outcome.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finished
            .iter()
            .filter(|(is_child, outcome)| *is_child && *outcome == RunOutcome::Cancelled)
            .count(),
        2,
        "child and grandchild both cancelled: {finished:?}"
    );
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Cancelled)
    ));
}

#[tokio::test]
async fn steering_during_an_audit_reaches_the_next_parent_request() {
    use tokio::sync::Notify;
    struct HeldFirstAuditor {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        calls: AtomicUsize,
    }

    impl Provider for HeldFirstAuditor {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async_stream::stream! {
                if first {
                    entered.notify_one();
                    release.notified().await;
                }
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: r#"{"verdict":"pass"}"#.to_owned(),
                });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }

    for interrupt in [true, false] {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&requests),
            script: Vec::new(),
            turn: StdMutex::new(0),
        });
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let auditor: Arc<dyn Provider> = Arc::new(HeldFirstAuditor {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            calls: AtomicUsize::new(0),
        });
        let mut harness =
            audit_harness(parent, auditor, crate::runtime::AuditMode::Always, 1).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "answer").await;
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let receipt = steer(&harness.runtime, run_id, "use the new direction", interrupt)
            .await
            .unwrap();
        let CommandOutcome::SteeringQueued { message_id, .. } = receipt.outcome else {
            panic!("steering must be queued");
        };
        if !interrupt {
            release.notify_one();
        }
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        harness.runtime.shutdown().await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "interrupt={interrupt}");
        assert!(
            request_texts(&requests[1])
                .iter()
                .any(|text| text.contains("use the new direction"))
        );
        assert!(observed.iter().any(|event| matches!(&event.event,
            SessionEvent::SteeringApplied { message_id: applied, .. } if *applied == message_id)));
        let child_outcomes: Vec<_> = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    session, outcome, ..
                } if session.parent_id == Some(harness.session_id) => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(child_outcomes.len(), 2);
        assert_eq!(
            *child_outcomes[0],
            if interrupt {
                RunOutcome::Cancelled
            } else {
                RunOutcome::Completed
            }
        );
        assert_eq!(*child_outcomes[1], RunOutcome::Completed);
        assert_eq!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        );
    }
}

#[tokio::test]
async fn a_mutating_run_is_audited_by_a_read_only_child_and_passes() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor_requests = Arc::new(StdMutex::new(Vec::new()));
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::clone(&auditor_requests),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write hello").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let (started, completed) = audit_events(&observed, run_id);
    let audit_session = started.expect("the audit child announces itself on the parent");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Pass);
    assert!(completed[0].findings.is_empty());
    assert_eq!(completed[0].revisions, 0);
    assert_eq!(completed[0].usage.map(|usage| usage.input_tokens), Some(50));
    // The audit child is a read-only session with purpose audit, at the
    // strong role's route, and its brief carries the prompt, answer, and
    // actions but not the transcript.
    let child = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session } if session.id == audit_session => {
                Some(session.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(child.purpose, SessionPurpose::Audit);
    assert_eq!(child.approval_mode, ApprovalMode::ReadOnly);
    assert_eq!(child.model.as_deref(), Some("test/auditor"));
    let brief = auditor_requests.lock().unwrap().clone();
    let text = request_texts(&brief[0]).join("\n");
    assert!(text.contains("User request:\nwrite hello"));
    assert!(text.contains("final answer"));
    assert!(text.contains("- write_file out.txt"));
    assert!(
        !brief[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "write_file")
    );
    // The parent completed with the audit ordered before RunFinished,
    // and the snapshot carries the record.
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let completed_at = observed
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::RunAuditCompleted { run_id: r, .. } if *r == run_id))
        .unwrap();
    let finished_at = observed
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::RunFinished { run_id: r, .. } if *r == run_id))
        .unwrap();
    assert!(completed_at < finished_at);
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let run = snapshot
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert_eq!(
        run.audit.as_deref().map(|audit| audit.outcome),
        Some(AuditOutcome::Pass)
    );
    // The audit child's own run carries its usage; the scripted parent
    // reports none, so the parent's total is unknown rather than partial.
    let child_run = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { session, usage, .. } if session.id == audit_session => {
                Some(*usage)
            }
            _ => None,
        })
        .flatten()
        .expect("the audit child reports usage");
    assert_eq!(child_run.input_tokens, 50);
    assert_eq!(run.usage, None);
    // The parent's own model saw no audit notice.
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert_eq!(parent_reqs.len(), 2);
}

#[tokio::test]
async fn the_contract_judges_the_audited_revision_and_a_revision_does_not_reset_repairs() {
    // Turn 1 mutates (audit trigger); turn 2 answers validly; the auditor
    // says revise; turn 3 (the revision) is invalid JSON; the contract
    // spends its one repair on turn 4, which validates. A second revise
    // cannot happen (max_revisions 1), so the order is
    // answer → audit → revision → repair.
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MutateThenScriptProvider {
        requests: Arc::clone(&parent_requests),
        answers: StdMutex::new(
            [
                r#"{"ok": true, "n": 1}"#,
                "revised, but prose",
                r#"{"ok": true, "n": 2}"#,
            ]
            .into_iter()
            .collect(),
        ),
        turn: AtomicUsize::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"revise","findings":["n is wrong"]}"#,
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("write hello and report")],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: Some(Box::new(report_contract(1))),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Revised);
    assert_eq!(
        finished_final_output(&observed, run_id).as_deref(),
        Some(&FinalOutput::Valid {
            value: serde_json::json!({"ok": true, "n": 2}),
            repair_turns: 1,
        })
    );
    {
        let requests = parent_requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "tool turn, answer, revision, repair");
        let revision = request_texts(&requests[2]).join("\n");
        assert!(
            revision.contains("independent read-only audit"),
            "{revision}"
        );
        let repair = request_texts(&requests[3]).join("\n");
        assert!(
            repair.contains(crate::output::OUTPUT_REPAIR_NOTICE),
            "{repair}"
        );
    }
    // The audit record and the verdict are both durable on the row.
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
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert!(snapshot.audit.is_some());
    assert!(matches!(
        snapshot.final_output.as_deref(),
        Some(FinalOutput::Valid {
            repair_turns: 1,
            ..
        })
    ));
}

#[tokio::test]
async fn a_revise_verdict_sends_the_run_back_once_and_the_revision_stands() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"revise","findings":["out.txt lacks a trailing newline","the answer claims tests ran"]}"#,
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write hello").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let (_, completed) = audit_events(&observed, run_id);
    // One audit, one revision; the revised answer is not re-audited when
    // the cap is reached.
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Revised);
    assert_eq!(completed[0].findings.len(), 2);
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert_eq!(parent_reqs.len(), 3, "tool turn, answer, revision");
    let revision_request = request_texts(&parent_reqs[2]).join("\n");
    assert!(revision_request.contains("independent read-only audit"));
    assert!(revision_request.contains("- out.txt lacks a trailing newline"));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    // The transcript shows two assistant answers.
    let turns = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::ModelTurnCompleted { run_id: r, .. } if *r == run_id)
        })
        .count();
    assert_eq!(turns, 3);
}

#[tokio::test]
async fn audit_triggers_and_suppressions_follow_the_heuristic() {
    // A read-only run under Heuristic is not audited ...
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let auditor_requests = Arc::new(StdMutex::new(Vec::new()));
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::clone(&auditor_requests),
    });
    let mut harness = audit_harness(
        parent,
        Arc::clone(&auditor),
        crate::runtime::AuditMode::Heuristic,
        1,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "read").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(audit_events(&observed, run_id).0.is_none());
    assert!(auditor_requests.lock().unwrap().is_empty());

    // ... but under Always it is; and Off never audits even a mutation.
    for (mode, mutate, expect_audit) in [
        (crate::runtime::AuditMode::Always, false, true),
        (crate::runtime::AuditMode::Off, true, false),
        (crate::runtime::AuditMode::Heuristic, true, true),
    ] {
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: if mutate {
                vec![(
                    "write_file",
                    r#"{"path":"out.txt","content":"x"}"#.to_owned(),
                )]
            } else {
                vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())]
            },
            turn: StdMutex::new(0),
        });
        let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
            reply: r#"{"verdict":"pass"}"#,
            requests: Arc::new(StdMutex::new(Vec::new())),
        });
        let mut harness = audit_harness(parent, auditor, mode, 1).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        assert_eq!(
            audit_events(&observed, run_id).0.is_some(),
            expect_audit,
            "{mode:?} mutate={mutate}"
        );
    }

    // A child run is never audited, even when it mutates under Always.
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"look","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = spawn_harness_with_loader(
        Arc::new(AuditLoader {
            inner: QueueLoader {
                routed: vec![("test/auditor", auditor), ("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
            mode: crate::runtime::AuditMode::Always,
            max_revisions: 1,
        }),
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let audits = observed
        .iter()
        .filter(|event| matches!(&event.event, SessionEvent::RunAuditStarted { .. }))
        .count();
    assert_eq!(
        audits, 1,
        "only the root is audited, and spawning triggers it"
    );
}

#[tokio::test]
async fn an_unavailable_auditor_fails_open_and_is_recorded() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    // The auditor answers prose, not a verdict.
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: "Looks fine to me!",
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Always, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Unavailable);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));

    // A failing audit child is also fail-open.
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = audit_harness(
        parent,
        Arc::new(RefusalProvider),
        crate::runtime::AuditMode::Always,
        1,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Unavailable);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn sequential_children_receive_the_remaining_budget_after_prior_spend() {
    let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(MultiSpawnProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            spawns: 2,
            arguments: |_| {
                r#"{"task":"research","model":"test/child","authority":"write"}"#.to_owned()
            },
            turn: StdMutex::new(0),
        }),
        usage: Some(usage(10, 5)),
    });
    let child: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(StaticTextProvider),
        usage: Some(usage(30, 10)),
    });
    let mut harness =
        child_budget_harness(parent, child, true, crate::runtime::AuditMode::Off).await;
    let run_id = submit_child_budget_prompt(
        &harness,
        RunLimits {
            max_total_tokens: Some(200),
            max_input_tokens: Some(150),
            max_output_tokens: Some(100),
            max_cost_usd_nanos: Some(300_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    harness.runtime.shutdown().await.unwrap();
    let admitted: Vec<_> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                run.limits.as_deref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0].max_total_tokens, Some(185));
    assert_eq!(admitted[0].max_cost_usd_nanos, Some(280_000));
    assert_eq!(admitted[1].max_total_tokens, Some(145));
    assert_eq!(admitted[1].max_input_tokens, Some(110));
    assert_eq!(admitted[1].max_output_tokens, Some(85));
    assert_eq!(admitted[1].max_cost_usd_nanos, Some(190_000));
    assert_eq!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    );
}

#[tokio::test]
async fn budgeted_read_children_refuse_exhausted_or_unknown_remainders() {
    for (limits, child_usage, family) in [
        (
            RunLimits {
                max_total_tokens: Some(40),
                ..RunLimits::default()
            },
            Some(usage(30, 10)),
            "total_tokens",
        ),
        (
            RunLimits {
                max_input_tokens: Some(30),
                ..RunLimits::default()
            },
            Some(usage(30, 10)),
            "input_tokens",
        ),
        (
            RunLimits {
                max_output_tokens: Some(10),
                ..RunLimits::default()
            },
            Some(usage(30, 10)),
            "output_tokens",
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(90_000),
                ..RunLimits::default()
            },
            Some(usage(30, 10)),
            "cost",
        ),
        (
            RunLimits {
                max_total_tokens: Some(100),
                ..RunLimits::default()
            },
            None,
            "tokens_unknown",
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(100_000),
                ..RunLimits::default()
            },
            None,
            "cost_unknown",
        ),
    ] {
        for concurrency in [1, MAX_CONCURRENT_CHILDREN_PER_RUN] {
            let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
                inner: Arc::new(MultiSpawnProvider {
                    requests: Arc::new(StdMutex::new(Vec::new())),
                    spawns: 2,
                    arguments: |_| r#"{"task":"research","model":"test/child"}"#.to_owned(),
                    turn: StdMutex::new(0),
                }),
                usage: Some(usage(0, 0)),
            });
            let child: Arc<dyn Provider> = Arc::new(UsageProvider {
                inner: Arc::new(StaticTextProvider),
                usage: child_usage,
            });
            let mut harness =
                child_budget_harness(parent, child, false, crate::runtime::AuditMode::Off).await;
            let run_id = submit_child_budget_prompt(
                &harness,
                RunLimits {
                    max_concurrent_children: Some(concurrency),
                    ..limits
                },
            )
            .await;
            let observed = collect_until_run_finished(&mut harness.events, run_id).await;
            harness.runtime.shutdown().await.unwrap();
            let children = observed.iter().filter(|event| matches!(&event.event,
                SessionEvent::SessionCreated { session } if session.parent_id == Some(harness.session_id))).count();
            assert_eq!(children, 1, "{family}, concurrency={concurrency}");
            assert!(observed.iter().any(|event| matches!(&event.event,
                SessionEvent::ToolCallFinished { tool_call, .. } if tool_call.run_id == run_id && tool_call.is_error
                    && tool_call.result.as_deref().is_some_and(|result| result.contains("cannot afford a sub-agent") && result.contains(family)))));
            if child_usage.is_none() {
                assert_eq!(exhaustion_of(&observed, run_id).limit.as_str(), family);
            } else {
                assert_eq!(
                    finished_outcome(&observed, run_id),
                    Some(RunOutcome::Completed),
                    "exact spend leaves no allowance for another child but may complete"
                );
            }
        }
    }
}

#[tokio::test]
async fn audits_inherit_remaining_limits_and_charge_inclusive_spend_once() {
    let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(StaticTextProvider),
        usage: Some(usage(10, 0)),
    });
    let auditor: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(VerdictProvider {
            reply: r#"{"verdict":"pass"}"#,
            requests: Arc::new(StdMutex::new(Vec::new())),
        }),
        usage: Some(usage(5, 0)),
    });
    let mut harness =
        child_budget_harness(parent, auditor, false, crate::runtime::AuditMode::Always).await;
    let run_id = submit_child_budget_prompt(
        &harness,
        RunLimits {
            max_total_tokens: Some(100),
            max_cost_usd_nanos: Some(100_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    harness.runtime.shutdown().await.unwrap();
    let admitted = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.purpose == SessionPurpose::Audit =>
            {
                Some(run)
            }
            _ => None,
        })
        .expect("auditor was admitted");
    let limits = admitted
        .limits
        .as_deref()
        .expect("auditor must inherit a budget");
    assert_eq!(limits.max_total_tokens, Some(90));
    assert_eq!(limits.max_cost_usd_nanos, Some(90_000));
    // The auditor is bounded on its own as well: a turn cap and a deadline
    // the parent did not impose, so an audit is never a second open-ended run.
    assert_eq!(
        limits.max_model_turns,
        Some(crate::runtime::MAX_AUDIT_CHILD_TURNS)
    );
    assert!(
        limits
            .max_duration_ms
            .is_some_and(|ms| ms <= crate::runtime::MAX_AUDIT_CHILD_DURATION_MS),
        "{:?}",
        limits.max_duration_ms
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 8,
        })
        .await
        .unwrap();
    let totals = snapshot
        .sessions
        .iter()
        .find(|session| session.id == harness.session_id)
        .unwrap()
        .accounting
        .unwrap();
    assert_eq!(totals.direct.usage, Some(usage(10, 0)));
    assert_eq!(totals.direct.estimated_cost_usd_nanos, Some(10_000));
    assert_eq!(totals.inclusive.usage, Some(usage(15, 0)));
    assert_eq!(totals.inclusive.estimated_cost_usd_nanos, Some(20_000));
    assert_eq!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    );
}

#[tokio::test]
async fn audit_overspend_and_unknown_spend_exhaust_the_parent() {
    for (limits, auditor_usage, expected) in [
        (
            RunLimits {
                max_total_tokens: Some(100),
                ..RunLimits::default()
            },
            Some(usage(95, 0)),
            BudgetLimitKind::TotalTokens,
        ),
        (
            RunLimits {
                max_input_tokens: Some(100),
                ..RunLimits::default()
            },
            Some(usage(95, 0)),
            BudgetLimitKind::InputTokens,
        ),
        (
            RunLimits {
                max_output_tokens: Some(100),
                ..RunLimits::default()
            },
            Some(usage(0, 96)),
            BudgetLimitKind::OutputTokens,
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(100_000),
                ..RunLimits::default()
            },
            Some(usage(45, 0)),
            BudgetLimitKind::Cost,
        ),
        (
            RunLimits {
                max_total_tokens: Some(100),
                ..RunLimits::default()
            },
            None,
            BudgetLimitKind::TokensUnknown,
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(100_000),
                ..RunLimits::default()
            },
            None,
            BudgetLimitKind::CostUnknown,
        ),
    ] {
        let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(StaticTextProvider),
            usage: Some(usage(10, 5)),
        });
        let auditor: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(VerdictProvider {
                reply: r#"{"verdict":"pass"}"#,
                requests: Arc::new(StdMutex::new(Vec::new())),
            }),
            usage: auditor_usage,
        });
        let mut harness =
            child_budget_harness(parent, auditor, false, crate::runtime::AuditMode::Always).await;
        let run_id = submit_child_budget_prompt(&harness, limits).await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        harness.runtime.shutdown().await.unwrap();
        assert_eq!(exhaustion_of(&observed, run_id).limit, expected);
        let (_, audits) = audit_events(&observed, run_id);
        assert_eq!(
            audits.len(),
            1,
            "the audit receipt is durable before exhaustion"
        );
    }
}

#[tokio::test]
async fn child_duration_is_reduced_by_preflight_and_prior_children() {
    struct HeldPreflight {
        inner: ChildBudgetLoader,
        first: AtomicBool,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl RuntimeLoader for HeldPreflight {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let hold = request.model.model.as_deref() == Some("test/child")
                && !self.first.swap(true, Ordering::SeqCst);
            let loaded = self.inner.load(request);
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                if hold {
                    entered.notify_one();
                    release.notified().await;
                }
                loaded.await
            })
        }
    }
    for expires_during_preflight in [false, true] {
        let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(MultiSpawnProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                spawns: 2,
                arguments: |_| r#"{"task":"research","model":"test/child"}"#.to_owned(),
                turn: StdMutex::new(0),
            }),
            usage: Some(usage(0, 0)),
        });
        let child: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(StaticTextProvider),
            usage: Some(usage(0, 0)),
        });
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut harness = spawn_harness_with_loader(
            Arc::new(HeldPreflight {
                inner: ChildBudgetLoader {
                    inner: QueueLoader {
                        routed: vec![("test/child", child)],
                        queue: StdMutex::new(vec![parent]),
                    },
                    write_children: false,
                    audit: crate::runtime::AuditMode::Off,
                },
                first: AtomicBool::new(false),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
            8,
        )
        .await;
        let run_id = submit_child_budget_prompt(
            &harness,
            RunLimits {
                max_total_tokens: Some(1000),
                max_duration_ms: Some(if expires_during_preflight { 500 } else { 5000 }),
                ..RunLimits::default()
            },
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(if expires_during_preflight {
            600
        } else {
            200
        }))
        .await;
        release.notify_one();
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        harness.runtime.shutdown().await.unwrap();
        let durations: Vec<_> = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::PromptQueued { session, run, .. }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    run.limits
                        .as_ref()
                        .and_then(|limits| limits.max_duration_ms)
                }
                _ => None,
            })
            .collect();
        if expires_during_preflight {
            assert!(
                durations.is_empty(),
                "expired preparation must not create a child"
            );
            assert_eq!(
                exhaustion_of(&observed, run_id).limit,
                BudgetLimitKind::Duration
            );
            continue;
        }
        assert_eq!(durations.len(), 2);
        assert!(
            durations[0] <= 4800,
            "preflight time cannot restart the child's duration: {durations:?}"
        );
        assert!(
            durations[1] < durations[0],
            "later children inherit the remaining clock: {durations:?}"
        );
    }
}
