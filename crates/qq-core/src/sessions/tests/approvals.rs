use super::*;

#[tokio::test]
async fn immediate_approval_response_cannot_race_past_registered_waiter() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
    );
    assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);

    let receipt = respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    assert_eq!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            tool_call_id: tool_call.id,
            resolution: ApprovalResolution::ApprovedOnce,
        }
    );

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(matches!(
        &observed[0].event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    ));
    assert!(
        observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
                && tool_call.result.as_deref() == Some("mutated")
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let requests = harness.requests.lock().unwrap();
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult {
            content,
            is_error: false,
            ..
        }] if content == "mutated"
    ));
}

#[tokio::test]
async fn denial_returns_a_tool_error_and_the_run_still_completes() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::Denied,
        } if tool_call.state == ToolCallState::Denied && tool_call.is_error
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
    );
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let denied_result = {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        match requests[1].messages()[2].content() {
            [
                ContentBlock::ToolResult {
                    content,
                    is_error: true,
                    ..
                },
            ] => content.clone(),
            other => panic!("unexpected tool result content {other:?}"),
        }
    };
    assert_eq!(denied_result, approval::USER_DENIED_RESULT);
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
    assert_eq!(
        snapshot.focused.unwrap().tool_calls[0].state,
        ToolCallState::Denied
    );
}

#[tokio::test]
async fn responding_twice_returns_the_recorded_outcome_without_side_effects() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    let _ = collect_through_finished(&mut harness.events).await;

    // A retry with a different decision returns the recorded denial.
    let retry = respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    assert_eq!(
        retry.outcome,
        CommandOutcome::ToolApprovalResolved {
            tool_call_id: tool_call.id,
            resolution: ApprovalResolution::Denied,
        }
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
    assert_eq!(
        snapshot.focused.unwrap().tool_calls[0].state,
        ToolCallState::Denied
    );

    assert_eq!(
        respond_approval(
            &harness.runtime,
            harness.run_id,
            ToolCallId::generate().unwrap(),
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap_err(),
        SessionRuntimeError::ToolCallNotFound
    );
}

#[tokio::test]
async fn unresolved_approvals_are_denied_by_timeout_with_a_distinct_error() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        Duration::from_millis(50),
    )
    .await;
    let (_, _) = collect_until_approval_requested(&mut harness.events).await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        } if tool_call.state == ToolCallState::Denied
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let requests = harness.requests.lock().unwrap();
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult {
            content,
            is_error: true,
            ..
        }] if content == approval::TIMEOUT_DENIED_RESULT
    ));
}

#[tokio::test]
async fn approve_for_session_grants_cover_later_calls_without_prompting() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        2,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "the session grant must cover the second call"
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                &event.event,
                SessionEvent::ToolCallFinished { tool_call }
                    if tool_call.state == ToolCallState::Completed
            ))
            .count(),
        2
    );
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn an_oversized_session_grant_approves_the_call_once_instead_of_failing() {
    // A session choice used to fail the whole command with "approval grant is
    // empty or exceeds the session limit" when the value was empty or past
    // MAX_GRANT_BYTES, leaving the call awaiting after the user had approved
    // it. The call must run, and nothing must be recorded.
    let command = format!("echo {}", "x".repeat(MAX_GRANT_BYTES));
    let arguments = serde_json::json!({"command": command}).to_string();
    let mut harness = scripted_runs_harness(
        ApprovalMode::Ask,
        vec![vec![("__test_shell", arguments.clone())]],
    )
    .await;
    let run_id = submit_prompt(&harness, "run the long command").await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;

    let receipt = respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::ShellPrefix { prefix: command },
        },
    )
    .await
    .unwrap();
    assert_eq!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            tool_call_id: tool_call.id,
            resolution: ApprovalResolution::ApprovedOnce,
        }
    );
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
    )));
    let (_, grants) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert!(
        grants.shell_prefixes.is_empty(),
        "a grant past the byte cap must not be stored"
    );

    // A session already at the grant cap is the other half of the old error.
    // The call is approved once; the table does not grow.
    let mut full = scripted_runs_harness(
        ApprovalMode::Ask,
        vec![vec![("__test_mutate", "{}".to_owned())]],
    )
    .await;
    let full_session = full.session_id;
    full.runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            for index in 0..MAX_SESSION_GRANTS {
                connection.execute(
                    "INSERT INTO session_grants(session_id, kind, value, created_at_ms)
                     VALUES (?1, 'tool', ?2, 0)",
                    params![full_session.to_string(), format!("tool-{index}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let run_id = submit_prompt(&full, "mutate").await;
    let (_, tool_call) = collect_until_approval_requested(&mut full.events).await;
    let receipt = respond_approval(
        &full.runtime,
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
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    ));
    let (_, grants) = full
        .runtime
        .inner
        .store
        .approval_policy(full.session_id)
        .await
        .unwrap();
    assert_eq!(
        u32::try_from(grants.tools.len()).unwrap(),
        MAX_SESSION_GRANTS
    );
    collect_through_finished(&mut full.events).await;

    // An empty grant is the same class of request: approve once, record nothing.
    let mut empty = scripted_runs_harness(
        ApprovalMode::Ask,
        vec![vec![("__test_mutate", "{}".to_owned())]],
    )
    .await;
    let run_id = submit_prompt(&empty, "mutate").await;
    let (_, tool_call) = collect_until_approval_requested(&mut empty.events).await;
    let receipt = respond_approval(
        &empty.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "   ".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    ));
    let observed = collect_through_finished(&mut empty.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::WorkspaceGrantPromoted { .. })),
        "a grant that was not recorded must not be promoted"
    );
}

