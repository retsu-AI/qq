use super::*;

fn assert_no_human_permission_request(events: &[SessionEventEnvelope]) {
    assert!(
        !events.iter().any(|event| matches!(
            event.event,
            SessionEvent::ToolApprovalRequested {
                delegated: false,
                ..
            } | SessionEvent::ToolApprovalEscalated { .. }
        )),
        "Auto may supervise internally but must never ask a human"
    );
}

#[tokio::test]
async fn auto_denies_out_of_scope_shell_and_continues_without_approval_or_review() {
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = scripted_runs_harness_with(
        ApprovalMode::Auto,
        vec![vec![
            (
                "__test_shell",
                r#"{"command":"unknown-ungranted-command"}"#.to_owned(),
            ),
            ("__test_mutate", "{}".to_owned()),
        ]],
        None,
        Some(reviewer),
    )
    .await;
    let run_id = submit_prompt(&harness, "try an allowed alternative").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolApprovalRequested { .. } | SessionEvent::ToolApprovalEscalated { .. }
    )));
    assert!(consulted.lock().unwrap().is_empty());
    assert!(!observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallStarted { tool_call } if tool_call.name == "__test_shell"
    )));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "__test_shell" && tool_call.is_error
                && tool_call.result.as_deref().is_some_and(|result| result.contains("outside_auto_scope"))
    )));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "__test_mutate" && !tool_call.is_error
                && tool_call.result.as_deref() == Some("mutated")
    )));
    assert!(matches!(
        observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    harness.runtime.shutdown().await.unwrap();
}

pub(super) struct GatedApprovalProvider {
    pub(super) inner: ApprovalProvider,
    pub(super) release: StdMutex<Option<oneshot::Receiver<()>>>,
}

impl Provider for GatedApprovalProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let release = self.release.lock().unwrap().take();
        let mut events = self.inner.stream(request);
        Box::pin(async_stream! {
            if let Some(release) = release { release.await.unwrap(); }
            while let Some(event) = events.next().await { yield event; }
        })
    }
}

#[tokio::test]
async fn an_auto_parent_prevents_child_mode_changes_from_introducing_human_approval() {
    for mode in [ApprovalMode::Ask, ApprovalMode::Full] {
        let (reviewer, consulted) =
            StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
        let (release, pending) = oneshot::channel();
        let child = Arc::new(GatedApprovalProvider {
            inner: ApprovalProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                turn: StdMutex::new(0),
                tool: "__test_mutate",
                arguments: "{}",
                tool_turns: 1,
                usage: None,
            },
            release: StdMutex::new(Some(pending)),
        });
        let (mut harness, _) = write_child_harness(child, vec![("spawn_agent",
            r#"{"task":"mutate within parent authority","model":"test/child","authority":"write"}"#.to_owned())],
            Some(reviewer), ApprovalMode::Auto).await;
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
        let changed = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetApprovalMode {
                    session_id: child_id,
                    mode,
                },
            )
            .await;
        if mode == ApprovalMode::Full {
            assert_eq!(
                changed.unwrap_err(),
                SessionRuntimeError::ChildAuthorityEscalation
            );
        } else {
            changed.unwrap();
        }
        release.send(()).unwrap();
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        assert_no_human_permission_request(&observed);
        let completed = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolCallFinished { tool_call }
                    if event.session_id == child_id && tool_call.name == "__test_mutate" =>
                {
                    Some(tool_call)
                }
                _ => None,
            })
            .expect("the child action settles");
        if mode == ApprovalMode::Full {
            assert!(!completed.is_error, "{:?}", completed.result);
            assert_eq!(
                consulted.lock().unwrap().len(),
                1,
                "a refused Full elevation cannot bypass internal supervision"
            );
        } else {
            assert!(completed.is_error);
            assert!(
                completed
                    .result
                    .as_deref()
                    .unwrap()
                    .contains("outside_auto_scope")
            );
            assert!(consulted.lock().unwrap().is_empty());
        }
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ask_requires_human_approval_for_each_call_even_with_grants_and_delegate_on() {
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = scripted_runs_harness_with(
        ApprovalMode::Ask,
        vec![vec![
            ("__test_mutate", "{}".to_owned()),
            ("__test_mutate", "{}".to_owned()),
        ]],
        None,
        Some(reviewer),
    )
    .await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalDelegate {
                session_id: harness.session_id,
                delegate: Some(qq_protocol::ApprovalDelegate::On),
            },
        )
        .await
        .unwrap();
    let session_id = harness.session_id;
    harness.runtime.inner.store.call(Priority::Control, move |connection| {
        connection.execute("INSERT INTO session_grants(session_id, kind, value, created_at_ms, source) VALUES (?1, 'tool', '__test_mutate', 0, 'human')", [session_id.to_string()])?;
        Ok(())
    }).await.unwrap();
    let run_id = submit_prompt(&harness, "mutate twice").await;
    let (_, first) = collect_until_approval_requested(&mut harness.events).await;
    let receipt = respond_approval(
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
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedOnce,
            ..
        }
    ));
    let (_, second) = collect_until_approval_requested(&mut harness.events).await;
    assert_ne!(first.id, second.id);
    respond_approval(&harness.runtime, run_id, second.id, ApprovalDecision::Deny)
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
    assert!(consulted.lock().unwrap().is_empty());
    let (_, grants, _) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert_eq!(
        grants.tools,
        ["__test_mutate".to_owned()].into_iter().collect()
    );
    harness.runtime.shutdown().await.unwrap();
}

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
            ..
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
        Some(Duration::from_millis(50)),
    )
    .await;
    let (_, _) = collect_until_approval_requested(&mut harness.events).await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
            ..
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
    let (directory, store, claimed, tool_call_id) =
        stored_approval_fixture("__test_shell", r#"{"command":"git push origin main"}"#).await;
    let receipt = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::RespondToolApproval {
                run_id: claimed.identity.run_id,
                tool_call_id,
                decision: ApprovalDecision::ApproveForSession {
                    grant: ApprovalGrant::Tool {
                        name: "__test_shell".to_owned(),
                    },
                },
            },
        )
        .await
        .unwrap()
        .receipt;
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedForSession,
            ..
        }
    ));
    // The grant is durable authority, independent of the old approval hold.
    store
        .finish_run(
            &claimed,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedRunsLoader {
            requests: Arc::new(StdMutex::new(Vec::new())),
            runs: vec![vec![
                (
                    "__test_shell",
                    r#"{"command":"git push origin main"}"#.to_owned(),
                ),
                (
                    "__test_shell",
                    r#"{"command":"git push origin main"}"#.to_owned(),
                ),
            ]],
            loads: StdMutex::new(0),
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: claimed.identity.workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    let run_id = submit_prompt_to(
        &runtime,
        claimed.identity.session_id,
        "reuse the stored grant",
    )
    .await;
    let observed = collect_until_run_finished(&mut events, run_id).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.event,
                SessionEvent::ToolCallFinished { tool_call }
                    if tool_call.run_id == run_id && tool_call.state == ToolCallState::Completed
            ))
            .count(),
        2
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_or_full_session_grants_record_once_approval_without_recording_grant() {
    for case in ["oversized", "full", "empty"] {
        let command = if case == "oversized" {
            format!("git push {}", "x".repeat(MAX_GRANT_BYTES))
        } else {
            "git push origin main".to_owned()
        };
        let arguments = serde_json::json!({"command": command}).to_string();
        let (_directory, store, claimed, tool_call_id) =
            stored_approval_fixture("__test_shell", &arguments).await;
        let session_id = claimed.identity.session_id;
        if case == "full" {
            store
                .call(Priority::Control, move |connection| {
                    for index in 0..MAX_SESSION_GRANTS {
                        connection.execute(
                            "INSERT INTO session_grants(session_id, kind, value, created_at_ms)
                         VALUES (?1, 'tool', ?2, 0)",
                            params![session_id.to_string(), format!("tool-{index}")],
                        )?;
                    }
                    Ok(())
                })
                .await
                .unwrap();
        }
        let decision = match case {
            "oversized" => ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix { prefix: command },
            },
            "full" => ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::Tool {
                    name: "__test_shell".to_owned(),
                },
            },
            _ => ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::Tool {
                    name: "   ".to_owned(),
                },
            },
        };
        let result = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RespondToolApproval {
                    run_id: claimed.identity.run_id,
                    tool_call_id,
                    decision,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result.receipt.outcome,
            CommandOutcome::ToolApprovalResolved {
                tool_call_id,
                resolution: ApprovalResolution::ApprovedOnce,
            },
            "{case}"
        );
        let (_, grants, _) = store.approval_policy(session_id).await.unwrap();
        assert!(grants.shell_prefixes.is_empty(), "{case}");
        assert_eq!(
            grants.tools.len(),
            if case == "full" {
                usize::try_from(MAX_SESSION_GRANTS).unwrap()
            } else {
                0
            },
            "{case}"
        );
        assert!(
            store.next_grant_promotion().await.unwrap().is_none(),
            "an unrecorded grant must not be promoted: {case}"
        );
        store.close().await.unwrap();
    }
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
        ApprovalMode::Auto,
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

