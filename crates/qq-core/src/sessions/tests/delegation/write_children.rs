use super::*;

#[tokio::test]
async fn an_auto_child_denies_writes_beneath_an_ask_parent_without_prompting() {
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"child.txt","content":"approved"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"write child.txt","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::Ask,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let (_, spawn) = collect_until_approval_requested(&mut harness.events).await;
    assert_eq!(spawn.name, "spawn_agent");
    respond_approval(
        &harness.runtime,
        run_id,
        spawn.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::SessionCreated { session }
            if session.parent_id == Some(harness.session_id)
                && session.approval_mode == ApprovalMode::Auto
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(&event.event, SessionEvent::ToolApprovalRequested { .. })),
        "Auto child must not inherit a human approval wait"
    );
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "write_file" && tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref().is_some_and(|result| result == approval::PARENT_DENIED_RESULT)
    )));
    assert!(!observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallStarted { tool_call } if tool_call.name == "write_file"
    )));
    assert!(!harness.directory.path().join("child.txt").exists());
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_write_child_cannot_run_a_fixed_stop() {
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "__test_shell",
            r#"{"command":"git push --force origin main"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"publish work","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::Auto,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event, SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "__test_shell" && tool_call.state == ToolCallState::Denied
                    && tool_call.result.as_deref().is_some_and(|text| text.contains("fixed_stop"))
        )),
        "child call must be durably denied: {observed:?}"
    );
    assert!(!observed.iter().any(|event| matches!(
        &event.event, SessionEvent::ToolCallStarted { tool_call }
            if tool_call.name == "__test_shell"
    )));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_write_child_executes_authorized_edits() {
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
    assert_eq!(child_session.approval_mode, ApprovalMode::Auto);
    assert!(!observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolApprovalRequested { .. } | SessionEvent::ToolApprovalResolved { .. }
    )));
    assert_eq!(
        std::fs::read_to_string(harness.directory.path().join("child.txt")).unwrap(),
        "written by the child"
    );
    let child_reqs = child_requests.lock().unwrap().clone();
    assert!(
        child_reqs[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "write_file")
    );
    let child_system = child_reqs[0].system().unwrap();
    assert!(child_system.contains("\n\nSub-agent:\n"), "{child_system}");
    assert!(child_system.contains("- Do the work."));
    assert!(!child_system.contains("You cannot change files or run commands"));
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
async fn write_children_require_the_roster_flag() {
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
    let options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    let SessionFixture {
        directory: _directory,
        runtime,
        session_id,
        mut events,
        ..
    } = SessionFixture::open_in(
        directory,
        options,
        Arc::new(QueueLoader {
            routed: vec![("test/child", child)],
            queue: StdMutex::new(vec![parent]),
        }),
        ApprovalMode::default(),
    )
    .await;
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
    runtime.shutdown().await.unwrap();

    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let (mut harness, parent_requests) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::Auto,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some()
    )));
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert!(matches!(
        parent_reqs[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: false, .. }]
            if content == "done"
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_write_spawn_is_a_mutating_call_under_the_parents_policy() {
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let (mut harness, parent_requests) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::ReadOnly,
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
            if content == approval::POLICY_DENIED_RESULT
    ));

    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"t","model":"test/child","authority":"write"}"#.to_owned(),
        )],
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
            if session.parent_id.is_some() && session.approval_mode == ApprovalMode::Auto
    )));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn write_children_serialize_on_the_per_run_write_permit() {
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
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 8;
    let SessionFixture {
        directory: _directory,
        runtime,
        session_id,
        mut events,
        ..
    } = SessionFixture::open_in(
        directory,
        options,
        Arc::new(WriteChildLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
        }),
        ApprovalMode::default(),
    )
    .await;
    let run_id = submit_prompt_to(&runtime, session_id, "fan out writers").await;
    let observed = collect_until_run_finished(&mut events, run_id).await;

    let children = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::SessionCreated { session }
                if session.parent_id.is_some() && session.approval_mode == ApprovalMode::Auto)
        })
        .count();
    assert_eq!(children, 3, "every writer eventually runs");
    assert_eq!(
        peak.load(Ordering::Acquire),
        1,
        "two writers never share the checkout"
    );
    let delivery_rows: u32 = runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM child_deliveries", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(delivery_rows, 0);
    assert!(delivered_answers(&parent_requests.lock().unwrap()).is_empty());
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_ask_parent_cannot_widen_its_read_child_to_human_approved_writes() {
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"read-child.txt","content":"must not write"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"inspect only","model":"test/child","authority":"read"}"#.to_owned(),
        )],
        ApprovalMode::Ask,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate a read").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event, SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "write_file" && tool_call.state == ToolCallState::Denied
        )),
        "the child's ReadOnly ceiling must deny the write: {observed:?}"
    );
    assert!(
        !observed
            .iter()
            .any(|event| matches!(&event.event, SessionEvent::ToolApprovalRequested { .. })),
        "the child's denied write must never become approvable"
    );
    assert!(!harness.directory.path().join("read-child.txt").exists());
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn parent_readonly_revocation_denies_a_late_client_child_approval() {
    let (release_child, pending_child) = oneshot::channel();
    let child: Arc<dyn Provider> = Arc::new(approvals::GatedApprovalProvider {
        inner: ApprovalProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            turn: StdMutex::new(0),
            tool: "write_file",
            arguments: r#"{"path":"human-revoked.txt","content":"must not write"}"#,
            tool_turns: 1,
            usage: None,
        },
        release: StdMutex::new(Some(pending_child)),
    });
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"write human-revoked.txt","model":"test/child","authority":"write"}"#
                .to_owned(),
        )],
        ApprovalMode::Full,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let child_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if let SessionEvent::SessionCreated { session } = event.event
                && session.parent_id == Some(harness.session_id)
            {
                break session.id;
            }
        }
    })
    .await
    .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: child_id,
                mode: ApprovalMode::Ask,
            },
        )
        .await
        .unwrap();
    release_child.send(()).unwrap();
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    assert_eq!(call.name, "write_file");
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: harness.session_id,
                mode: ApprovalMode::ReadOnly,
            },
        )
        .await
        .unwrap();
    let receipt = respond_approval(
        &harness.runtime,
        call.run_id,
        call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::Denied,
            ..
        }
    ));
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!harness.directory.path().join("human-revoked.txt").exists());
    assert!(!observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallStarted { tool_call } if tool_call.id == call.id)));

    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_write_requests_preserve_the_root_only_write_boundary() {
    let grandchild: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"nested.txt","content":"reviewed"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"write nested.txt","model":"test/grandchild","authority":"write"}"#
                .to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"delegate the write","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 8;
    let SessionFixture {
        directory,
        runtime,
        session_id,
        mut events,
        ..
    } = SessionFixture::open_in(
        directory,
        options,
        Arc::new(DepthLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child), ("test/grandchild", grandchild)],
                queue: StdMutex::new(vec![parent]),
            },
            max_depth: 2,
            write_children: true,
            pricing: None,
        }),
        ApprovalMode::Auto,
    )
    .await;
    let run = submit_prompt_to(&runtime, session_id, "delegate twice").await;
    let observed = collect_until_run_finished(&mut events, run).await;
    assert!(!directory.path().join("nested.txt").exists());
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.event,
                SessionEvent::SessionCreated { session } if session.parent_id.is_some()
            ))
            .count(),
        1,
        "no write grandchild is admitted"
    );
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "spawn_agent"
            && tool_call.is_error
            && tool_call.result.as_deref().is_some_and(|text| text.contains("write sub-agents are not enabled"))
    )));
    runtime.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_late_child_approval_runs_once_after_the_parent_switches_to_auto() {
    {
        for decision in [ApprovalDecision::ApproveOnce] {
            let (release_child, pending_child) = oneshot::channel();
            let child: Arc<dyn Provider> = Arc::new(approvals::GatedApprovalProvider {
                inner: ApprovalProvider {
                    requests: Arc::new(StdMutex::new(Vec::new())),
                    turn: StdMutex::new(0),
                    tool: "shell",
                    arguments: r#"{"command":"rm -r approval-scratch"}"#,
                    tool_turns: 1,
                    usage: None,
                },
                release: StdMutex::new(Some(pending_child)),
            });
            let spawn = serde_json::json!({"task": "remove approval-scratch", "model": "test/child", "authority": "write"});
            let (mut harness, _) = write_child_harness(
                child,
                vec![("spawn_agent", spawn.to_string())],
                ApprovalMode::Full,
            )
            .await;
            std::fs::create_dir(harness.directory.path().join("approval-scratch")).unwrap();
            let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
            let child_id = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let event = harness.events.next().await.unwrap().unwrap();
                    if let SessionEvent::SessionCreated { session } = event.event
                        && session.parent_id == Some(harness.session_id)
                    {
                        break session.id;
                    }
                }
            })
            .await
            .unwrap();
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SetApprovalMode {
                        session_id: child_id,
                        mode: ApprovalMode::Ask,
                    },
                )
                .await
                .unwrap();
            release_child.send(()).unwrap();
            let (_, call) = collect_until_approval_requested(&mut harness.events).await;
            assert_eq!(call.name, "shell");
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SetApprovalMode {
                        session_id: harness.session_id,
                        mode: ApprovalMode::Auto,
                    },
                )
                .await
                .unwrap();
            let expected = ApprovalResolution::ApprovedOnce;
            let receipt = respond_approval(&harness.runtime, call.run_id, call.id, decision)
                .await
                .unwrap();
            assert!(
                matches!(receipt.outcome,
            CommandOutcome::ToolApprovalResolved { resolution, .. } if resolution == expected),
                "a child approval must respect the current parent's mode: {receipt:?}"
            );
            collect_until_run_finished(&mut harness.events, run_id).await;
            assert!(!harness.directory.path().join("approval-scratch").exists());
            harness.runtime.shutdown().await.unwrap();
        }
    }
}