#[tokio::test]
async fn config_grants_seed_new_sessions_and_cover_calls_without_prompting() {
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed {
            tools: vec!["mcp__notes__search".to_owned()],
            shell_prefixes: vec!["cargo test".to_owned()],
            hosts: Vec::new(),
        },
        WorkspaceGrantOutcome::Failed {
            message: "unused".to_owned(),
        },
    );
    let mut harness = scripted_runs_harness_with_authority(
        ApprovalMode::Ask,
        vec![vec![
            (
                "__test_shell",
                r#"{"command":"cargo test -p qq-core"}"#.to_owned(),
            ),
            ("mcp__notes__search", "{}".to_owned()),
        ]],
        Some(authority.clone()),
    )
    .await;
    submit_prompt(&harness, "run both granted tools").await;

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "config-seeded grants must cover both calls without prompting"
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "__test_shell"
                && tool_call.state == ToolCallState::Completed
    )));
    // The exact-name MCP grant passed the gate; with no registry attached
    // the dispatch then fails, but the call was never held for approval.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "mcp__notes__search"
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let seeded = authority.seeded.lock().unwrap();
    assert_eq!(seeded.len(), 1, "one session creation resolves one seed");
    assert_eq!(
        seeded[0],
        std::fs::canonicalize(&harness.workspace_path).unwrap()
    );
}