// Outbox tests resolve a real delegated hold directly through the API. The
// action already has authority; this fixture never requests human approval.
async fn grant_promotion_harness(
    tool_turns: usize,
    authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
) -> (ApprovalHarness, oneshot::Sender<()>) {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let harness = supervised_approval_harness(
        "__test_shell",
        r#"{"command":"cargo test"}"#,
        tool_turns,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        authority,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    (harness, release)
}

#[tokio::test]
async fn approve_for_workspace_records_the_session_grant_and_promotes_it() {
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed::default(),
        WorkspaceGrantOutcome::Written {
            path: "/w/.qq/config.ron".to_owned(),
        },
    );
    let (mut harness, _review_release) = grant_promotion_harness(2, Some(authority.clone())).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::RespondToolApproval {
        run_id: harness.run_id,
        tool_call_id: tool_call.id,
        decision: ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
        !observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::ToolApprovalRequested {
                delegated: false,
                ..
            }
        )),
        "subsequent supervision must not request human approval"
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
        } if name == "__test_shell" && path == "/w/.qq/config.ron"
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
    let (mut harness, _review_release) = grant_promotion_harness(1, Some(authority.clone())).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::RespondToolApproval {
        run_id: harness.run_id,
        tool_call_id: tool_call.id,
        decision: ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
            name: "__test_shell".to_owned(),
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
    let (_directory, store, claimed, tool_call_id) =
        stored_approval_fixture("__test_shell", r#"{"command":"git push origin main"}"#).await;
    store
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
    let error = store
        .command(
            command_id,
            SessionCommand::RespondToolApproval {
                run_id: claimed.identity.run_id,
                tool_call_id,
                decision: ApprovalDecision::ApproveForWorkspace {
                    grant: ApprovalGrant::Tool {
                        name: "__test_shell".to_owned(),
                    },
                },
            },
        )
        .await
        .err()
        .expect("the injected outbox failure rejects the transaction");
    assert_eq!(error, SessionRuntimeError::CONSTRAINT);

    let session_id = claimed.identity.session_id;
    let state = store
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
    store.close().await.unwrap();
}

struct PendingSupervisionReviewer;

impl ApprovalReviewer for PendingSupervisionReviewer {
    fn review(&self, _request: ReviewRequest) -> ReviewFuture {
        Box::pin(std::future::pending())
    }
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
    // Distinct grants for the same allowed action exercise serialization;
    // mandatory child supervision remains even when the first grant matches.
    let mut harness = supervised_approval_harness(
        "__test_shell",
        r#"{"command":"cargo test"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        Some(authority),
        Arc::new(PendingSupervisionReviewer),
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let run_id = harness.run_id;

    let (_, first) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        run_id,
        first.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::ShellPrefix {
                prefix: "cargo".to_owned(),
            },
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        entries.recv().await,
        Some(ApprovalGrant::ShellPrefix { prefix }) if prefix == "cargo"
    ));

    let (_, second) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        run_id,
        second.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::ShellPrefix {
                prefix: "cargo test".to_owned(),
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
        Some(ApprovalGrant::ShellPrefix { prefix }) if prefix == "cargo test"
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
    let (mut harness, _review_release) = grant_promotion_harness(1, Some(authority)).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
    let (mut harness, _review_release) = grant_promotion_harness(1, Some(authority)).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
    let (mut harness, _review_release) =
        grant_promotion_harness(1, Some(Arc::new(PanickingGrantAuthority))).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
    let (mut harness, _review_release) = grant_promotion_harness(1, Some(authority)).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
    let (mut harness, _review_release) = grant_promotion_harness(1, None).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveForWorkspace {
            grant: ApprovalGrant::Tool {
                name: "__test_shell".to_owned(),
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
async fn ask_shell_approval_requests_carry_the_command_preview() {
    let mut harness = approval_harness(
        ApprovalMode::Ask,
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
            SessionEvent::ToolApprovalRequested {
                shell, delegated, ..
            } => {
                // No reviewer configured: the human is asked now.
                assert!(!delegated);
                shell.clone()
            }
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
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalRequested {
                delegated: true,
                ..
            }
        )),
        "the request says the reviewer decides first (ENG-1034)"
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
    assert_eq!(requests[0].tool_name, "__test_mutate");
    assert!(matches!(
        requests[0].origin,
        ReviewOrigin::Child { depth: 1, .. }
    ));
}

#[tokio::test]
async fn a_delegate_approval_records_an_exact_command_grant_and_nothing_wider() {
    // Delegate grants remain exact audit records. Every permitted child action
    // still passes internal supervision, including repeated commands.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_scripted_approval_harness(
        vec![
            ("__test_shell", r#"{"command":"cargo test x"}"#.to_owned()),
            ("__test_shell", r#"{"command":"cargo test x"}"#.to_owned()),
            ("__test_shell", r#"{"command":"cargo test y"}"#.to_owned()),
        ],
        None,
        reviewer,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    let held: Vec<String> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolApprovalRequested {
                shell: Some(shell), ..
            } => Some(shell.command.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        held,
        ["cargo test x", "cargo test x", "cargo test y"],
        "stored grants never bypass mandatory child supervision"
    );
    assert_eq!(
        consulted.lock().unwrap().len(),
        3,
        "the reviewer supervises every child command"
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
        3
    );
    let (_, grants, _) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert!(
        grants.shell_prefixes.is_empty(),
        "a delegate grant is not a human prefix grant: {grants:?}"
    );
    assert_eq!(
        grants.delegate.commands,
        ["cargo test x".to_owned(), "cargo test y".to_owned()]
            .into_iter()
            .collect()
    );
    // The same row shape a delegate writes is never a workspace promotion.
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
                .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(pending, 0, "a delegate verdict never schedules a promotion");
}

#[tokio::test]
#[cfg(feature = "tool-fetch")]
async fn a_delegate_host_grant_covers_the_exact_host_only() {
    // Test persistence and exact matching independently of live admission:
    // a reviewer cannot supply host authority missing from an Auto task.
    let (_directory, store, claimed, id) =
        stored_approval_fixture("fetch", r#"{"url":"https://docs.invalid/a"}"#).await;
    let policy = store
        .tool_approval_policy(claimed.identity.session_id)
        .await
        .unwrap();
    let event = store
        .resolve_approval_by_reviewer(
            &claimed,
            id,
            Some(DelegateGrant::Host("docs.invalid".to_owned())),
            policy,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::ApprovedByReviewer,
            ..
        }
    ));
    assert!(matches!(
        store
            .conclude_tool_approval(&claimed, id, false)
            .await
            .unwrap(),
        ConcludedApproval::Approved
    ));
    let (_, grants, _) = store
        .approval_policy(claimed.identity.session_id)
        .await
        .unwrap();
    assert!(
        grants.hosts.is_empty(),
        "reviewer grants are not human host grants"
    );
    assert_eq!(
        grants.delegate.hosts,
        ["docs.invalid".to_owned()].into_iter().collect()
    );
    let class = |host: &str| approval::ToolClass::Network {
        host: Some(host.to_owned()),
        refusal: None,
    };
    assert!(matches!(
        approval::evaluate(ApprovalMode::Auto, "fetch", &class("docs.invalid"), &grants),
        approval::PolicyDecision::Execute
    ));
    assert!(matches!(
        approval::evaluate(
            ApprovalMode::Auto,
            "fetch",
            &class("api.docs.invalid"),
            &grants
        ),
        approval::PolicyDecision::RequireApproval
    ));
}

#[tokio::test]
async fn a_reviewer_approval_records_a_delegate_grant_row_that_still_matches_exactly() {
    // DA5: a reviewer's grant row names its source, so an audit can tell it
    // from a human grant. It is a delegate row: exact match only, counted
    // against the same per-run cap, never a human prefix grant. New rows
    // never say `jev`: that source is historical.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_scripted_approval_harness(
        vec![
            ("__test_shell", r#"{"command":"cargo test x"}"#.to_owned()),
            ("__test_shell", r#"{"command":"cargo test x"}"#.to_owned()),
            (
                "__test_shell",
                r#"{"command":"cargo test x --amend"}"#.to_owned(),
            ),
        ],
        None,
        reviewer,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert_eq!(
        consulted.lock().unwrap().len(),
        3,
        "each permitted child action is supervised, including repeated commands"
    );
    assert!(observed.iter().all(|event| !matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::DeniedByReviewer,
            ..
        }
    )));
    let rows: Vec<(String, String, String)> = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            let mut statement = connection.prepare(
                "SELECT kind, value, source FROM session_grants ORDER BY created_at_ms, value",
            )?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            (
                "shell_prefix".to_owned(),
                "cargo test x".to_owned(),
                "delegate".to_owned()
            ),
            (
                "shell_prefix".to_owned(),
                "cargo test x --amend".to_owned(),
                "delegate".to_owned()
            ),
        ]
    );
    let (_, grants, _) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert!(
        grants.shell_prefixes.is_empty(),
        "a reviewer row is a delegate row, not a human prefix: {grants:?}"
    );
    assert!(grants.delegate.commands.contains("cargo test x"));
}

#[tokio::test]
async fn a_delegate_grant_that_cannot_be_stored_still_approves_the_call_once() {
    // Storage limits never invalidate a current reviewer verdict. Scope
    // admission and real supervised execution are covered separately.
    for capped in [false, true] {
        let command = if capped {
            "cargo test capped".to_owned()
        } else {
            format!("cargo test {}", "x".repeat(MAX_GRANT_BYTES))
        };
        let arguments = serde_json::json!({"command": command}).to_string();
        let (_directory, store, claimed, id) =
            stored_approval_fixture("__test_shell", &arguments).await;
        if capped {
            let session = claimed.identity.session_id.to_string();
            let run = claimed.identity.run_id.to_string();
            store.call(Priority::Control, move |connection| {
                for index in 0..MAX_DELEGATE_GRANTS_PER_RUN {
                    connection.execute(
                        "INSERT INTO session_grants(session_id, kind, value, created_at_ms, source, run_id)
                         VALUES (?1, 'shell_prefix', ?2, 0, 'delegate', ?3)",
                        params![session, format!("cmd-{index}"), run],
                    )?;
                }
                Ok(())
            }).await.unwrap();
        }
        // Capture authority after seeding; changing it during review must deny.
        let policy = store
            .tool_approval_policy(claimed.identity.session_id)
            .await
            .unwrap();
        let event = store
            .resolve_approval_by_reviewer(
                &claimed,
                id,
                Some(DelegateGrant::Command(command.clone())),
                policy,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedByReviewer,
                ..
            }
        ));
        assert!(matches!(
            store
                .conclude_tool_approval(&claimed, id, false)
                .await
                .unwrap(),
            ConcludedApproval::Approved
        ));
        let (_, grants, _) = store
            .approval_policy(claimed.identity.session_id)
            .await
            .unwrap();
        assert!(!grants.delegate.commands.contains(&command));
        assert_eq!(
            u32::try_from(grants.delegate.commands.len()).unwrap(),
            if capped {
                MAX_DELEGATE_GRANTS_PER_RUN
            } else {
                0
            }
        );
    }
}

#[tokio::test]
async fn a_delegate_grant_never_lifts_forbidden_and_a_forbidden_call_never_reaches_the_reviewer() {
    // Plan acceptance 2: with a reviewer configured, a Forbidden command is
    // refused by the classifier and the reviewer is not consulted. Seeding
    // the exact string as a delegate row changes nothing: only a human's
    // exact string opens that floor (ADR-0020).
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = scripted_runs_harness_with(
        ApprovalMode::Auto,
        vec![vec![(
            "__test_shell",
            r#"{"command":"git push --force"}"#.to_owned(),
        )]],
        None,
        Some(reviewer),
    )
    .await;
    let session_id = harness.session_id;
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO session_grants(
                     session_id, kind, value, created_at_ms, source, run_id
                 ) VALUES (?1, 'shell_prefix', 'git push --force', 0, 'delegate', 'seeded')",
                [session_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    submit_prompt(&harness, "force push").await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "Forbidden is settled before any hold"
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref().is_some_and(|result| result.starts_with("forbidden:"))
    )));
    assert!(
        consulted.lock().unwrap().is_empty(),
        "a Forbidden call never reaches the delegate"
    );
}