#[tokio::test]
async fn approve_for_workspace_records_the_session_grant_and_promotes_it() {
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed::default(),
        WorkspaceGrantOutcome::Written {
            path: "/w/.qq/config.ron".to_owned(),
        },
    );
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        2,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority.clone()),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::RespondToolApproval {
        run_id: harness.run_id,
        tool_call_id: tool_call.id,
        decision: ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    };
    let receipt = harness
        .runtime
        .command(command_id, command.clone())
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedForWorkspace,
            ..
        }
    ));

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "the recorded session grant must cover the second call"
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                &event.event,
                SessionEvent::ToolCallFinished { tool_call }
                    if tool_call.state == ToolCallState::Completed
            ))
            .count(),
        2
    );
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));

    let promoted = grant_promotion_event(&observed, &mut harness.events).await;
    assert_eq!(promoted.caused_by, Some(command_id));
    assert_eq!(promoted.run_id, Some(harness.run_id));
    assert!(matches!(
        &promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            grant: ApprovalGrant::Tool { name },
            outcome: WorkspaceGrantOutcome::Written { path },
        } if name == "__test_mutate" && path == "/w/.qq/config.ron"
    ));
    {
        let promotions = authority.promotions.lock().unwrap();
        assert_eq!(promotions.len(), 1);
        assert_eq!(
            promotions[0].0,
            std::fs::canonicalize(harness._directory.path()).unwrap()
        );
    }

    // Retrying the same command replays the durable receipt without
    // re-running the promotion.
    let retried = harness.runtime.command(command_id, command).await.unwrap();
    assert_eq!(retried, receipt);
    assert_eq!(authority.promotions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancelled_command_future_cannot_lose_a_committed_promotion_wake() {
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed::default(),
        WorkspaceGrantOutcome::Written {
            path: "/w/.qq/config.ron".to_owned(),
        },
    );
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority.clone()),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::RespondToolApproval {
        run_id: harness.run_id,
        tool_call_id: tool_call.id,
        decision: ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    };
    let (committed, release) = store::hold_committed_command(command_id);
    let runtime = harness.runtime.clone();
    let submitted = command.clone();
    let command_task = tokio::spawn(async move { runtime.command(command_id, submitted).await });
    tokio::time::timeout(Duration::from_secs(2), committed)
        .await
        .unwrap()
        .unwrap();
    command_task.abort();
    assert!(command_task.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();

    let promoted = grant_promotion_event(&[], &mut harness.events).await;
    assert_eq!(promoted.caused_by, Some(command_id));
    assert!(matches!(
        promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            outcome: WorkspaceGrantOutcome::Written { .. },
            ..
        }
    ));
    assert_eq!(authority.promotions.lock().unwrap().len(), 1);

    // Replaying the durable command releases the original tool waiter but
    // cannot enqueue or execute the already-settled promotion again.
    harness.runtime.command(command_id, command).await.unwrap();
    collect_through_finished(&mut harness.events).await;
    assert_eq!(authority.promotions.lock().unwrap().len(), 1);
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn promotion_outbox_rejects_a_mismatched_embedded_command_id() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let row_command_id = CommandId::generate().unwrap();
    let promotion = PendingGrantPromotion {
        workspace_id: WorkspaceId::generate().unwrap(),
        workspace_path: "/w".to_owned(),
        session_id: SessionId::generate().unwrap(),
        run_id: RunId::generate().unwrap(),
        command_id: CommandId::generate().unwrap(),
        grant: ApprovalGrant::Tool {
            name: "__test_mutate".to_owned(),
        },
    };
    let promotion_json = serde_json::to_string(&promotion).unwrap();
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO pending_workspace_grant_promotions(
                         command_id, created_at_ms, promotion_json
                     ) VALUES (?1, 1, ?2)",
                params![row_command_id.to_string(), promotion_json],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        store.next_grant_promotion().await.unwrap_err(),
        SessionRuntimeError::CODEC
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn workspace_promotion_outbox_is_atomic_with_the_approval() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_workspace_promotion_outbox
                     BEFORE INSERT ON pending_workspace_grant_promotions
                     BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
                )
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
        })
        .await
        .unwrap();
    let command_id = CommandId::generate().unwrap();
    let error = harness
        .runtime
        .command(
            command_id,
            SessionCommand::RespondToolApproval {
                run_id: harness.run_id,
                tool_call_id: tool_call.id,
                decision: ApprovalDecision::ApproveForWorkspace {
                    grant: ApprovalGrant::Tool {
                        name: "__test_mutate".to_owned(),
                    },
                },
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error, SessionRuntimeError::CONSTRAINT);

    let session_id = harness.session_id;
    let tool_call_id = tool_call.id;
    let state = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            let call = connection.query_row(
                "SELECT state, approval_resolution FROM tool_calls WHERE id = ?1",
                [tool_call_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )?;
            let grants: u32 = connection.query_row(
                "SELECT COUNT(*) FROM session_grants WHERE session_id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            let commands: u32 = connection.query_row(
                "SELECT COUNT(*) FROM commands WHERE id = ?1",
                [command_id.to_string()],
                |row| row.get(0),
            )?;
            let pending: u32 = connection.query_row(
                "SELECT COUNT(*) FROM pending_workspace_grant_promotions",
                [],
                |row| row.get(0),
            )?;
            Ok((call, grants, commands, pending))
        })
        .await
        .unwrap();
    assert_eq!(state, (("awaiting_approval".to_owned(), None), 0, 0, 0));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn workspace_grant_promotions_are_serialized() {
    let (entered, mut entries) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let authority = Arc::new(ObservedBlockingGrantAuthority {
        entered,
        release: Arc::clone(&release),
        active: Arc::new(AtomicUsize::new(0)),
        max_active: Arc::clone(&max_active),
    });
    // Two distinct mutating names so two workspace grants are promoted.
    // Both are catalog-known: an unknown name never reaches the gate.
    let mut harness = scripted_runs_harness_with_authority(
        ApprovalMode::Ask,
        vec![vec![
            ("__test_mutate", "{}".to_owned()),
            ("__test_shell", r#"{"command":"touch note"}"#.to_owned()),
        ]],
        Some(authority),
    )
    .await;
    let run_id = submit_prompt(&harness, "perform both writes").await;

    let (_, first) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        run_id,
        first.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        entries.recv().await,
        Some(ApprovalGrant::Tool { name }) if name == "__test_mutate"
    ));

    let (_, second) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        run_id,
        second.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), entries.recv())
            .await
            .is_err(),
        "the second authority call must wait behind the first"
    );

    release.add_permits(1);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), entries.recv())
            .await
            .unwrap(),
        Some(ApprovalGrant::Tool { name }) if name == "__test_shell"
    ));
    release.add_permits(1);
    collect_through_finished(&mut harness.events).await;
    harness.runtime.shutdown().await.unwrap();
    assert_eq!(max_active.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn shutdown_waits_for_an_accepted_workspace_grant_promotion() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let authority = Arc::new(BlockingGrantAuthority {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    entered.notified().await;
    let observed = collect_through_finished(&mut harness.events).await;

    let runtime = harness.runtime.clone();
    let shutdown = tokio::spawn(async move { runtime.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    release.notify_one();

    shutdown.await.unwrap().unwrap();
    let promoted = grant_promotion_event(&observed, &mut harness.events).await;
    assert!(matches!(
        promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            outcome: WorkspaceGrantOutcome::Written { .. },
            ..
        }
    ));
}

#[tokio::test]
async fn shutdown_reports_an_accepted_grant_promotion_persistence_failure() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let authority = Arc::new(BlockingGrantAuthority {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    entered.notified().await;
    collect_through_finished(&mut harness.events).await;
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_grant_promotion_event
                     BEFORE INSERT ON events
                     WHEN NEW.envelope_json LIKE '%workspace_grant_promoted%'
                     BEGIN SELECT RAISE(ABORT, 'injected promotion failure'); END;",
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();

    let runtime = harness.runtime.clone();
    let shutdown = tokio::spawn(async move { runtime.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    release.notify_one();

    assert_eq!(
        shutdown.await.unwrap().unwrap_err(),
        SessionRuntimeError::Unavailable
    );
    let pending: u32 = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM pending_workspace_grant_promotions",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(
        pending, 1,
        "a failed fate commit must retain its outbox row"
    );

    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection
                .execute("DROP TRIGGER reject_grant_promotion_event", [])
                .map(|_| ())
                .map_err(|_| SessionRuntimeError::CONSTRAINT)
        })
        .await
        .unwrap();
    let database_path = harness._directory.path().join("sessions.sqlite3");
    let store_id = harness.runtime.inner.store.store_id();
    harness.runtime.inner.store.close().await.unwrap();

    let recovered_authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed::default(),
        WorkspaceGrantOutcome::AlreadyPresent {
            path: "/w/.qq/config.ron".to_owned(),
        },
    );
    let mut options = SessionRuntimeOptions::new(database_path);
    options.grant_authority = Some(recovered_authority.clone());
    let recovered = SessionRuntime::open(options, Arc::new(ScriptedLoader))
        .await
        .unwrap();
    let mut recovered_events = recovered
        .subscribe(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after: EventCursor {
                store_id,
                workspace_id: harness.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    let promoted = grant_promotion_event(&[], &mut recovered_events).await;
    assert!(matches!(
        promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            outcome: WorkspaceGrantOutcome::AlreadyPresent { .. },
            ..
        }
    ));
    assert_eq!(recovered_authority.promotions.lock().unwrap().len(), 1);
    let remaining: u32 = recovered
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM pending_workspace_grant_promotions",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_panicking_grant_authority_persists_a_failed_promotion_fate() {
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(Arc::new(PanickingGrantAuthority)),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();

    let observed = collect_through_finished(&mut harness.events).await;
    let promoted = grant_promotion_event(&observed, &mut harness.events).await;
    assert!(matches!(
        promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            outcome: WorkspaceGrantOutcome::Failed { ref message },
            ..
        } if message == "the workspace grant authority panicked"
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_never_reports_success_after_the_runtime_has_failed() {
    let (_directory, runtime) = test_runtime().await;
    runtime.inner.failed.send_replace(true);

    assert_eq!(
        runtime.shutdown().await.unwrap_err(),
        SessionRuntimeError::Unavailable
    );
}

#[tokio::test]
async fn failed_workspace_grant_promotion_leaves_the_approval_standing() {
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed::default(),
        WorkspaceGrantOutcome::Failed {
            message: "denied by managed policy".to_owned(),
        },
    );
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
    )
    .await
    .unwrap();

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
        )),
        "the approved call must execute despite the failed promotion"
    );
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let promoted = grant_promotion_event(&observed, &mut harness.events).await;
    assert!(matches!(
        &promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            outcome: WorkspaceGrantOutcome::Failed { message },
            ..
        } if message == "denied by managed policy"
    ));
}

#[tokio::test]
async fn approve_for_workspace_without_an_authority_reports_a_failed_promotion() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_mutate".to_owned(),
            },
        },
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
    let promoted = grant_promotion_event(&observed, &mut harness.events).await;
    assert!(matches!(
        &promoted.event,
        SessionEvent::WorkspaceGrantPromoted {
            outcome: WorkspaceGrantOutcome::Failed { message },
            ..
        } if message.contains("no workspace grant store")
    ));
}

#[tokio::test]
async fn read_only_sessions_deny_mutating_tools_without_prompting() {
    let mut harness = approval_harness(
        ApprovalMode::ReadOnly,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref() == Some(approval::POLICY_DENIED_RESULT)
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn shell_approval_requests_carry_the_command_and_auto_mode_asks_for_prompt_tier_shell() {
    let mut harness = approval_harness(
        ApprovalMode::Auto,
        "__test_shell",
        r#"{"command":"git push origin main","cwd":"crates"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let shell = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalRequested { shell, .. } => shell.clone(),
            _ => None,
        })
        .expect("shell approval requests carry the command");
    assert_eq!(shell.command, "git push origin main");
    assert_eq!(shell.cwd.as_deref(), Some("crates"));

    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::ShellPrefix {
                prefix: "git push".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
    )));
}