#[tokio::test]
async fn reviewer_escalation_denies_without_waiting_for_a_client() {
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Escalate {
        reason: "unsure".to_owned(),
    }));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolApprovalResolved { resolution: ApprovalResolution::DeniedByReviewer, tool_call, .. }
            if tool_call.result.as_deref().is_some_and(|result| result.contains("unsure"))
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
    );
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_session_summary_says_who_a_hold_waits_on_until_it_settles() {
    // ENG-1034 (PR #289 review): a client that never loaded the session's
    // body, or attached after the request event, learns from the summary
    // alone whether a held call needs a human.
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Escalate {
        reason: "unsure".to_owned(),
    }));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    let holds = |snapshot: WorkspaceSnapshot| snapshot.focused.unwrap().summary.pending_holds;
    let request = SnapshotRequest {
        workspace_id: harness.workspace_id,
        focused_session_id: Some(harness.session_id),
        include_sessions: Vec::new(),
        session_limit: 1,
        message_limit: 8,
    };
    assert_eq!(
        holds(harness.runtime.snapshot(request.clone()).await.unwrap()),
        [qq_protocol::PendingHold {
            tool_call_id: tool_call.id,
            delegated: true,
        }],
        "the reviewer is deciding"
    );

    release.send(()).unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    // Settled summaries clear the internal review without asking a human.
    for event in &observed {
        if let SessionEvent::RunFinished { session, .. } = &event.event {
            assert!(session.pending_holds.is_empty());
        }
    }
    assert!(holds(harness.runtime.snapshot(request).await.unwrap()).is_empty());
}