#[tokio::test]
async fn reviewer_approval_executes_a_held_call_without_a_client() {
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = approval_harness_with_reviewer(
        ApprovalMode::Auto,
        "__test_shell",
        r#"{"command":"git push origin main"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        observed
            .iter()
            .any(|event| matches!(&event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedByReviewer,
            ..
        }
    )));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let requests = consulted.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_name, "__test_shell");
    assert_eq!(
        requests[0]
            .shell
            .as_ref()
            .map(|shell| shell.command.as_str()),
        Some("git push origin main")
    );
}

#[tokio::test]
async fn reviewer_escalation_leaves_the_call_waiting_for_a_client() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Escalate {
        reason: "unsure".to_owned(),
    }));
    let mut harness = approval_harness_with_reviewer(
        ApprovalMode::Auto,
        "__test_shell",
        r#"{"command":"git push origin main"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    )));
}

#[tokio::test]
async fn reviewer_denial_still_lets_the_client_decide() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Deny {
        reason: "dangerous".to_owned(),
    }));
    let mut harness = approval_harness_with_reviewer(
        ApprovalMode::Auto,
        "__test_shell",
        r#"{"command":"git push origin main"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Deny,
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::Denied,
            ..
        }
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
    );
}

#[tokio::test]
async fn client_resolution_wins_over_a_late_reviewer_approval() {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = approval_harness_with_reviewer(
        ApprovalMode::Auto,
        "__test_shell",
        r#"{"command":"git push origin main"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    // The reviewer answers only after the client's resolution committed.
    let _ = release.send(());
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    )));
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedByReviewer,
            ..
        }
    )));
}

#[tokio::test]
async fn ask_mode_never_consults_the_reviewer() {
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = approval_harness_with_reviewer(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    assert!(consulted.lock().unwrap().is_empty());
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    collect_through_finished(&mut harness.events).await;
    assert!(consulted.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn approved_shell_calls_execute_stream_output_and_share_prefix_grants() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "shell",
        r#"{"command":"echo approved-output"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);
    let shell = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalRequested { shell, .. } => shell.clone(),
            _ => None,
        })
        .expect("shell approval requests carry the command preview");
    assert_eq!(shell.command, "echo approved-output");
    assert_eq!(shell.cwd, None);

    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::ShellPrefix {
                prefix: "echo".to_owned(),
            },
        },
    )
    .await
    .unwrap();

    let observed = collect_through_finished_generously(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "the echo prefix grant must cover the second call"
    );
    let completed = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed =>
            {
                Some(tool_call.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 2);
    for tool_call in &completed {
        let result = tool_call.result.as_deref().unwrap();
        assert!(result.contains("approved-output"), "{result}");
        assert!(result.starts_with("shell exit=0 "), "{result}");
    }
    // Live output was published, and before the call's terminal event.
    let first_delta = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolCallOutputDelta { tool_call_id, chunk }
                    if *tool_call_id == completed[0].id && chunk.contains("approved-output")
            )
        })
        .expect("shell output must stream as ToolCallOutputDelta events");
    let finished = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolCallFinished { tool_call } if tool_call.id == completed[0].id
            )
        })
        .unwrap();
    assert!(first_delta < finished);
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn read_only_sessions_deny_shell_without_prompting() {
    let mut harness = approval_harness(
        ApprovalMode::ReadOnly,
        "shell",
        r#"{"command":"echo blocked"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallOutputDelta { .. })),
        "a denied command must never produce output"
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref() == Some(approval::POLICY_DENIED_RESULT)
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_run_interrupts_the_running_shell_call() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "shell",
        r#"{"command":"echo running; sleep 30"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();

    // The first live chunk proves the command is running before the run
    // is cancelled; no sleeps are used to sequence the race.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if matches!(
                &event.event,
                SessionEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("running")
            ) {
                break;
            }
        }
    })
    .await
    .expect("the approved shell call must stream its first chunk");

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
    let observed = collect_through_finished_generously(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call: finished }
            if finished.id == tool_call.id
                && finished.state == ToolCallState::Interrupted
                && finished.result.as_deref() == Some(INTERRUPTED_TOOL_RESULT)
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Cancelled,
            ..
        }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_with_buffered_tool_output_drops_it_before_terminal_settlement() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "shell",
        r#"{"command":"printf buffered-output; sleep 30"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (mut observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let (buffered, release) = execution::hold_buffered_tool_output(tool_call.id);
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), buffered)
        .await
        .expect("shell output must enter the bounded batch")
        .unwrap();

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
    release.send(()).unwrap();
    observed.extend(collect_through_finished_generously(&mut harness.events).await);

    assert!(
        !observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallOutputDelta { tool_call_id, .. }
                if *tool_call_id == tool_call.id
        )),
        "a partial live batch must not publish after cancellation"
    );
    let started = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolCallStarted { tool_call: started }
                    if started.id == tool_call.id
            )
        })
        .unwrap();
    let cancelled = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::CancellationRequested { run_id, .. }
                    if *run_id == harness.run_id
            )
        })
        .unwrap();
    let interrupted = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolCallFinished { tool_call: finished }
                    if finished.id == tool_call.id
                        && finished.state == ToolCallState::Interrupted
                        && finished.result.as_deref() == Some(INTERRUPTED_TOOL_RESULT)
            )
        })
        .unwrap();
    let finished = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::RunFinished {
                    run_id,
                    outcome: RunOutcome::Cancelled,
                    ..
                } if *run_id == harness.run_id
            )
        })
        .unwrap();
    assert!(started < cancelled && cancelled < interrupted && interrupted < finished);
}

#[tokio::test]
async fn edit_approvals_carry_the_diff_preview_and_apply_after_approval() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Ask,
        vec![vec![
            ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
            (
                "edit_file",
                r#"{"edits":[{"path":"note.txt","old":"hello world","new":"goodbye world"}]}"#
                    .to_owned(),
            ),
        ]],
    )
    .await;
    let note = harness.workspace_path.join("note.txt");
    std::fs::write(&note, "hello world\n").unwrap();
    let run_id = submit_prompt(&harness, "edit the note").await;

    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let edit = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalRequested { edit, .. } => edit.clone(),
            _ => None,
        })
        .expect("edit approval requests carry the diff preview");
    assert_eq!(edit.path, "note.txt");
    assert_eq!(edit.diff, "- hello world\n+ goodbye world\n");
    assert_eq!(
        std::fs::read_to_string(&note).unwrap(),
        "hello world\n",
        "nothing may be applied before approval"
    );

    respond_approval(
        &harness.runtime,
        run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "edit_file" && tool_call.state == ToolCallState::Completed
    )));
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "goodbye world\n");
}

#[tokio::test]
async fn file_state_survives_restart_and_auto_mode_applies_the_edit_without_prompting() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())]],
    )
    .await;
    let note = harness.workspace_path.join("note.txt");
    std::fs::write(&note, "hello\n").unwrap();

    submit_prompt(&harness, "read the note").await;
    let first = collect_through_finished(&mut harness.events).await;
    assert!(first.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "read_file" && tool_call.state == ToolCallState::Completed
    )));
    let after = first.last().unwrap().cursor;
    let ScriptedRunsHarness {
        _directory,
        runtime,
        workspace_path,
        workspace_id,
        session_id,
        events,
        ..
    } = harness;
    runtime.shutdown().await.unwrap();
    // The subscription keeps the store (and its ownership) alive.
    drop(events);
    drop(runtime);

    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(workspace_path.join("sessions.sqlite3")),
        Arc::new(ScriptedRunsLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            runs: vec![vec![(
                "edit_file",
                r#"{"edits":[{"path":"note.txt","old":"hello","new":"goodbye"}]}"#.to_owned(),
            )]],
            loads: StdMutex::new(0),
        }),
    )
    .await
    .unwrap();
    let mut events = reopened
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();

    // The restarted runtime edits without re-reading: the read-before-write
    // rule is satisfied by the durable file-state map recorded by run one.
    let queued = reopened
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("now edit it".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        queued.outcome,
        CommandOutcome::PromptQueued { .. }
    ));
    let second = collect_through_finished(&mut events).await;
    assert!(
        !second
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "auto mode must not prompt for workspace edits"
    );
    assert!(second.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "edit_file" && tool_call.state == ToolCallState::Completed
    )));
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "goodbye\n");
    reopened.shutdown().await.unwrap();
    drop(_directory);
}