#[tokio::test]
async fn a_failed_reviewer_denial_write_fails_instead_of_waiting_for_a_human() {
    // A failed internal-denial transaction must fail the run instead of
    // leaving an unresolved review or falling back to a human.
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Escalate {
        reason: "unsure".to_owned(),
    }));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    collect_until_approval_requested(&mut harness.events).await;
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection
                .execute_batch(
                    r#"CREATE TRIGGER reject_reviewer_denial BEFORE INSERT ON events
                       WHEN NEW.envelope_json LIKE '%"type":"tool_approval_resolved"%'
                         AND NEW.envelope_json LIKE '%"resolution":"denied_by_reviewer"%'
                       BEGIN SELECT RAISE(ABORT, 'injected reviewer denial failure'); END;"#,
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
        .unwrap();
    release.send(()).unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalEscalated { .. }))
    );
    assert!(matches!(
        &observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Failed { .. },
            session,
            ..
        } if session.pending_holds.is_empty()
    ));
}

#[tokio::test]
async fn a_reviewer_denial_is_final_under_auto_and_no_human_is_asked() {
    // A supervised child's reviewer denial is final, names its reason,
    // and lets the model continue without executing the denied action.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Deny {
            reason: "pushes to a shared branch".to_owned(),
        }));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    // The hold is published before the reviewer is consulted (a client may
    // still answer first), and the denial settles it.
    let denied = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::DeniedByReviewer,
                ..
            } => Some(tool_call.clone()),
            _ => None,
        })
        .expect("the reviewer denial is published");
    assert_eq!(denied.state, ToolCallState::Denied);
    let result = denied.result.as_deref().unwrap();
    assert!(result.starts_with(approval::reviewer_denied_result()));
    assert!(result.contains("pushes to a shared branch"));
    assert!(
        !result.contains("supervised"),
        "the denial preserves the reviewer reason: {result}"
    );
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
    assert_eq!(consulted.lock().unwrap().len(), 1);
    // The model saw the denial as a tool error and the run went on.
    {
        let requests = harness.requests.lock().unwrap();
        assert!(
            requests[1]
                .messages()
                .iter()
                .flat_map(Message::content)
                .any(
                    |block| matches!(block, ContentBlock::ToolResult { content, is_error: true, .. }
                if content.starts_with(approval::reviewer_denied_result()))
                )
        );
    }
    // A late client answer finds the call already settled.
    let receipt = respond_approval(
        &harness.runtime,
        harness.run_id,
        denied.id,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(
        matches!(
            receipt,
            Ok(CommandReceipt {
                outcome: CommandOutcome::ToolApprovalResolved {
                    resolution: ApprovalResolution::DeniedByReviewer,
                    ..
                },
                ..
            }) | Err(SessionRuntimeError::ApprovalNotPending)
        ),
        "a settled denial is not reopened by a late approval"
    );
}

#[tokio::test]
async fn a_reviewer_escalation_reason_is_durably_denied_and_cannot_be_overridden() {
    let reason = "the task brief does not authorize this action";
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Escalate {
        reason: reason.to_owned(),
    }));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    let denied = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::DeniedByReviewer,
                delegate: Some(DelegateIdentity::Reviewer),
                tool_call,
                ..
            } => Some(tool_call),
            _ => None,
        })
        .expect("the reviewer's escalation becomes a durable denial");
    assert!(denied.result.as_deref().unwrap().contains(reason));
    let late = respond_approval(
        &harness.runtime,
        harness.run_id,
        denied.id,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(matches!(
        late,
        Ok(CommandReceipt {
            outcome: CommandOutcome::ToolApprovalResolved {
                resolution: ApprovalResolution::DeniedByReviewer,
                ..
            },
            ..
        }) | Err(SessionRuntimeError::ApprovalNotPending)
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reviewer_escalation_settles_even_with_no_human_deadline() {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Escalate {
        reason: "unsure".to_owned(),
    }));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        None,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    collect_until_approval_requested(&mut harness.events).await;
    release.send(()).unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    assert!(observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::DeniedByReviewer,
            ..
        }
    )));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reviewer_timeout_denies_without_waiting_on_a_human() {
    let (reviewer, _release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        None,
        Duration::from_millis(200),
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolApprovalResolved { resolution: ApprovalResolution::DeniedByReviewer, tool_call, .. }
            if tool_call.result.as_deref().is_some_and(|result| result.contains(approval::DELEGATE_TIMED_OUT_REASON))
    )));
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
    );
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_short_human_clock_cannot_replace_the_internal_reviewer_clock() {
    let (reviewer, _release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        Some(Duration::from_millis(1)),
        Duration::from_millis(200),
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolApprovalResolved { resolution: ApprovalResolution::DeniedByReviewer, tool_call, .. }
            if tool_call.result.as_deref().is_some_and(|result| result.contains(approval::DELEGATE_TIMED_OUT_REASON))
    )));
    assert!(!observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolApprovalResolved {
            resolution: ApprovalResolution::DeniedTimeout,
            ..
        }
    )));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interactive_hold_has_no_server_deadline_by_default() {
    // DA2 / RR9: `approval_timeout: None` (the default) means the server
    // never denies a held call on its own clock. The hold outlives what the
    // old 300 s default would have been in miniature, the run stays open,
    // and the human's late answer stands. Cancellation and the run deadline
    // remain the only things that end an unanswered interactive hold.
    let mut harness = approval_harness(ApprovalMode::Ask, "__test_mutate", "{}", 1, None).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    // Nothing arrives while the hold waits: no timeout, no resolution.
    let quiet = tokio::time::timeout(Duration::from_millis(400), harness.events.next()).await;
    assert!(
        quiet.is_err(),
        "no event may settle an unanswered hold: {quiet:?}"
    );
    let receipt = respond_approval(
        &harness.runtime,
        harness.run_id,
        tool_call.id,
        ApprovalDecision::ApproveOnce,
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
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
    )));
}