#[tokio::test]
async fn completed_edits_persist_a_display_diff_the_model_context_never_carries() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![
            vec![
                ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
                (
                    "edit_file",
                    r#"{"edits":[{"path":"note.txt","old":"hello","new":"goodbye"}]}"#.to_owned(),
                ),
            ],
            Vec::new(),
        ],
    )
    .await;
    let note = harness.workspace_path.join("note.txt");
    std::fs::write(&note, "hello\n").unwrap();

    submit_prompt(&harness, "edit the note").await;
    let observed = collect_through_finished(&mut harness.events).await;
    let finished_call = |name: &str| {
        observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolCallFinished { tool_call } if tool_call.name == name => {
                    Some(tool_call.clone())
                }
                _ => None,
            })
            .unwrap()
    };
    let edited = finished_call("edit_file");
    assert_eq!(edited.state, ToolCallState::Completed);
    // The model-facing result stays the compact summary; the diff rides
    // in the display payload only.
    assert!(
        edited
            .result
            .as_deref()
            .is_some_and(|result| result.starts_with("edit ok files=1 edits=1\nnote.txt h:")),
        "{:?}",
        edited.result
    );
    assert_eq!(
        edited.display,
        Some(ToolCallDisplay::Diff {
            path: "note.txt".to_owned(),
            diff: "--- a/note.txt\n+++ b/note.txt\n@@ -1,1 +1,1 @@\n-hello\n+goodbye\n".to_owned(),
        })
    );
    assert_eq!(finished_call("read_file").display, None);

    // The payload persists with the call and replays in snapshots.
    let (workspace_id, _) = resolve_workspace(&harness.runtime, &harness.workspace_path).await;
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let persisted = focused
        .tool_calls
        .iter()
        .find(|call| call.id == edited.id)
        .unwrap();
    assert_eq!(persisted.display, edited.display);

    // A follow-up run reassembles model context from the store: the
    // summary result replays, the display diff never does.
    submit_prompt(&harness, "what changed?").await;
    let _ = collect_through_finished(&mut harness.events).await;
    let requests = harness.requests.lock().unwrap();
    let follow_up = requests.last().unwrap();
    let tool_results = follow_up
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        tool_results
            .iter()
            .any(|content| content.starts_with("edit ok files=1 edits=1\n")),
        "{tool_results:?}"
    );
    assert!(
        !tool_results
            .iter()
            .any(|content| content.contains("+goodbye")),
        "the display diff must never enter model context"
    );
}

#[tokio::test]
async fn auto_mode_executes_mutating_tools_after_a_mode_change() {
    let mut harness = approval_harness(
        ApprovalMode::ReadOnly,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    // The first run is denied by read-only policy.
    let _ = collect_through_finished(&mut harness.events).await;

    let receipt = harness
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
    assert_eq!(
        receipt.outcome,
        CommandOutcome::ApprovalModeSet {
            session_id: harness.session_id,
            mode: ApprovalMode::Auto,
        }
    );
    // The change is published as a summary update carrying the new mode,
    // so every connected client renders it, and the receipt commits
    // through that event.
    let updated = harness.events.next().await.unwrap().unwrap();
    assert_eq!(updated.cursor, receipt.committed_through);
    assert!(matches!(
        &updated.event,
        SessionEvent::SessionUpdated { session }
            if session.id == harness.session_id
                && session.approval_mode == ApprovalMode::Auto
    ));

    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("mutate again".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
    )));
}

#[tokio::test]
async fn cancellation_interrupts_a_run_waiting_for_approval() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
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
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call: finished }
            if finished.id == tool_call.id
                && finished.state == ToolCallState::Interrupted
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Cancelled,
            ..
        }
    ));
}

#[tokio::test]
async fn recovery_marks_awaiting_approval_calls_interrupted() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: directory.path().to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let created = store
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
                approval_mode: ApprovalMode::Ask,
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("mutate".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    let tool_call_id = ToolCallId::generate().unwrap();
    let call = RuntimeToolCall {
        id: tool_call_id,
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        name: "__test_mutate".to_owned(),
        effect: crate::catalog::EffectClass::Mutating,
        arguments: "{}".to_owned(),
        rejection: None,
    };
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    vec![ContentBlock::tool_call(
                        call.provider_call_id.clone(),
                        call.name.clone(),
                        &serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap(),
                    )],
                ),
                calls: vec![call],
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    let awaiting = store
        .request_tool_approval(&claimed, tool_call_id, ApprovalPreviews::default())
        .await
        .unwrap();
    store.close().await.unwrap();
    drop(store);

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: awaiting.cursor,
        })
        .unwrap();
    let recovered = collect_through_finished(&mut events).await;
    assert!(matches!(
        &recovered[0].event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.id == tool_call_id
                && tool_call.state == ToolCallState::Interrupted
                && tool_call.is_error
    ));
    assert!(matches!(
        &recovered[1].event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Interrupted,
            ..
        }
    ));
}

const ASK_ARGUMENTS: &str =
    r#"{"questions":[{"prompt":"Which crate?","options":["qq-core","qq-tui"]},{"prompt":"Why?"}]}"#;

#[tokio::test]
async fn ask_user_holds_under_read_only_and_the_answers_become_the_result() {
    // Read-only would deny any other non-read call; a question is not an
    // action, so it is put to the user under every mode.
    let mut harness = approval_harness(
        ApprovalMode::ReadOnly,
        "ask_user",
        ASK_ARGUMENTS,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let question = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalRequested {
                question,
                shell,
                edit,
                ..
            } => {
                assert!(shell.is_none() && edit.is_none());
                question.clone()
            }
            _ => None,
        })
        .expect("the hold carries the parsed questions");
    assert_eq!(question.questions.len(), 2);
    assert_eq!(question.questions[0].options, ["qq-core", "qq-tui"]);
    assert!(!question.questions[0].free_text);
    assert!(question.questions[1].free_text);

    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Answer {
            answers: vec!["qq-core".to_owned(), "it owns the runtime".to_owned()],
        },
    )
    .await
    .unwrap();

    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::Answered,
        } if tool_call.state == ToolCallState::Completed && !tool_call.is_error
    )));
    // Nothing executed: no start, no finish beyond the resolution.
    assert!(!observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolCallStarted { .. } | SessionEvent::ToolCallFinished { .. }
    )));
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    let result = {
        let requests = harness.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        match requests[1].messages()[2].content() {
            [
                ContentBlock::ToolResult {
                    content,
                    is_error: false,
                    ..
                },
            ] => content.clone(),
            other => panic!("unexpected tool result content {other:?}"),
        }
    };
    assert_eq!(
        result,
        "ask_user answered=2/2\nQ1: Which crate?\nA: qq-core\nQ2: Why?\nA: it owns the runtime\n"
    );
}

#[tokio::test]
async fn declining_a_question_settles_it_as_answered_with_the_decline_text() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "ask_user",
        ASK_ARGUMENTS,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Answer {
            answers: Vec::new(),
        },
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::Answered,
        } if tool_call.result.as_deref() == Some(approval::DECLINED_QUESTION_RESULT)
    )));
    // A second answer is the idempotent replay of the first.
    let receipt = respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::Answer {
            answers: vec!["late".to_owned()],
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::Answered,
            ..
        }
    ));
}

#[tokio::test]
async fn an_unanswered_question_times_out_like_an_approval() {
    let mut harness = approval_harness(
        ApprovalMode::Auto,
        "ask_user",
        ASK_ARGUMENTS,
        1,
        Duration::from_millis(50),
    )
    .await;
    let (_, _) = collect_until_approval_requested(&mut harness.events).await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        } if tool_call.state == ToolCallState::Denied
    )));
}

#[tokio::test]
async fn malformed_ask_user_arguments_are_a_tool_error_without_a_hold() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "ask_user",
        r#"{"questions":[{"prompt":"a","options":["only one"]}]}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.is_error
                && tool_call.result.as_deref().is_some_and(|result| result.contains("needs 2-6 options"))
    )));
}

#[tokio::test]
async fn fetch_holds_with_a_host_preview_and_a_host_grant_covers_the_next_call() {
    // `invalid.` is reserved (RFC 6761) and never resolves, so the approved
    // call fails at resolution — a tool error, not a policy event — while
    // everything this test asserts happens before dispatch.
    let mut harness = approval_harness(
        ApprovalMode::Auto,
        "fetch",
        r#"{"url":"https://docs.invalid/axum"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let preview = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalRequested {
                fetch, shell, edit, ..
            } => {
                assert!(shell.is_none() && edit.is_none());
                fetch.clone()
            }
            _ => None,
        })
        .expect("the hold carries the fetch preview");
    assert_eq!(preview.url, "https://docs.invalid/axum");
    assert_eq!(preview.host, "docs.invalid");
    assert!(preview.method.is_none());

    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForSession {
            grant: ApprovalGrant::Host {
                host: "*.invalid".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "the host grant must cover the second call"
    );
    let finished: Vec<_> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolCallFinished { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 2);
    for call in finished {
        assert!(call.is_error);
        assert!(
            call.result
                .as_deref()
                .is_some_and(|result| result.contains("could not resolve docs.invalid")),
            "{:?}",
            call.result
        );
    }
    let (_, grants) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert_eq!(grants.hosts, ["*.invalid"]);
}

#[tokio::test]
async fn fetch_to_a_blocked_host_is_denied_under_full_without_a_hold() {
    let mut harness = approval_harness(
        ApprovalMode::Full,
        "fetch",
        r#"{"url":"http://169.254.169.254/latest/meta-data/"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref().is_some_and(|result| {
                    result.starts_with("fetch refused: host 169.254.169.254 resolves to a private")
                })
    )));
}