#[tokio::test]
async fn client_resolution_wins_over_a_late_reviewer_approval() {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
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
    // DA3: with no delegate choice configured, `ask` is the human's mode. A
    // configured reviewer is not consulted, before or after the answer.
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
    let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
    assert!(consulted.lock().unwrap().is_empty());
    // A configured reviewer that is not consulted does not mark the hold.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalRequested {
            delegated: false,
            ..
        }
    )));
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

#[tokio::test]
async fn the_delegate_off_denies_internal_supervision_without_asking_a_human() {
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::Off,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(consulted.lock().unwrap().is_empty());
    assert_no_human_permission_request(&observed);
    assert!(!observed.iter().any(|event| matches!(
        event.event,
        SessionEvent::ToolApprovalRequested { .. } | SessionEvent::ToolCallStarted { .. }
    )));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.is_error
            && tool_call.result.as_deref().is_some_and(|result| result.contains("supervision is unavailable"))
    )));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_mode_executes_without_the_reviewer_even_with_the_delegate_on() {
    // DA3: `full` holds nothing a delegate may decide. Turning the delegate
    // on does not introduce a reviewer round trip into a mode that executes.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Deny {
            reason: "would have denied".to_owned(),
        }));
    let mut harness = approval_harness_with_delegate(
        ApprovalMode::Full,
        "__test_shell",
        r#"{"command":"git push origin main"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
        approval::ApprovalDelegate::On,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(consulted.lock().unwrap().is_empty());
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
async fn the_session_off_switch_withdraws_internal_review_without_a_human_fallback() {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        2,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    collect_until_approval_requested(&mut harness.events).await;
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalDelegate {
                session_id: harness.session_id,
                delegate: Some(approval::ApprovalDelegate::Off),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::ApprovalDelegateSet {
            delegate: Some(approval::ApprovalDelegate::Off),
            ..
        }
    ));
    release.send(()).unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert_no_human_permission_request(&observed);
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
            .count(),
        1,
        "an already requested review remains valid across preference changes"
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.is_error))
            .count(),
        1
    );
    let (_, _, delegate) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert_eq!(delegate, Some(approval::ApprovalDelegate::Off));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_delegate_override_is_inherited_without_making_root_auto_request_approval() {
    // DA6: the override widens as well as narrows. Configured `by-mode`
    // under `ask` asks the human; `set_approval_delegate: on` for the session
    // routes the hold to the reviewer, and a child spawned afterwards starts
    // with the same override.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = scripted_runs_harness_with(
        ApprovalMode::Auto,
        vec![vec![("__test_mutate", "{}".to_owned())]],
        None,
        Some(reviewer),
    )
    .await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalDelegate {
                session_id: harness.session_id,
                delegate: Some(approval::ApprovalDelegate::On),
            },
        )
        .await
        .unwrap();
    let _ = harness.events.next().await.unwrap().unwrap();
    submit_prompt(&harness, "commit").await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(consulted.lock().unwrap().is_empty());
    assert_no_human_permission_request(&observed);
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if !tool_call.is_error
    )));

    let child = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id: harness.workspace_id,
                parent_id: Some(harness.session_id),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::Auto,
                profile: AgentProfileId::default(),
                reasoning_effort: None,
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated {
        session_id: child_id,
    } = child.outcome
    else {
        panic!("unexpected receipt")
    };
    let (_, _, delegate) = harness
        .runtime
        .inner
        .store
        .approval_policy(child_id)
        .await
        .unwrap();
    assert_eq!(delegate, Some(approval::ApprovalDelegate::On));
}

#[tokio::test]
async fn setting_the_delegate_on_an_unknown_session_is_refused() {
    let harness = approval_harness(
        ApprovalMode::Auto,
        "__test_mutate",
        "{}",
        0,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let error = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalDelegate {
                session_id: SessionId::generate().unwrap(),
                delegate: Some(approval::ApprovalDelegate::Off),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, SessionRuntimeError::SessionNotFound),
        "{error:?}"
    );
}

#[test]
fn the_delegate_table_keeps_the_mode_as_the_ceiling() {
    use approval::ApprovalDelegate::{ByMode, Off, On};
    for delegate in [ByMode, On, Off] {
        for mode in [
            ApprovalMode::ReadOnly,
            ApprovalMode::Ask,
            ApprovalMode::Full,
        ] {
            assert!(!delegate.consults_reviewer(mode));
        }
    }
    assert!(ByMode.consults_reviewer(ApprovalMode::Auto));
    assert!(On.consults_reviewer(ApprovalMode::Auto));
    assert!(!Off.consults_reviewer(ApprovalMode::Auto));
}

#[cfg(unix)]
#[tokio::test]
async fn approved_shell_calls_stream_output_and_reuse_exact_command_grants() {
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed {
            tools: Vec::new(),
            shell_prefixes: vec!["rm -f not-present; echo approved-output".to_owned()],
            hosts: Vec::new(),
        },
        WorkspaceGrantOutcome::Failed {
            message: "unused".to_owned(),
        },
    );
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Auto,
        "shell",
        r#"{"command":"rm -f not-present; echo approved-output"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority),
    )
    .await;
    let observed = collect_through_finished_generously(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "the exact command grant must cover the second call"
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
                reasoning_effort: None,
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
                model_work: crate::runtime::ModelWorkDisposition::Committed,
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
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
                notice: None,
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
            ..
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
            ..
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
        Some(Duration::from_millis(50)),
    )
    .await;
    let (_, _) = collect_until_approval_requested(&mut harness.events).await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
            ..
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
#[cfg(feature = "tool-fetch")]
async fn fetch_holds_with_a_host_preview_and_a_host_grant_covers_the_next_call() {
    // `invalid.` is reserved (RFC 6761) and never resolves, so the approved
    // call fails at resolution — a tool error, not a policy event — while
    // everything this test asserts happens before dispatch.
    let mut harness = approval_harness(
        ApprovalMode::Ask,
        "fetch",
        r#"{"url":"https://docs.invalid/axum"}"#,
        1,
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
    collect_through_finished(&mut harness.events).await;
    harness.runtime.shutdown().await.unwrap();
    let authority = ScriptedGrantAuthority::new(
        WorkspaceGrantSeed {
            tools: Vec::new(),
            shell_prefixes: Vec::new(),
            hosts: vec!["*.invalid".to_owned()],
        },
        WorkspaceGrantOutcome::Failed {
            message: "unused".to_owned(),
        },
    );
    let mut harness = approval_harness_with_authority(
        ApprovalMode::Auto,
        "fetch",
        r#"{"url":"https://docs.invalid/axum"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
        Some(authority),
    )
    .await;
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
    let (_, grants, _) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert_eq!(grants.hosts, ["*.invalid"]);
}

#[tokio::test]
#[cfg(feature = "tool-fetch")]
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

#[tokio::test]
#[cfg(feature = "tool-fetch")]
async fn a_blocked_host_never_reaches_the_delegate_even_when_one_would_approve() {
    // Plan acceptance 2, host half: under `auto` an ungranted host is exactly
    // the kind of call the delegate settles, so this is where a permissive
    // delegate could matter. It must not: a private or link-local host is
    // refused before the hold that consults any delegate, the reviewer sees
    // nothing, and no `tool_approval_requested` is published for a client to
    // answer either.
    let (reviewer, consulted) =
        StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = approval_harness_with_delegate(
        ApprovalMode::Auto,
        "fetch",
        r#"{"url":"http://169.254.169.254/latest/meta-data/"}"#,
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        None,
        Some(reviewer),
        approval::ApprovalDelegate::On,
    )
    .await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::ToolApprovalRequested { .. }
                | SessionEvent::ToolApprovalResolved { .. }
                | SessionEvent::ToolApprovalEscalated { .. }
        )),
        "a blocked host is refused before any hold"
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref().is_some_and(|result| {
                    result.starts_with("fetch refused: host 169.254.169.254 resolves to a private")
                })
    )));
    assert!(
        consulted.lock().unwrap().is_empty(),
        "a blocked host never reaches the delegate"
    );
    let (_, grants, _) = harness
        .runtime
        .inner
        .store
        .approval_policy(harness.session_id)
        .await
        .unwrap();
    assert!(
        grants.delegate.hosts.is_empty(),
        "nothing was approved, so nothing was recorded: {grants:?}"
    );
}

#[tokio::test]
async fn goal_approval_wait_pauses_without_another_request_even_at_deadline() {
    struct Authority(u64);
    impl WorkspaceGrantAuthority for Authority {
        fn seed_grants(&self, _: &Path) -> GrantSeedFuture {
            Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
        }
        fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> GrantPromotionFuture {
            panic!("a timed out goal must not promote authority")
        }
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy {
                approval_wait_ms: self.0,
                ..GoalPolicy::default()
            })))
        }
    }
    for (ordinary_wait, goal_wait_ms, deadline_ms, delegate) in [
        (None, 50, 10_000, false),
        (Some(Duration::from_millis(10)), 500, 10_000, false),
        (None, 5000, 1000, false),
        (None, 50, 10_000, true),
        (None, 5000, 1000, true),
    ] {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.approval_timeout = ordinary_wait;
        options.grant_authority = Some(Arc::new(Authority(goal_wait_ms)));
        let (reviewer, _release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
        if delegate {
            options.approval_reviewer = Some(reviewer);
        }
        let runtime = SessionRuntime::open(
            options,
            Arc::new(ApprovalLoader {
                requests: Arc::clone(&requests),
                tool: "__test_mutate",
                arguments: "{}",
                tool_turns: 1,
                delegate: if delegate {
                    approval::ApprovalDelegate::On
                } else {
                    approval::ApprovalDelegate::Off
                },
            }),
        )
        .await
        .unwrap();
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session_with_mode(&runtime, workspace, None, ApprovalMode::Ask).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: workspace,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal","session_id":session_id,"objective":"ask before mutation",
                    "budget":{"max_goal_runs":2,"deadline_ms":now_ms()+deadline_ms}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let (_, call) = collect_until_approval_requested(&mut events).await;
        let observed = collect_through_finished(&mut events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "approval timeout must not ask the model again"
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace, Some(session_id), 1, 1))
            .await
            .unwrap();
        let goal = snapshot.focused.unwrap().goal.unwrap();
        assert!(matches!(goal.status, qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::NeedsUser {
                cause: qq_protocol::GoalNeedsUser::Approval { tool_call_id }
            }
        } if tool_call_id == call.id));
        assert!(observed.iter().any(|event| matches!(
            event.event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::DeniedTimeout,
                ..
            }
        )));
        runtime.shutdown().await.unwrap();
    }
}

// Poll the gate independently of the session's outer cancellation select: both
// consume the same watch signal, so either may observe cancellation first.
#[tokio::test]
async fn an_expired_goal_hold_survives_the_gate_winning_cancellation() {
    for question in [true, false] {
        for (expired, user_cancel) in [(true, false), (false, false), (true, true)] {
            let mut harness = scripted_runs_harness(ApprovalMode::Ask, Vec::new()).await;
            let inner = &harness.runtime.inner;
            let permits = inner
                .permits
                .acquire_many(inner.max_active_runs as u32)
                .await
                .unwrap();
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    serde_json::from_value(serde_json::json!({
                        "type":"set_goal", "session_id":harness.session_id,
                        "objective":"retain the unanswered hold",
                        "budget":{"max_goal_runs":2,"deadline_ms":now_ms()+7_200_000}
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
            inner.store.drive_goals(now_ms()).await.unwrap();
            let claimed = inner.store.claim_next_run(false).await.unwrap().unwrap();
            let call = RuntimeToolCall {
                id: ToolCallId::generate().unwrap(),
                turn_ordinal: 1,
                call_ordinal: 1,
                provider_call_id: "call_1".to_owned(),
                name: if question {
                    "ask_user"
                } else {
                    "__test_mutate"
                }
                .to_owned(),
                effect: crate::catalog::EffectClass::Mutating,
                arguments: if question { ASK_ARGUMENTS } else { "{}" }.to_owned(),
                rejection: None,
            };
            inner
                .store
                .persist_model_turn(
                    &claimed,
                    ModelTurnCommit {
                        model_work: crate::runtime::ModelWorkDisposition::Committed,
                        goal_notices: Vec::new(),
                        reserved_goal_notices: 0,
                        turn_ordinal: 1,
                        message: Message::new(
                            Role::Assistant,
                            vec![ContentBlock::ToolCall {
                                id: call.provider_call_id.clone(),
                                name: call.name.clone(),
                                arguments: serde_json::from_str(&call.arguments).unwrap(),
                            }],
                        ),
                        calls: vec![call.clone()],
                        turn_message: None,
                        context_tokens: None,
                        occupancy_basis: None,
                        usage: None,
                        estimated_cost_usd_nanos: None,
                        accounting: None,
                        truncated: false,
                        notice: None,
                    },
                )
                .await
                .unwrap();
            let (cancel, cancellation) = watch::channel(false);
            let gate = crate::sessions::approvals::SessionToolGate::new(
                Arc::clone(inner),
                claimed.clone(),
                cancellation,
                Arc::new(crate::tools::network::NetworkPolicy::default()),
                approval::ApprovalDelegate::Off,
                false,
            );
            let decision = tokio::spawn(gate.resolve(&call));
            collect_until_approval_requested(&mut harness.events).await;
            if expired {
                inner.goal_deadlines.lock().unwrap().insert(
                    claimed.identity.run_id,
                    crate::runtime::RunDeadline::new(
                        RunLimits {
                            max_duration_ms: Some(1),
                            ..RunLimits::default()
                        },
                        tokio::time::Instant::now() - Duration::from_millis(2),
                    )
                    .unwrap(),
                );
            }
            if user_cancel {
                harness
                    .runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::CancelRun {
                            run_id: claimed.identity.run_id,
                        },
                    )
                    .await
                    .unwrap();
            }
            cancel.send_replace(true);
            assert!(matches!(decision.await.unwrap(), GateDecision::Deny { .. }));
            let goal = harness
                .runtime
                .snapshot(SnapshotRequest::new(
                    harness.workspace_id,
                    Some(harness.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap()
                .focused
                .unwrap()
                .goal
                .unwrap();
            if expired && !user_cancel {
                assert!(
                    matches!(goal.status, qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Approval { tool_call_id }
                    }
                } if tool_call_id == call.id),
                    "the gate discarded its unanswered hold: {:?}",
                    goal.status
                );
            } else {
                assert_eq!(
                    goal.status,
                    if user_cancel {
                        qq_protocol::GoalStatus::Paused {
                            reason: qq_protocol::GoalPauseReason::User,
                        }
                    } else {
                        qq_protocol::GoalStatus::Active
                    },
                    "ordinary cancellation must not invent a deadline hold"
                );
            }
            inner
                .goal_deadlines
                .lock()
                .unwrap()
                .remove(&claimed.identity.run_id);
            inner
                .store
                .finish_run(
                    &claimed,
                    RunOutcome::Cancelled,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            drop(permits);
            harness.runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goal_parent_deadline_preserves_a_sponsored_childs_unanswered_approval() {
    for (run_duration, preparation_ms, wall_clock_lags) in [
        (false, 0, false),
        (true, 0, false),
        (true, 1_200_000, false),
        (true, 0, true),
    ] {
        struct Loader {
            inner: DepthLoader,
            root_loads: AtomicUsize,
            held: StdMutex<
                Option<(
                    tokio::sync::oneshot::Sender<()>,
                    tokio::sync::oneshot::Receiver<()>,
                )>,
            >,
        }
        impl RuntimeLoader for Loader {
            fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                if request.model.model.as_deref() != Some("test/child")
                    && self.root_loads.fetch_add(1, Ordering::SeqCst) > 0
                {
                    return Box::pin(std::future::pending());
                }
                let root = request.model.model.as_deref() != Some("test/child");
                let loaded = self.inner.load(request);
                let held = if root {
                    self.held.lock().unwrap().take()
                } else {
                    None
                };
                Box::pin(async move {
                    if let Some((entered, release)) = held {
                        entered.send(()).unwrap();
                        release.await.unwrap();
                    }
                    loaded.await
                })
            }
        }
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let directory = TempDir::new().unwrap();
        let start = now_ms();
        let epoch = super::goal_driver_soak::Epoch::new(start);
        let parent_requests = Arc::new(StdMutex::new(Vec::new()));
        let child_requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(MeteredDelegationProvider {
            inner: ScriptedRunProvider {
                requests: parent_requests.clone(),
                script: vec![(
                    "spawn_agent",
                    r#"{"task":"ask the operator before proceeding","model":"test/child"}"#
                        .to_owned(),
                )],
                turn: StdMutex::new(0),
            },
            usage: vec![Some(usage(1, 1))],
            turn: AtomicUsize::new(0),
        });
        let child: Arc<dyn Provider> = Arc::new(ApprovalProvider {
            requests: child_requests.clone(),
            turn: StdMutex::new(0),
            tool: "ask_user",
            arguments: r#"{"questions":[{"prompt":"May the sponsored child proceed?"}]}"#,
            tool_turns: 1,
            usage: Some(provider_usage_of(usage(1, 1))),
        });
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.approval_timeout = None;
        let runtime = SessionRuntime::open(
            options,
            Arc::new(Loader {
                inner: DepthLoader {
                    inner: QueueLoader {
                        routed: vec![("test/child", child)],
                        queue: StdMutex::new(vec![parent]),
                    },
                    max_depth: 2,
                    write_children: false,
                    pricing: None,
                },
                root_loads: AtomicUsize::new(0),
                held: StdMutex::new((preparation_ms > 0).then_some((entered, released))),
            }),
        )
        .await
        .unwrap();
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: workspace,
                after: created.committed_through,
            })
            .unwrap();
        runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session_id,
                "objective":"wait for the sponsored child's explicit answer",
                "budget":{"max_goal_runs":10,"max_total_tokens":1_000_000,
                    "deadline_ms":start+if run_duration { 7_200_000 } else { 3_600_000 },
                    "per_run": if run_duration { RunLimits { max_duration_ms: Some(3_600_000), ..RunLimits::default() } } else { RunLimits::default() }}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        if preparation_ms > 0 {
            entry.await.unwrap();
            tokio::time::pause();
            epoch.advance(start + preparation_ms);
            tokio::time::advance(Duration::from_millis(preparation_ms)).await;
            tokio::time::resume();
            release.send(()).unwrap();
        }
        let (observed, call) = collect_until_approval_requested(&mut events).await;
        assert_eq!(call.name, "ask_user");
        assert_ne!(call.session_id, session_id);
        let parent = observed
            .iter()
            .find_map(|event| match event.event {
                SessionEvent::RunStarted { run_id, .. } if event.session_id == session_id => {
                    Some(run_id)
                }
                _ => None,
            })
            .expect("the parent started before the child asked");
        let child = call.run_id;
        let owner: String = runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_paid_owner_run_id FROM runs WHERE id=?1",
                    [child.to_string()],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(owner, parent.to_string());
        assert_eq!(parent_requests.lock().unwrap().len(), 1);
        assert_eq!(child_requests.lock().unwrap().len(), 1);
        // The finite token allowance keeps the read child blocking. Both model
        // requests and the actual held call precede the only time advance.
        tokio::time::pause();
        if !wall_clock_lags {
            epoch.advance(start + 3_600_001);
        }
        tokio::time::advance(Duration::from_millis(3_600_001 - preparation_ms)).await;
        tokio::time::resume();
        let completed = collect_until_run_finished(&mut events, parent).await;
        runtime.shutdown().await.unwrap();
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace, Some(session_id), 8, 16))
            .await
            .unwrap();
        let child_snapshot = runtime
            .snapshot(SnapshotRequest::new(
                workspace,
                Some(call.session_id),
                8,
                16,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(
            child_snapshot
                .runs
                .iter()
                .find(|run| run.id == child)
                .unwrap()
                .outcome,
            Some(RunOutcome::Cancelled),
            "the regression requires parent teardown to cancel the held child"
        );
        assert!(
            completed.iter().any(|event| matches!(
                event.event, SessionEvent::RunFinished { run_id, .. } if run_id == child
            )),
            "the child must settle before its parent's terminal event"
        );
        let goal = snapshot.focused.unwrap().goal.unwrap();
        assert!(
            matches!(goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Approval { tool_call_id }
                    }
                } if tool_call_id == call.id
            ),
            "the sponsored held approval must outrank the owner's deadline (preparation_ms={preparation_ms}, wall_clock_lags={wall_clock_lags}): {:?}",
            goal.status
        );
        assert_eq!(parent_requests.lock().unwrap().len(), 1);
        assert_eq!(child_requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn changing_auto_to_ask_revokes_an_inflight_reviewer_approval() {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let run_id = harness.run_id;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalMode {
                session_id: harness.session_id,
                mode: ApprovalMode::Ask,
            },
        )
        .await
        .unwrap();
    release.send(()).unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed.iter().any(|event| matches!(
            &event.event, SessionEvent::ToolCallStarted { tool_call } if tool_call.id == call.id
        )),
        "a stale reviewer approval must not execute after Ask is selected"
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event, SessionEvent::ToolApprovalResolved { tool_call, resolution: ApprovalResolution::Denied, .. }
            if tool_call.id == call.id && tool_call.state == ToolCallState::Denied
    )), "authority revocation must persist a denied receipt: {observed:?}");
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn readonly_revocation_denies_a_late_human_approval_without_recording_grants() {
    for mode in [ApprovalMode::Ask] {
        for decision in [
            ApprovalDecision::ApproveOnce,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
            ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        ] {
            let mut harness = scripted_runs_harness_with(
                mode,
                vec![vec![("__test_mutate", "{}".to_owned())]],
                None,
                None,
            )
            .await;
            let run_id = submit_prompt(&harness, "mutate").await;
            let (_, call) = collect_until_approval_requested(&mut harness.events).await;
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
            let receipt = respond_approval(&harness.runtime, run_id, call.id, decision)
                .await
                .unwrap();
            assert!(
                matches!(
                    receipt.outcome,
                    CommandOutcome::ToolApprovalResolved {
                        resolution: ApprovalResolution::Denied,
                        ..
                    }
                ),
                "revocation must override a late approval: {receipt:?}"
            );
            let observed = collect_through_finished(&mut harness.events).await;
            assert!(!observed.iter().any(|event| matches!(&event.event,
                SessionEvent::ToolCallStarted { tool_call } if tool_call.id == call.id)));
            assert!(observed.iter().any(|event| matches!(&event.event,
                SessionEvent::ToolApprovalResolved { tool_call, .. }
                    if tool_call.id == call.id && tool_call.result.as_deref().is_some_and(|text| text.contains("permission_denied: authority_changed")))));
            let (_, grants, _) = harness
                .runtime
                .inner
                .store
                .approval_policy(harness.session_id)
                .await
                .unwrap();
            assert!(grants.tools.is_empty());
            harness.runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn selecting_equivalent_delegate_policy_preserves_an_inflight_reviewer_approval() {
    let (reviewer, release) = StubReviewer::held(ReviewVerdict::free(ReviewDecision::Approve));
    let mut harness = supervised_approval_harness(
        "__test_mutate",
        "{}",
        1,
        DEFAULT_APPROVAL_TIMEOUT,
        DEFAULT_DELEGATE_TIMEOUT,
        None,
        reviewer,
        approval::ApprovalDelegate::ByMode,
    )
    .await;
    let run_id = harness.run_id;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetApprovalDelegate {
                session_id: harness.session_id,
                delegate: Some(qq_protocol::ApprovalDelegate::ByMode),
            },
        )
        .await
        .unwrap();
    release.send(()).unwrap();
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(observed.iter().any(|event| matches!(
        &event.event, SessionEvent::ToolApprovalResolved { tool_call, resolution: ApprovalResolution::ApprovedByReviewer, .. }
            if tool_call.id == call.id
    )), "a delegate preference is not an authority change: {observed:?}");
    assert!(observed.iter().any(|event| matches!(
        &event.event, SessionEvent::ToolCallStarted { tool_call } if tool_call.id == call.id
    )));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_close_observes_termination_without_a_settlement() {
    for shutdown in [false, true] {
        let (directory, runtime) = test_runtime().await;
        super::execution_context::process_call_fixture(&runtime.inner.store, directory.path())
            .await;
        let mut closing = Box::pin(runtime.close_when_idle(std::future::pending()));
        assert!(futures_util::poll!(closing.as_mut()).is_pending());
        runtime
            .inner
            .store
            .call(Priority::Control, |_| Ok(()))
            .await
            .unwrap();
        assert!(futures_util::poll!(closing.as_mut()).is_pending());

        if shutdown {
            runtime.inner.shutdown.send_replace(true);
        } else {
            runtime.inner.failed.send_replace(true);
        }
        assert!(matches!(
            futures_util::poll!(closing.as_mut()),
            std::task::Poll::Ready(Err(SessionRuntimeError::Unavailable))
        ));
    }
}

#[tokio::test]
async fn explicit_session_grants_are_bounded_idempotent_and_durable() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("sessions.sqlite3");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    let seed = WorkspaceGrantSeed {
        tools: vec!["one".into(), "one".into(), "two".into()],
        shell_prefixes: vec!["cargo test".into()],
        hosts: vec!["docs.example.org".into()],
    };
    runtime
        .seed_session_grants(session_id, seed.clone())
        .await
        .unwrap();
    runtime.seed_session_grants(session_id, seed).await.unwrap();
    let (_, grants, _) = runtime
        .inner
        .store
        .approval_policy(session_id)
        .await
        .unwrap();
    assert_eq!(
        grants.tools,
        std::collections::HashSet::from(["one".into(), "two".into()])
    );
    assert_eq!(grants.shell_prefixes, ["cargo test"]);
    assert_eq!(grants.hosts, ["docs.example.org"]);
    runtime.close().await.unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let (_, restored, _) = runtime
        .inner
        .store
        .approval_policy(session_id)
        .await
        .unwrap();
    assert_eq!(restored.tools, grants.tools);
    assert_eq!(restored.shell_prefixes, grants.shell_prefixes);
    assert_eq!(restored.hosts, grants.hosts);
    runtime
        .seed_session_grants(
            session_id,
            WorkspaceGrantSeed {
                tools: (0..MAX_SESSION_GRANTS)
                    .map(|n| format!("tool-{n}"))
                    .collect(),
                ..WorkspaceGrantSeed::default()
            },
        )
        .await
        .unwrap();
    runtime
        .seed_session_grants(
            session_id,
            WorkspaceGrantSeed {
                tools: vec!["overflow".into()],
                ..WorkspaceGrantSeed::default()
            },
        )
        .await
        .unwrap();
    let total = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM session_grants WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get::<_, u32>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(total, MAX_SESSION_GRANTS);
    let (_, capped, _) = runtime
        .inner
        .store
        .approval_policy(session_id)
        .await
        .unwrap();
    assert!(!capped.tools.contains("overflow"));
    assert!(capped.tools.contains("one"));
    assert_eq!(capped.shell_prefixes, ["cargo test"]);
    let child = create_session(&runtime, workspace_id, Some(session_id)).await;
    let CommandOutcome::SessionCreated {
        session_id: child_id,
    } = child.outcome
    else {
        panic!("child session")
    };
    assert!(matches!(
        runtime
            .seed_session_grants(
                child_id,
                WorkspaceGrantSeed {
                    tools: vec!["shell".into()],
                    ..WorkspaceGrantSeed::default()
                }
            )
            .await,
        Err(SessionRuntimeError::ChildAuthorityEscalation)
    ));
    let (_, child_grants, _) = runtime.inner.store.approval_policy(child_id).await.unwrap();
    assert!(child_grants.tools.is_empty());
    runtime.shutdown().await.unwrap();
}
