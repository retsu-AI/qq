use super::*;

struct GoalVerdictProvider {
    blocked: bool,
    turns: Mutex<usize>,
}
impl Provider for GoalVerdictProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut turns = self.turns.lock().unwrap();
        let mut events = if request.tools().is_empty() {
            assert!(!self.blocked);
            vec![Ok(ProviderEvent::OutputTextDelta {
                text: r#"{"achieved":true,"evidence":"independently verified the persisted work and evidence"}"#.to_owned(),
            })]
        } else if *turns == 0 {
            assert!(
                request
                    .tools()
                    .iter()
                    .any(|tool| tool.name() == "update_goal")
            );
            vec![
                Ok(ProviderEvent::ToolCallStarted {
                    id: "goal-update".into(),
                    name: "update_goal".into(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "goal-update".into(),
                    json: if self.blocked {
                        r#"{"status":{"type":"blocked","reason":"missing external input"}}"#
                    } else {
                        r#"{"status":{"type":"achieved","evidence":"the requested work is ready"}}"#
                    }
                    .to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "goal-update".into(),
                }),
            ]
        } else {
            vec![Ok(ProviderEvent::OutputTextDelta {
                text: "work ready".into(),
            })]
        };
        *turns += 1;
        events.push(Ok(ProviderEvent::Completed {
            usage: Some(ProviderUsage {
                input_tokens: 5,
                output_tokens: 3,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: None,
            }),
        }));
        Box::pin(stream::iter(events))
    }
}

#[tokio::test]
async fn foreground_goal_achievement_and_blocking_emit_every_direct_run_before_final_state() {
    for blocked in [true, false] {
        let fixture = fixture(move || GoalVerdictProvider {
            blocked,
            turns: Mutex::new(0),
        })
        .await;
        let mut request = goal_request();
        request.max_runs = Some(3);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(10),
            run_goal(
                &fixture.sessions,
                options(&fixture.workspace),
                request,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            code,
            if blocked { 7 } else { 0 },
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let final_record = records.last().unwrap();
        assert_eq!(final_record["type"], "goal");
        assert_eq!(final_record["exit_code"], code);
        let runs: Vec<_> = records
            .iter()
            .filter(|record| record["type"] == "goal_run")
            .collect();
        assert_eq!(runs.len(), if blocked { 1 } else { 2 }, "{records:?}");
        for run in runs {
            assert!(run["run"]["outcome"].is_object());
            assert_eq!(run["goal"], run["run"]["goal"]);
            assert_eq!(run["session_id"], final_record["goal"]["session_id"]);
        }
        assert_eq!(
            final_record["goal"]["status"]["type"],
            if blocked { "blocked" } else { "achieved" }
        );
        if !blocked {
            assert_eq!(final_record["goal"]["counters"]["audits"], 1);
        }
        fixture.sessions.close().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_goal_reopen_replays_work_before_client_adoption() {
    struct CheckAuthority;
    impl qq_core::WorkspacePolicyAuthority for CheckAuthority {
        fn goal_policy(&self, _: &Path) -> qq_core::GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(qq_core::GoalPolicy::default())))
        }
        fn validate_goal_check(&self, _: &Path, command: &str) -> qq_core::GoalCheckPolicyFuture {
            assert_eq!(command, "printf checked");
            Box::pin(std::future::ready(Ok(())))
        }
    }
    for asking in [false, true] {
        let loader: Arc<dyn RuntimeLoader> = if asking {
            Arc::new(ProviderLoader(|| AskingProvider {
                turn: Mutex::new(0),
            }))
        } else {
            Arc::new(ProviderLoader(|| GoalVerdictProvider {
                blocked: false,
                turns: Mutex::new(0),
            }))
        };
        let mut fixture = fixture_with_loader(Arc::clone(&loader)).await;
        let mut opts = options(&fixture.workspace);
        opts.approval = HeadlessApproval::Full;
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        opts.session = Some(handle.session_id);
        let mut request = goal_request();
        request.check = Some("printf checked".into());
        request.max_runs = Some(3);
        assert_eq!(
            run_goal(
                &fixture.sessions,
                opts.clone(),
                request,
                std::future::pending(),
                &mut Vec::new(),
                &mut Vec::new()
            )
            .await,
            6
        );
        fixture.sessions.close().await.unwrap();
        let path = fixture
            ._directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        fixture.sessions = SessionRuntime::open(
            SessionRuntimeOptions::new(path).with_policy_authority(Arc::new(CheckAuthority)),
            loader,
        )
        .await
        .unwrap();
        let snapshot = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap();
        let goal = snapshot.focused.unwrap().goal.unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: snapshot.cursor,
            })
            .unwrap();
        send(
            &fixture.sessions,
            SessionCommand::GoalControl {
                session_id: handle.session_id,
                expected: goal_identity(&goal),
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::ForGoal,
                    command: "printf checked".into(),
                },
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.next().await.unwrap().unwrap().event;
                if (asking && matches!(event, SessionEvent::ToolApprovalRequested { question: Some(_), .. }))
                    || matches!(event, SessionEvent::GoalUpdated { ref goal } if goal.status == qq_protocol::GoalStatus::Achieved) {
                    break;
                }
            }
        }).await.unwrap();
        drop(events);
        retire_delivery_history(&fixture).await;
        let mut resume = goal_request();
        resume.objective = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_goal(
                &fixture.sessions,
                opts,
                resume,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .expect("adoption must consume an already-held input request");
        assert_eq!(
            code,
            if asking { 6 } else { 0 },
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let direct: Vec<_> = records.iter().filter(|r| r["type"] == "goal_run").collect();
        assert_eq!(
            direct.len(),
            if asking { 1 } else { 2 },
            "every startup run must precede the final record"
        );
        if asking {
            assert_eq!(
                records.last().unwrap()["goal"]["status"]["reason"]["cause"]["type"],
                "approval"
            );
        }
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_rejects_preempting_user_work_before_adoption() {
    let fixture = fixture(|| HangingProvider).await;
    let base = options(&fixture.workspace);
    let owner = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
        .await
        .unwrap();
    send(
        &fixture.sessions,
        SessionCommand::SubmitPrompt {
            session_id: owner.session_id,
            input: vec![InputPart::text("question")],
            limits: RunLimits::default(),
            correlation: Default::default(),
            output: None,
        },
    )
    .await
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    send(
        &fixture.sessions,
        SessionCommand::SetGoal {
            check_approval: None,
            session_id: owner.session_id,
            objective: "later objective".into(),
            check: None,
            budget: Box::new(qq_protocol::GoalBudget {
                max_goal_runs: 2,
                deadline_ms: now + 60000,
                max_total_tokens: None,
                max_cost_usd_nanos: None,
                per_run: RunLimits::default(),
            }),
            expected: None,
            replace: false,
        },
    )
    .await
    .unwrap();
    let mut opts = base;
    opts.session = Some(owner.session_id);
    let request = goal_request();
    let result = prepare_session(&fixture.sessions, &opts, SessionIntent::Goal(&request)).await;
    assert!(
        matches!(
            result,
            Err(Failure {
                status: HeadlessStatus::InvalidConfiguration,
                ..
            })
        ),
        "must reject same-session user work before accepting exclusive goal ownership"
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_invalid_resume_does_not_pause_active_goal() {
    for approval in [None, Some(qq_protocol::GoalCheckScope::Once)] {
        let fixture = fixture(|| HangingProvider).await;
        let base = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut args = goal_request();
        let command = crate::goals::prepare_set(
            &crate::cli::GoalSetArgs {
                target: crate::cli::GoalTarget {
                    session: handle.session_id,
                    workspace: None,
                    json: true,
                },
                objective: "keep working".into(),
                replace: false,
                check: None,
                clear_check: false,
                goal_deadline: None,
                goal_max_runs: Some(2),
                max_cost_usd_nanos: None,
            },
            None,
            &args.defaults,
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap(),
        )
        .unwrap();
        let goal = goal_receipt(send(&fixture.sessions, command).await.unwrap()).unwrap();
        args.objective = None;
        args.check = Some("new check".into());
        args.check_approval = approval;
        let mut opts = base;
        opts.model.model = Some("test/changed-model".into());
        opts.approval = HeadlessApproval::Full;
        opts.approval_explicit = true;
        opts.session = Some(handle.session_id);
        let code = run_goal(
            &fixture.sessions,
            opts,
            args,
            std::future::pending(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .await;
        assert_eq!(code, 2);
        let snapshot = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(
            focused.summary.approval_mode,
            qq_protocol::ApprovalMode::ReadOnly
        );
        assert_eq!(
            focused.summary.profile,
            qq_protocol::AgentProfileId::default()
        );
        assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
        let current = focused.goal.unwrap();
        assert_eq!(current.status, qq_protocol::GoalStatus::Active);
        assert_eq!(current.revision, goal.revision);
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_missing_specification_rejects_before_settings_change() {
    for replace in [false, true] {
        let fixture = fixture(|| HangingProvider).await;
        let base = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut opts = base;
        opts.session = Some(handle.session_id);
        opts.model.model = Some("test/changed-model".into());
        opts.profile = qq_protocol::AgentProfileId::new("reviewer").unwrap();
        opts.approval = HeadlessApproval::Full;
        opts.approval_explicit = true;
        let mut request = goal_request();
        request.replace = replace;
        if !replace {
            request.objective = None;
        }
        let mut stdout = Vec::new();
        assert_eq!(
            run_goal(
                &fixture.sessions,
                opts,
                request,
                std::future::pending(),
                &mut stdout,
                &mut Vec::new()
            )
            .await,
            2
        );
        assert!(stdout.is_empty());
        let focused = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert!(focused.goal.is_none());
        assert_eq!(
            focused.summary.approval_mode,
            qq_protocol::ApprovalMode::ReadOnly
        );
        assert_eq!(
            focused.summary.profile,
            qq_protocol::AgentProfileId::default()
        );
        assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_invalid_new_invocations_do_not_create_sessions() {
    for case in ["replace", "runs", "objective", "missing"] {
        let fixture = fixture(|| HangingProvider).await;
        let opts = options(&fixture.workspace);
        let CommandOutcome::WorkspaceResolved { workspace_id } = send(
            &fixture.sessions,
            SessionCommand::ResolveWorkspace {
                path: fixture.workspace.display().to_string(),
            },
        )
        .await
        .unwrap()
        .outcome
        else {
            panic!("workspace");
        };
        let mut request = goal_request();
        match case {
            "replace" => request.replace = true,
            "runs" => request.max_runs = Some(request.defaults.max_runs + 1),
            "objective" => request.objective = Some(String::new()),
            _ => request.objective = None,
        }
        assert_eq!(
            run_goal(
                &fixture.sessions,
                opts,
                request,
                std::future::pending(),
                &mut Vec::new(),
                &mut Vec::new()
            )
            .await,
            2,
            "{case}"
        );
        let snapshot = fixture
            .sessions
            .snapshot(SnapshotRequest::new(workspace_id, None, 16, 16))
            .await
            .unwrap();
        assert!(
            snapshot.sessions.is_empty(),
            "{case}: invalid invocation created a visible session"
        );
        fixture.sessions.close().await.unwrap();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn foreground_goal_text_trace_failure_preserves_summary_and_exit() {
    let fixture = fixture(|| HangingProvider).await;
    let mut opts = options(&fixture.workspace);
    opts.format = HeadlessFormat::Text;
    opts.trace = Some(PathBuf::from("/dev/full"));
    let mut request = goal_request();
    request.check = Some("printf checked".into());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_goal(
        &fixture.sessions,
        opts,
        request,
        std::future::pending(),
        &mut stdout,
        &mut stderr,
    )
    .await;
    assert_eq!(code, 6);
    let summary = String::from_utf8(stdout).unwrap();
    assert_eq!(summary.lines().count(), 1);
    assert!(summary.starts_with("goal "), "{summary}");
    assert!(!stderr.is_empty());
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_without_check_consent_returns_paused_without_model_work() {
    let fixture = fixture(|| TextProvider).await;
    let mut request = goal_request();
    request.check = Some("printf checked".to_owned());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_goal(
            &fixture.sessions,
            options(&fixture.workspace),
            request,
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .unwrap();
    assert_eq!(code, 6, "{}", String::from_utf8_lossy(&stderr));
    let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let Some(qq_protocol::HeadlessRecord::Goal(final_goal)) = records.last() else {
        panic!("final goal record")
    };
    assert_eq!(final_goal.goal.counters.runs, 0);
    assert_eq!(
        final_goal.goal.status,
        qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::NeedsUser {
                cause: qq_protocol::GoalNeedsUser::CheckApproval,
            },
        }
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, qq_protocol::HeadlessRecord::GoalRun(_)))
    );
    let mut options = options(&fixture.workspace);
    options.session = Some(final_goal.goal.session_id);
    let mut resume = goal_request();
    resume.objective = None;
    resume.max_runs = None;
    let mut resumed = Vec::new();
    assert_eq!(
        run_goal(
            &fixture.sessions,
            options,
            resume,
            std::future::pending(),
            &mut resumed,
            &mut stderr
        )
        .await,
        6
    );
    let final_resumed: qq_protocol::HeadlessRecord =
        serde_json::from_str(String::from_utf8(resumed).unwrap().lines().last().unwrap()).unwrap();
    assert!(
        matches!(final_resumed, qq_protocol::HeadlessRecord::Goal(record)
        if record.goal == final_goal.goal)
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_runs_existing_driver_until_budget_and_emits_each_direct_run() {
    let fixture = fixture(|| TextProvider).await;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        run_goal(
            &fixture.sessions,
            options(&fixture.workspace),
            goal_request(),
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .unwrap();
    assert_eq!(code, 8, "{}", String::from_utf8_lossy(&stderr));
    let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let runs: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            qq_protocol::HeadlessRecord::GoalRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1);
    assert!(matches!(
        runs[0].run.origin,
        Some(qq_protocol::RunOrigin::Goal { .. })
    ));
    assert!(runs[0].run.outcome.is_some());
    assert!(
        matches!(records.last(), Some(qq_protocol::HeadlessRecord::Goal(outcome))
        if outcome.exit_code == 8 && outcome.goal.counters.runs == 1)
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_unanswered_question_retains_needs_user_reason() {
    let fixture = fixture(|| AskingProvider {
        turn: Mutex::new(0),
    })
    .await;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_goal(
            &fixture.sessions,
            options(&fixture.workspace),
            goal_request(),
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .unwrap();
    assert_eq!(code, 6, "{}", String::from_utf8_lossy(&stderr));
    let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let Some(qq_protocol::HeadlessRecord::Goal(final_goal)) = records.last() else {
        panic!("final goal record")
    };
    assert!(
        matches!(
            final_goal.goal.status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::NeedsUser {
                    cause: qq_protocol::GoalNeedsUser::Approval { .. },
                },
            }
        ),
        "{:?}",
        final_goal.goal.status
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_cleanup_preserves_an_existing_input_pause() {
    let fixture = fixture(|| AskingProvider {
        turn: Mutex::new(0),
    })
    .await;
    let opts = options(&fixture.workspace);
    let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
        .await
        .unwrap();
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let goal = goal_receipt(
        send(
            &fixture.sessions,
            SessionCommand::SetGoal {
                check_approval: None,
                session_id: handle.session_id,
                objective: "ask for the missing input".into(),
                check: None,
                budget: Box::new(qq_protocol::GoalBudget {
                    max_goal_runs: 2,
                    deadline_ms: now + 60000,
                    max_total_tokens: None,
                    max_cost_usd_nanos: None,
                    per_run: RunLimits::default(),
                }),
                expected: None,
                replace: false,
            },
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let mut guard = AcceptedGoalGuard::new(fixture.sessions.clone(), &goal, &handle);
    let call = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let SessionEvent::ToolApprovalRequested {
                tool_call,
                question: Some(_),
                ..
            } = events.next().await.unwrap().unwrap().event
            {
                break tool_call.id;
            }
        }
    })
    .await
    .unwrap();
    let stopped = goal_receipt(
        send(
            &fixture.sessions,
            SessionCommand::GoalControl {
                session_id: handle.session_id,
                expected: goal_identity(&goal),
                action: qq_protocol::GoalControl::PauseForInput { tool_call_id: call },
            },
        )
        .await
        .unwrap(),
    )
    .unwrap();
    guard.expected = goal_identity(&stopped);
    guard.settle().await.unwrap();
    let actual = fixture
        .sessions
        .snapshot(SnapshotRequest::new(
            handle.workspace_id,
            Some(handle.session_id),
            1,
            1,
        ))
        .await
        .unwrap()
        .focused
        .unwrap()
        .goal
        .unwrap();
    assert_eq!(
        actual.status, stopped.status,
        "cleanup must preserve the actionable input request"
    );
    assert_eq!(actual.revision, stopped.revision);
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_input_controls_and_run_reads_are_identity_fenced() {
    let fixture = fixture(|| AskingProvider {
        turn: Mutex::new(0),
    })
    .await;
    let options = options(&fixture.workspace);
    let owner = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
        .await
        .unwrap();
    let other = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
        .await
        .unwrap();
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: owner.workspace_id,
            after: other.subscribe_after,
        })
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let receipt = send(
        &fixture.sessions,
        SessionCommand::SetGoal {
            check_approval: None,
            session_id: owner.session_id,
            objective: "held goal".into(),
            check: None,
            budget: Box::new(qq_protocol::GoalBudget {
                max_goal_runs: 2,
                deadline_ms: now + 60000,
                max_total_tokens: None,
                max_cost_usd_nanos: None,
                per_run: RunLimits::default(),
            }),
            expected: None,
            replace: false,
        },
    )
    .await
    .unwrap();
    let goal = goal_receipt(receipt).unwrap();
    let expected = goal_identity(&goal);
    send(
        &fixture.sessions,
        SessionCommand::SubmitPrompt {
            session_id: other.session_id,
            input: vec![InputPart::text("foreign question")],
            limits: RunLimits::default(),
            correlation: Default::default(),
            output: None,
        },
    )
    .await
    .unwrap();
    let (own_call, foreign_call) = tokio::time::timeout(Duration::from_secs(5), async {
        let mut own = None;
        let mut foreign = None;
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::ToolApprovalRequested {
                tool_call,
                question: Some(_),
                ..
            } = event.event
            {
                if event.session_id == owner.session_id {
                    own = Some(tool_call);
                } else if event.session_id == other.session_id {
                    foreign = Some(tool_call);
                }
            }
            if let (Some(own), Some(foreign)) = (&own, &foreign) {
                break (own.clone(), foreign.clone());
            }
        }
    })
    .await
    .unwrap();
    let pending = fixture
        .sessions
        .goal_pending_runs(owner.session_id, expected)
        .await
        .unwrap();
    assert!(pending.contains(&own_call.run_id));
    assert!(!pending.contains(&foreign_call.run_id));
    assert!(matches!(
        fixture
            .sessions
            .goal_pending_runs(
                owner.session_id,
                qq_protocol::GoalIdentity {
                    revision: expected.revision + 1,
                    ..expected
                }
            )
            .await,
        Err(SessionRuntimeError::GoalConflict { .. })
    ));
    assert!(
        fixture
            .sessions
            .goal_owns_run(owner.session_id, expected, own_call.run_id)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .sessions
            .goal_owns_run(owner.session_id, expected, foreign_call.run_id)
            .await
            .unwrap()
    );
    let control = |expected, call| SessionCommand::GoalControl {
        session_id: owner.session_id,
        expected,
        action: qq_protocol::GoalControl::PauseForInput { tool_call_id: call },
    };
    assert!(matches!(
        fixture
            .sessions
            .command(
                CommandId::generate().unwrap(),
                control(expected, qq_protocol::ToolCallId::from_bytes([0x99; 16]))
            )
            .await,
        Err(SessionRuntimeError::GoalInputNotPending)
    ));
    assert!(matches!(
        fixture
            .sessions
            .command(
                CommandId::generate().unwrap(),
                control(expected, foreign_call.id)
            )
            .await,
        Err(SessionRuntimeError::GoalInputNotPending)
    ));
    assert!(matches!(
        fixture
            .sessions
            .command(
                CommandId::generate().unwrap(),
                control(
                    qq_protocol::GoalIdentity {
                        revision: expected.revision + 1,
                        ..expected
                    },
                    own_call.id
                )
            )
            .await,
        Err(SessionRuntimeError::GoalConflict { .. })
    ));
    let run = fixture
        .sessions
        .run_snapshot(owner.workspace_id, owner.session_id, own_call.run_id)
        .await
        .unwrap();
    assert_eq!(run.id, own_call.run_id);
    assert!(matches!(
        fixture
            .sessions
            .run_snapshot(
                WorkspaceId::from_bytes([0x90; 16]),
                owner.session_id,
                own_call.run_id
            )
            .await,
        Err(SessionRuntimeError::SessionNotFound)
    ));
    assert!(matches!(
        fixture
            .sessions
            .run_snapshot(owner.workspace_id, other.session_id, own_call.run_id)
            .await,
        Err(SessionRuntimeError::RunNotFound)
    ));
    let id = CommandId::generate().unwrap();
    let command = control(expected, own_call.id);
    let receipt = fixture.sessions.command(id, command.clone()).await.unwrap();
    assert_eq!(
        fixture.sessions.command(id, command).await.unwrap(),
        receipt
    );
    let stopped = goal_receipt(receipt).unwrap();
    assert!(matches!(stopped.status, qq_protocol::GoalStatus::Paused {
        reason: qq_protocol::GoalPauseReason::NeedsUser {
            cause: qq_protocol::GoalNeedsUser::Approval { tool_call_id },
        },
    } if tool_call_id == own_call.id));
    assert!(matches!(
        fixture
            .sessions
            .command(
                CommandId::generate().unwrap(),
                control(goal_identity(&stopped), own_call.id)
            )
            .await,
        Err(SessionRuntimeError::GoalInputNotPending)
    ));
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_interrupt_pauses_and_returns_distinct_exit() {
    let fixture = fixture(|| HangingProvider).await;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_goal(
        &fixture.sessions,
        options(&fixture.workspace),
        goal_request(),
        std::future::ready(()),
        &mut stdout,
        &mut stderr,
    )
    .await;
    assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
    let final_record: qq_protocol::HeadlessRecord =
        serde_json::from_str(String::from_utf8(stdout).unwrap().lines().last().unwrap()).unwrap();
    assert!(
        matches!(final_record, qq_protocol::HeadlessRecord::Goal(record)
        if record.exit_code == 130 && record.goal.status == qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::User,
        })
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_interrupt_reconciles_durable_achievement() {
    struct RefusedLoad(qq_protocol::RunFailureKind);
    impl RuntimeLoader for RefusedLoad {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let kind = self.0;
            Box::pin(std::future::ready(Err(RuntimeLoadError {
                kind,
                message: "recover this configuration or credential".into(),
            })))
        }
    }
    for failure in [
        None,
        Some(qq_protocol::RunFailureKind::Authentication),
        Some(qq_protocol::RunFailureKind::Configuration),
    ] {
        let loader: Arc<dyn RuntimeLoader> = match failure {
            Some(kind) => Arc::new(RefusedLoad(kind)),
            None => Arc::new(ProviderLoader(|| GoalVerdictProvider {
                blocked: false,
                turns: Mutex::new(0),
            })),
        };
        let fixture = fixture_with_loader(loader).await;
        let base = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut observer = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        let stopped = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&stopped);
        let interrupt = async move {
            loop {
                let event = observer.next().await.unwrap().unwrap();
                if let SessionEvent::GoalUpdated { goal } = event.event
                    && goal.status != qq_protocol::GoalStatus::Active
                {
                    *observed.lock().unwrap() = Some((goal.status.clone(), goal.revision));
                    break;
                }
            }
        };
        let mut opts = base;
        opts.session = Some(handle.session_id);
        let mut request = goal_request();
        request.max_runs = Some(3);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_goal(
                &fixture.sessions,
                opts,
                request,
                interrupt,
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            code,
            if failure.is_none() { 0 } else { 6 },
            "{failure:?}: {}",
            String::from_utf8_lossy(&stderr)
        );
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        assert_eq!(records.last().unwrap()["type"], "goal");
        let (status, revision) = stopped.lock().unwrap().take().unwrap();
        assert_eq!(
            records.last().unwrap()["goal"]["status"],
            serde_json::to_value(&status).unwrap()
        );
        assert_eq!(records.last().unwrap()["goal"]["revision"], revision);
        match failure {
            None => assert_eq!(status, qq_protocol::GoalStatus::Achieved),
            Some(qq_protocol::RunFailureKind::Authentication) => assert!(matches!(
                status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Authentication { .. },
                    }
                }
            )),
            Some(_) => assert!(matches!(
                status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Configuration { .. },
                    }
                }
            )),
        }
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_refused_check_consent_is_invalid_configuration() {
    struct DeniedCheck;
    impl qq_core::WorkspacePolicyAuthority for DeniedCheck {
        fn goal_policy(&self, _: &Path) -> qq_core::GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(qq_core::GoalPolicy::default())))
        }
        fn validate_goal_check(&self, _: &Path, _: &str) -> qq_core::GoalCheckPolicyFuture {
            Box::pin(std::future::ready(Err(
                qq_core::GoalCheckPolicyError::ShellDenied,
            )))
        }
    }
    for command in ["sudo printf checked", "printf checked"] {
        let mut fixture = fixture(|| HangingProvider).await;
        fixture.sessions.close().await.unwrap();
        let path = fixture
            ._directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        fixture.sessions = SessionRuntime::open(
            SessionRuntimeOptions::new(path).with_policy_authority(Arc::new(DeniedCheck)),
            Arc::new(ProviderLoader(|| HangingProvider)),
        )
        .await
        .unwrap();
        let mut opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        opts.session = Some(handle.session_id);
        let mut request = goal_request();
        request.check = Some(command.into());
        request.check_approval = Some(qq_protocol::GoalCheckScope::ForGoal);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_goal(
            &fixture.sessions,
            opts,
            request,
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(code, 2, "{command}: {}", String::from_utf8_lossy(&stderr));
        assert!(!stderr.is_empty());
        let snapshot = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert!(
            snapshot.goal.is_none(),
            "refused admission must roll back the goal"
        );
        assert!(
            snapshot.runs.is_empty(),
            "refused admission must not schedule work"
        );
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_interrupt_with_queued_question_replays_to_final_record() {
    let fixture = fixture(|| AskingProvider {
        turn: Mutex::new(0),
    })
    .await;
    let receipt = send(
        &fixture.sessions,
        SessionCommand::ResolveWorkspace {
            path: fixture.workspace.display().to_string(),
        },
    )
    .await
    .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
        panic!("workspace receipt")
    };
    let mut observer = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    let interrupt = async move {
        loop {
            let event = observer.next().await.unwrap().unwrap();
            if matches!(
                event.event,
                SessionEvent::ToolApprovalRequested {
                    question: Some(_),
                    ..
                }
            ) {
                break;
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_goal(
            &fixture.sessions,
            options(&fixture.workspace),
            goal_request(),
            interrupt,
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .unwrap();
    assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
    let records = parse_records(&String::from_utf8(stdout).unwrap());
    assert_eq!(records.last().unwrap()["type"], "goal");
    assert_eq!(records.last().unwrap()["exit_code"], 130);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "goal_run")
            .count(),
        1
    );
    fixture.sessions.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_goal_check_scopes_control_real_repeated_check_execution() {
    const CHECK: &str = "printf x >> checks.log; if test -e checks.flag; then exit 0; else printf ready > checks.flag; exit 1; fi";
    struct CheckAuthority;
    impl qq_core::WorkspacePolicyAuthority for CheckAuthority {
        fn goal_policy(&self, _: &Path) -> qq_core::GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(qq_core::GoalPolicy::default())))
        }
        fn validate_goal_check(
            &self,
            workspace: &Path,
            command: &str,
        ) -> qq_core::GoalCheckPolicyFuture {
            assert!(workspace.is_absolute());
            assert_eq!(command, CHECK);
            Box::pin(std::future::ready(Ok(())))
        }
    }
    for scope in [
        qq_protocol::GoalCheckScope::Once,
        qq_protocol::GoalCheckScope::ForGoal,
    ] {
        let mut fixture = fixture(|| GoalVerdictProvider {
            blocked: false,
            turns: Mutex::new(0),
        })
        .await;
        fixture.sessions.close().await.unwrap();
        let path = fixture
            ._directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        fixture.sessions = SessionRuntime::open(
            SessionRuntimeOptions::new(path).with_policy_authority(Arc::new(CheckAuthority)),
            Arc::new(ProviderLoader(|| GoalVerdictProvider {
                blocked: false,
                turns: Mutex::new(0),
            })),
        )
        .await
        .unwrap();
        let mut opts = options(&fixture.workspace);
        opts.approval = HeadlessApproval::Full;
        let mut request = goal_request();
        request.max_runs = Some(3);
        request.check = Some(CHECK.to_owned());
        request.check_approval = Some(scope);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(10),
            run_goal(
                &fixture.sessions,
                opts,
                request,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        let repeated = scope == qq_protocol::GoalCheckScope::ForGoal;
        assert_eq!(
            code,
            if repeated { 0 } else { 6 },
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let checks: Vec<_> = records
            .iter()
            .filter(|record| {
                record["type"] == "goal_run" && record["run"]["origin"]["type"] == "goal_check"
            })
            .collect();
        assert_eq!(checks.len(), 2);
        assert_ne!(checks[0]["run"]["id"], checks[1]["run"]["id"]);
        assert_eq!(
            records.last().unwrap()["goal"]["counters"]["audits"],
            if repeated { 2 } else { 1 }
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("checks.log")).unwrap(),
            if repeated { "xx" } else { "x" }
        );
        let final_goal = &records.last().unwrap()["goal"];
        assert_eq!(
            final_goal["check_authorization"],
            if repeated { "for_goal" } else { "consumed" }
        );
        assert_eq!(
            final_goal["status"]["type"],
            if repeated { "achieved" } else { "paused" }
        );
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_adopts_active_work_and_leaves_foreign_runs_running() {
    for interrupt_on_close in [false, true] {
        let fixture = fixture(|| HangingProvider).await;
        let base = options(&fixture.workspace);
        let owner = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
            .await
            .unwrap();
        let other = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: owner.workspace_id,
                after: other.subscribe_after,
            })
            .unwrap();
        let foreign = send(
            &fixture.sessions,
            SessionCommand::SubmitPrompt {
                session_id: other.session_id,
                input: vec![InputPart::text("independent work")],
                limits: RunLimits::default(),
                correlation: Default::default(),
                output: None,
            },
        )
        .await
        .unwrap();
        let CommandOutcome::PromptQueued {
            run_id: foreign_id, ..
        } = foreign.outcome
        else {
            panic!("foreign prompt")
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let goal = goal_receipt(
            send(
                &fixture.sessions,
                SessionCommand::SetGoal {
                    check_approval: None,
                    session_id: owner.session_id,
                    objective: "existing active objective".into(),
                    check: None,
                    budget: Box::new(qq_protocol::GoalBudget {
                        max_goal_runs: 2,
                        deadline_ms: now + 60000,
                        max_total_tokens: None,
                        max_cost_usd_nanos: None,
                        per_run: RunLimits::default(),
                    }),
                    expected: None,
                    replace: false,
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut own_started = false;
            let mut other_started = false;
            while !own_started || !other_started {
                let event = events.next().await.unwrap().unwrap();
                if matches!(event.event, SessionEvent::RunStarted { .. }) {
                    own_started |= event.session_id == owner.session_id;
                    other_started |= event.session_id == other.session_id;
                }
            }
        })
        .await
        .unwrap();
        let goal = goal_receipt(
            send(
                &fixture.sessions,
                SessionCommand::SetGoal {
                    check_approval: None,
                    session_id: owner.session_id,
                    objective: "edited while running".into(),
                    check: goal.check.clone(),
                    budget: Box::new(goal.budget.clone()),
                    expected: Some(goal_identity(&goal)),
                    replace: false,
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let mut options = base;
        options.session = Some(owner.session_id);
        let mut request = goal_request();
        request.objective = None;
        request.max_runs = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_goal(
            &fixture.sessions,
            options,
            request,
            std::future::ready(()),
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let finished = &records.last().unwrap()["goal"];
        assert_eq!(
            finished["goal_id"],
            serde_json::to_value(goal.goal_id).unwrap()
        );
        assert_eq!(finished["objective"], goal.objective);
        assert_eq!(
            finished["budget"],
            serde_json::to_value(&goal.budget).unwrap()
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record["type"] == "goal_run")
                .count(),
            1
        );
        let adopted = records
            .iter()
            .find(|record| record["type"] == "goal_run")
            .unwrap();
        assert_eq!(adopted["goal"]["revision"], goal.revision - 1);
        assert_eq!(
            adopted["goal"]["goal_id"],
            serde_json::to_value(goal.goal_id).unwrap()
        );
        assert_eq!(
            fixture
                .sessions
                .run_snapshot(owner.workspace_id, other.session_id, foreign_id)
                .await
                .unwrap()
                .status,
            qq_protocol::RunStatus::Running
        );
        let (interrupt, interrupted) = tokio::sync::oneshot::channel();
        let closing = crate::close_headless(&fixture.sessions, true, async {
            if interrupt_on_close {
                interrupted.await.unwrap();
            } else {
                std::future::pending::<()>().await;
            }
        });
        tokio::pin!(closing);
        assert!(futures_util::poll!(closing.as_mut()).is_pending());
        let inspect_and_finish = async {
            assert_eq!(
                fixture
                    .sessions
                    .run_snapshot(owner.workspace_id, other.session_id, foreign_id)
                    .await
                    .unwrap()
                    .status,
                qq_protocol::RunStatus::Running
            );
            if interrupt_on_close {
                interrupt.send(()).unwrap();
                loop {
                    let event = events.next().await.unwrap().unwrap();
                    if let SessionEvent::RunFinished {
                        run_id, outcome, ..
                    } = event.event
                        && run_id == foreign_id
                    {
                        assert!(matches!(outcome, qq_protocol::RunOutcome::Cancelled));
                        break;
                    }
                }
            } else {
                request_cancel(&fixture.sessions, foreign_id).await.unwrap();
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(closing, inspect_and_finish)
        })
        .await;
        if result.is_err() {
            fixture.sessions.close().await.unwrap();
        }
        let (closed, ()) =
            result.expect("runtime close must react to the new interrupt after the goal settles");
        closed.unwrap();
    }
}

#[tokio::test]
async fn closing_goal_host_drains_accepted_work_without_admitting_successors() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct ReleasedRun {
        calls: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Semaphore>,
        started: tokio::sync::mpsc::Sender<usize>,
    }
    impl Provider for ReleasedRun {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.try_send(call).unwrap();
            let release = self.release.clone();
            if call == 0 {
                Box::pin(
                    stream::iter([Ok(ProviderEvent::OutputTextDelta {
                        text: "accepted work finished".into(),
                    })])
                    .chain(stream::once(async move {
                        release.acquire().await.unwrap().forget();
                        Ok(ProviderEvent::Completed {
                            usage: Some(ProviderUsage {
                                input_tokens: 1,
                                output_tokens: 0,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_tokens: None,
                            }),
                        })
                    })),
                )
            } else {
                Box::pin(stream::pending())
            }
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (started, mut starts) = tokio::sync::mpsc::channel(8);
        let fixture = fixture({
            let calls = calls.clone(); let release = release.clone();
            move || ReleasedRun { calls: calls.clone(), release: release.clone(), started: started.clone() }
        }).await;
        let opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt).await.unwrap();
        let mut request = goal_request();
        request.max_runs = Some(3);
        let command = prepare_goal_request(&request, &opts, handle.session_id, None).unwrap().unwrap();
        send(&fixture.sessions, command).await.unwrap();
        assert_eq!(starts.recv().await, Some(0));
        let closing = fixture.sessions.close_when_idle(std::future::pending());
        tokio::pin!(closing);
        assert!(futures_util::poll!(closing.as_mut()).is_pending());
        tokio::select! {
            result = &mut closing => panic!("closed before accepted work settled: {result:?}"),
            snapshot = fixture.sessions.snapshot(SnapshotRequest::new(handle.workspace_id, Some(handle.session_id), 1, 1)) => { snapshot.unwrap(); }
        }
        release.add_permits(1);
        tokio::select! {
            biased;
            call = starts.recv() => panic!("close admitted successor call {call:?}"),
            result = &mut closing => result.unwrap(),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }).await.expect("close must drain accepted work without autonomous refill");
}

#[tokio::test]
async fn foreground_goal_adopts_active_edits_for_interrupt_and_cleanup() {
    struct ObservedRevision {
        output: ForegroundOutput,
        seen: Option<tokio::sync::oneshot::Sender<()>>,
        fail_edit: bool,
    }
    impl Write for ObservedRevision {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let edited = serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|record| {
                record["envelope"]["event"]["type"] == "goal_updated"
                    && record["envelope"]["event"]["goal"]["revision"].as_u64() == Some(2)
            });
            if edited && self.fail_edit {
                self.fail_edit = false;
                return Err(io::Error::other("injected edit-event output failure"));
            }
            let count = self.output.write(bytes)?;
            if edited && let Some(seen) = self.seen.take() {
                let _ = seen.send(());
            }
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.output.flush()
        }
    }
    for action in ["interrupt", "drop", "output"] {
        let fixture = fixture(|| HangingProvider).await;
        let mut opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        opts.session = Some(handle.session_id);
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        let (seen, observed) = tokio::sync::oneshot::channel();
        let (interrupt, interrupted) = tokio::sync::oneshot::channel();
        let mut writer = ObservedRevision {
            output: ForegroundOutput::default(),
            seen: Some(seen),
            fail_edit: action == "output",
        };
        let mut stderr = Vec::new();
        let mut foreground = Box::pin(run_goal(
            &fixture.sessions,
            opts,
            goal_request(),
            async {
                interrupted.await.unwrap();
            },
            &mut writer,
            &mut stderr,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop { tokio::select! {
                code = &mut foreground => panic!("returned before edit: {code}"),
                event = events.next() => if matches!(event.unwrap().unwrap().event, SessionEvent::RunStarted { .. }) { break; },
            } }
        }).await.unwrap();
        let current = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap()
            .focused
            .unwrap()
            .goal
            .unwrap();
        let edited = goal_receipt(
            send(
                &fixture.sessions,
                SessionCommand::SetGoal {
                    check_approval: None,
                    session_id: handle.session_id,
                    objective: "edited while adopted".into(),
                    check: None,
                    budget: Box::new(current.budget.clone()),
                    expected: Some(goal_identity(&current)),
                    replace: false,
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        if action == "output" {
            let code = tokio::time::timeout(Duration::from_secs(5), foreground)
                .await
                .unwrap();
            assert_eq!(code, 4, "{}", String::from_utf8_lossy(&stderr));
            let stopped = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap()
                .focused
                .unwrap()
                .goal
                .unwrap();
            assert!(
                matches!(
                    stopped.status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::User,
                    }
                ),
                "edit-event output failure left adopted goal running: {stopped:?}; {}",
                String::from_utf8_lossy(&stderr)
            );
            assert!(stopped.pending_run_id.is_none());
        } else {
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! { result = observed => result.unwrap(),
                code = &mut foreground => panic!("returned while observing edit: {code}") }
            })
            .await
            .unwrap();
            if action == "drop" {
                drop(foreground);
            } else {
                interrupt.send(()).unwrap();
                let code = tokio::time::timeout(Duration::from_secs(5), foreground)
                    .await
                    .unwrap();
                assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
            }
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let SessionEvent::GoalUpdated { goal } =
                    events.next().await.unwrap().unwrap().event
                    && goal.goal_id == edited.goal_id
                    && goal.revision > edited.revision
                    && matches!(
                        goal.status,
                        qq_protocol::GoalStatus::Paused {
                            reason: qq_protocol::GoalPauseReason::User
                        }
                    )
                {
                    break;
                }
            }
        })
        .await
        .expect("edited adopted goal must be paused by its owner");
        fixture.sessions.close().await.unwrap();
    }
}

#[derive(Clone, Default)]
struct ForegroundOutput {
    bytes: Arc<Mutex<Vec<u8>>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Write for ForegroundOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other("injected output failure"));
        }
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn foreground_goal_held_family_drains_before_terminal_output() {
    use futures_util::FutureExt;

    for action in [
        "interrupt",
        "pause",
        "cancel",
        "clear",
        "replace",
        "output",
        "abort",
    ] {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (entered, entry) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let (exited, exit) = tokio::sync::oneshot::channel();
            let fixture = fixture_with_loader(Arc::new(ParentChildLoader {
                parent: Arc::new(SpawnsLoopingChildProvider::new()),
                child: Arc::new(HangingProvider),
                held_child: Some(Arc::new(HeldChildPreparation {
                    entered: Mutex::new(Some(entered)),
                    release: Mutex::new(Some(released)),
                    exited: Mutex::new(Some(exited)),
                })),
            })).await;
            let mut opts = options(&fixture.workspace);
            opts.approval = HeadlessApproval::Full;
            let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
                .await.unwrap();
            opts.session = Some(handle.session_id);
            let mut foreign_opts = opts.clone();
            foreign_opts.session = None;
            foreign_opts.model.model = Some("test/foreign".into());
            let foreign = prepare_session(&fixture.sessions, &foreign_opts, SessionIntent::Prompt)
                .await.unwrap();
            let receipt = send(&fixture.sessions, SessionCommand::SubmitPrompt {
                session_id: foreign.session_id,
                input: vec![InputPart::text("independent work")],
                limits: RunLimits::default(), correlation: Default::default(), output: None,
            }).await.unwrap();
            let CommandOutcome::PromptQueued { run_id: foreign_run, .. } = receipt.outcome else {
                panic!("foreign prompt")
            };
            let (interrupt, interrupted) = tokio::sync::oneshot::channel();
            let output = ForegroundOutput::default();
            let mut writer = output.clone();
            let mut stderr = Vec::new();
            let mut foreground = Box::pin(run_goal(
                &fixture.sessions, opts, goal_request(),
                async { interrupted.await.unwrap(); }, &mut writer, &mut stderr,
            ));
            let progress = tokio::select! {
                entry = entry => entry.unwrap(),
                code = &mut foreground => panic!("{action}: returned before child entry: {code}"),
            };
            let snapshot = fixture.sessions.snapshot(SnapshotRequest::new(
                handle.workspace_id, Some(handle.session_id), 16, 16,
            )).await.unwrap();
            let owner = snapshot.focused.unwrap();
            let goal = owner.goal.unwrap();
            let expected = goal_identity(&goal);
            let root = owner.summary.active_run_id.expect("parent retains child ownership");
            let pending = fixture.sessions.goal_pending_runs(handle.session_id, expected).await.unwrap();
            assert_eq!(pending.len(), 2, "{action}: root and admitted sponsored child");
            let child = *pending.iter().find(|id| **id != root).unwrap();
            assert!(fixture.sessions.goal_owns_run(handle.session_id, expected, child).await.unwrap());
            let mut events = fixture.sessions.subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id, after: snapshot.cursor,
            }).unwrap();

            let early_code: Option<u8> = match action {
                "interrupt" | "clear" | "replace" => {
                    interrupt.send(()).unwrap();
                    None
                }
                "pause" => {
                    send(&fixture.sessions, SessionCommand::GoalControl {
                        session_id: handle.session_id, expected, action: qq_protocol::GoalControl::Pause,
                    }).await.unwrap();
                    None
                }
                "cancel" => {
                    request_cancel(&fixture.sessions, root).await.unwrap();
                    None
                }
                "output" => {
                    output.fail.store(true, std::sync::atomic::Ordering::SeqCst);
                    None
                }
                "abort" => None,
                _ => unreachable!(),
            };
            if action == "abort" || early_code.is_some() {
                drop(foreground);
            } else {
                tokio::select! {
                    () = progress.cancelled() => {},
                    code = &mut foreground => panic!("{action}: returned while child preparation held: {code}"),
                }
                assert!(foreground.as_mut().now_or_never().is_none(),
                    "{action}: foreground must remain pending after cancellation receipt");
                let stopped = fixture.sessions.snapshot(SnapshotRequest::new(
                    handle.workspace_id, Some(handle.session_id), 1, 1,
                )).await.unwrap().focused.unwrap().goal.unwrap();
                if action != "cancel" {
                    let error = fixture.sessions.command(CommandId::generate().unwrap(), SessionCommand::GoalControl {
                        session_id: handle.session_id, expected: goal_identity(&stopped),
                        action: qq_protocol::GoalControl::Resume,
                    }).await.unwrap_err();
                    assert!(matches!(error, SessionRuntimeError::GoalCancellationPending));
                }
                assert_eq!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&stopped)).await.unwrap().len(), 2);
                if action == "clear" {
                    send(&fixture.sessions, SessionCommand::GoalControl {
                        session_id: handle.session_id, expected: goal_identity(&stopped),
                        action: qq_protocol::GoalControl::Clear,
                    }).await.unwrap();
                } else if action == "replace" {
                    send(&fixture.sessions, SessionCommand::SetGoal {
                        check_approval: None,
                        session_id: handle.session_id, objective: "replacement must survive".into(),
                        check: Some(qq_protocol::GoalCheck {
                            command: "printf checked".into(), timeout_ms: 1000,
                        }),
                        budget: Box::new(goal.budget.clone()), expected: Some(goal_identity(&stopped)), replace: true,
                    }).await.unwrap();
                }
                if matches!(action, "clear" | "replace") {
                    let mut draining = fixture.sessions.cancelled_goal_runs(handle.session_id, expected.goal_id).await.unwrap();
                    let mut family = vec![root, child];
                    draining.sort();
                    family.sort();
                    assert_eq!(draining, family);
                    assert!(fixture.sessions.cancelled_goal_runs(foreign.session_id, expected.goal_id).await.unwrap().is_empty());
                }
                assert!(foreground.as_mut().now_or_never().is_none());
                assert!(!parse_records(&String::from_utf8(output.bytes.lock().unwrap().clone()).unwrap())
                    .iter().any(|record| record["type"] == "goal"));
                release.send(()).unwrap();
                exit.await.unwrap();
                let code = foreground.await;
                let records = parse_records(&String::from_utf8(output.bytes.lock().unwrap().clone()).unwrap());
                let finals: Vec<_> = records.iter().filter(|r| r["type"] == "goal").collect();
                if matches!(action, "clear" | "replace") {
                    assert_eq!(code, 4);
                    assert_eq!(finals.len(), 1, "{action}: {records:?}");
                    assert_eq!(records.last().unwrap()["type"], "goal");
                    assert_eq!(finals[0]["exit_code"], 4);
                    assert_eq!(finals[0]["status"], "harness_failure");
                    assert_eq!(finals[0]["goal"]["goal_id"], serde_json::to_value(expected.goal_id).unwrap());
                    let errors = String::from_utf8_lossy(&stderr);
                    assert!(errors.contains(if action == "clear" { "goal was not found" } else { "goal identity or revision conflicts with the current goal" })
                        || errors.contains("goal changed while terminal work drained"), "{action}: {errors}");
                } else if action == "output" {
                    assert_eq!(code, 4);
                    assert!(finals.is_empty(), "broken stdout must not be retried");
                } else {
                    assert_eq!(code, if action == "interrupt" { 130 } else { 6 });
                    assert_eq!(finals.len(), 1, "{action}: {records:?}");
                    assert_eq!(records.last().unwrap()["type"], "goal");
                    assert_eq!(finals[0]["exit_code"], code);
                    assert_eq!(finals[0]["status"], if action == "interrupt" { "interrupted" } else { "paused" });
                    let direct: Vec<_> = records.iter().enumerate().filter(|(_, r)| r["type"] == "goal_run").collect();
                    assert_eq!(direct.len(), 1, "descendants must not produce direct-run records");
                    assert_eq!(direct[0].1["goal"], serde_json::to_value(expected).unwrap());
                    assert_eq!(direct[0].1["run"]["id"], serde_json::to_value(root).unwrap());
                    let finished = records.iter().position(|r| r["envelope"]["event"]["type"] == "run_finished"
                        && r["envelope"]["run_id"] == serde_json::to_value(root).unwrap()).unwrap();
                    assert!(finished < direct[0].0 && direct[0].0 < records.len() - 1);
                    let sequences: Vec<_> = records.iter().filter(|r| r["type"] == "event")
                        .map(|r| r["envelope"]["cursor"]["sequence"].as_u64().expect("event sequence")).collect();
                    assert!(sequences.len() >= 2);
                    assert!(sequences.windows(2).all(|w| w[0] < w[1]));
                }
                let durable = fixture.sessions.snapshot(SnapshotRequest::new(
                    handle.workspace_id, Some(handle.session_id), 1, 16,
                )).await.unwrap().focused.unwrap();
                if action == "replace" {
                    if let Some(run) = durable.summary.active_run_id {
                        assert_ne!(run, root, "the old family must have drained");
                        let replacement = fixture.sessions.run_snapshot(handle.workspace_id, handle.session_id, run).await.unwrap();
                        assert!(matches!(replacement.origin,
                            Some(qq_protocol::RunOrigin::Goal { goal_id, .. })
                            if goal_id == durable.goal.as_ref().unwrap().goal_id));
                    }
                } else {
                    assert_eq!(durable.summary.active_run_id, None, "{action}");
                }
                if action == "clear" {
                    assert!(durable.goal.is_none());
                } else {
                    let final_goal = durable.goal.unwrap();
                    if action == "replace" {
                        assert_ne!(final_goal.goal_id, expected.goal_id);
                        assert_eq!(final_goal.objective, "replacement must survive");
                    } else {
                        assert!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&final_goal)).await.unwrap().is_empty());
                    }
                }
                let mut finished = Vec::new();
                while finished.len() < 2 {
                    let event = events.next().await.unwrap().unwrap();
                    if let SessionEvent::RunFinished { run_id, .. } = event.event
                        && [root, child].contains(&run_id) {
                        finished.push(run_id);
                    }
                }
                assert_eq!(finished, vec![child, root], "child settles before parent publication");
                assert_eq!(fixture.sessions.run_snapshot(handle.workspace_id, foreign.session_id, foreign_run).await.unwrap().status, RunStatus::Running);
                fixture.sessions.close().await.unwrap();
                return;
            }
            progress.cancelled().await;
            assert_eq!(early_code, (action == "output").then_some(4));
            let stopped = fixture.sessions.snapshot(SnapshotRequest::new(
                handle.workspace_id, Some(handle.session_id), 1, 1,
            )).await.unwrap().focused.unwrap().goal.unwrap();
            assert_eq!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&stopped)).await.unwrap().len(), 2);
            assert!(!parse_records(&String::from_utf8(output.bytes.lock().unwrap().clone()).unwrap())
                .iter().any(|record| record["type"] == "goal"));
            release.send(()).unwrap();
            exit.await.unwrap();
            let mut finished = Vec::new();
            while finished.len() < 2 {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::RunFinished { run_id, .. } = event.event
                    && [root, child].contains(&run_id) { finished.push(run_id); }
            }
            assert_eq!(finished, vec![child, root]);
            let durable = fixture.sessions.snapshot(SnapshotRequest::new(
                handle.workspace_id, Some(handle.session_id), 1, 16,
            )).await.unwrap().focused.unwrap();
            let durable_goal = durable.goal.unwrap();
            assert_eq!(durable_goal.status, qq_protocol::GoalStatus::Paused { reason: qq_protocol::GoalPauseReason::User });
            assert!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&durable_goal)).await.unwrap().is_empty());
            assert_eq!(fixture.sessions.run_snapshot(handle.workspace_id, foreign.session_id, foreign_run).await.unwrap().status, RunStatus::Running);
            fixture.sessions.close().await.unwrap();
        }).await.unwrap_or_else(|_| panic!("{action}: held-family acceptance hung"));
    }
}

#[tokio::test]
async fn foreground_goal_harness_failure_still_emits_final_record() {
    let fixture = fixture(|| HangingProvider).await;
    let opts = options(&fixture.workspace);
    let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
        .await
        .unwrap();
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    let stop = async {
        loop {
            if matches!(
                events.next().await.unwrap().unwrap().event,
                SessionEvent::RunStarted { .. }
            ) {
                break;
            }
        }
        fixture.sessions.close().await.unwrap();
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut opts = opts;
    opts.session = Some(handle.session_id);
    assert_eq!(
        run_goal(
            &fixture.sessions,
            opts,
            goal_request(),
            stop,
            &mut stdout,
            &mut stderr
        )
        .await,
        4
    );
    let records = parse_records(&String::from_utf8(stdout).unwrap());
    assert_eq!(records.last().unwrap()["type"], "goal");
    assert_eq!(records.last().unwrap()["status"], "harness_failure");
    assert_eq!(records.last().unwrap()["exit_code"], 4);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn foreground_goal_early_trace_failure_keeps_final_stdout_usable() {
    struct LargeText {
        release: Arc<tokio::sync::Notify>,
    }
    impl Provider for LargeText {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let release = Arc::clone(&self.release);
            Box::pin(
                stream::once(async move {
                    release.notified().await;
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "x".repeat(16384),
                    })
                })
                .chain(stream::pending()),
            )
        }
    }
    let release = Arc::new(tokio::sync::Notify::new());
    let provider_release = Arc::clone(&release);
    let fixture = fixture(move || LargeText {
        release: Arc::clone(&provider_release),
    })
    .await;
    let mut opts = options(&fixture.workspace);
    let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
        .await
        .unwrap();
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    let mut request = goal_request();
    request.objective = Some("x".repeat(2048));
    let command = prepare_goal_request(&request, &opts, handle.session_id, None)
        .unwrap()
        .unwrap();
    send(&fixture.sessions, command).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.next().await.unwrap().unwrap().event,
                SessionEvent::RunStarted { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    opts.session = Some(handle.session_id);
    opts.trace = Some(PathBuf::from("/dev/full"));
    request.objective = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        run_goal(
            &fixture.sessions,
            opts,
            request,
            async {
                release.notify_one();
                std::future::pending::<()>().await;
            },
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .expect("trace failure cleanup must finish");
    assert_eq!(code, 4);
    let records = parse_records(&String::from_utf8(stdout).unwrap());
    let finals: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "goal")
        .collect();
    assert_eq!(
        finals.len(),
        1,
        "usable stdout needs a final harness result"
    );
    assert_eq!(records.last().unwrap()["type"], "goal");
    assert_eq!(finals[0]["status"], "harness_failure");
    assert_eq!(finals[0]["exit_code"], 4);
    assert!(!stderr.is_empty());
    let runs: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "goal_run")
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "error cleanup must emit the settled direct run"
    );
    assert!(runs[0]["run"]["outcome"].is_object());
    assert!(finals[0]["goal"]["pending_run_id"].is_null());
    let durable = fixture
        .sessions
        .snapshot(SnapshotRequest::new(
            handle.workspace_id,
            Some(handle.session_id),
            1,
            1,
        ))
        .await
        .unwrap()
        .focused
        .unwrap()
        .goal
        .unwrap();
    assert_eq!(
        finals[0]["goal"]["counters"],
        serde_json::to_value(durable.counters).unwrap()
    );
    fixture.sessions.close().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn foreground_goal_trace_flush_does_not_duplicate_the_final_record() {
    let fixture = fixture(|| HangingProvider).await;
    let mut opts = options(&fixture.workspace);
    opts.trace = Some(PathBuf::from("/dev/full"));
    let mut request = goal_request();
    request.check = Some("printf checked".into());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_goal(
        &fixture.sessions,
        opts,
        request,
        std::future::pending(),
        &mut stdout,
        &mut stderr,
    )
    .await;
    let records = parse_records(&String::from_utf8(stdout).unwrap());
    let finals: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "goal")
        .collect();
    assert_eq!(
        finals.len(),
        1,
        "a flushed final record must never be retried"
    );
    assert_eq!(finals[0]["exit_code"], code);
    assert_eq!(code, 6);
    assert!(
        !stderr.is_empty(),
        "the trace failure must still be reported"
    );
    let goal: Box<qq_protocol::GoalSnapshot> =
        serde_json::from_value(finals[0]["goal"].clone()).unwrap();
    let workspace = serde_json::from_value(finals[0]["workspace_id"].clone()).unwrap();
    let mut trace = BufWriter::with_capacity(8192, std::fs::File::create("/dev/full").unwrap());
    trace.write_all(&vec![b' '; 8191]).unwrap();
    let mut sink = RecordSink {
        goal_record: GoalRecordState::Pending,
        stdout_failed: false,
        trace: Some(trace),
        to_stdout: true,
    };
    let mut final_stdout = Vec::new();
    assert!(
        finish_goal(
            &mut sink,
            &mut final_stdout,
            &options(&fixture.workspace),
            workspace,
            goal,
            None
        )
        .is_err()
    );
    let records = parse_records(&String::from_utf8(final_stdout).unwrap());
    assert_eq!(
        records.len(),
        1,
        "final stdout must survive an implicit trace-buffer flush failure"
    );
    assert_eq!(records[0]["exit_code"], 6);
    assert!(matches!(sink.goal_record, GoalRecordState::Emitted(6)));
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_goal_output_failure_pauses_active_work_and_preserves_stopped_reason() {
    for needs_consent in [false, true] {
        let fixture = fixture(|| HangingProvider).await;
        let mut options = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
            .await
            .unwrap();
        options.session = Some(handle.session_id);
        let mut request = goal_request();
        if needs_consent {
            request.check = Some("printf checked".into());
        }
        let mut stderr = Vec::new();
        assert_eq!(
            run_goal(
                &fixture.sessions,
                options,
                request,
                std::future::pending(),
                &mut BrokenWriter,
                &mut stderr
            )
            .await,
            4
        );
        assert!(String::from_utf8_lossy(&stderr).contains("injected output failure"));
        let snapshot = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap();
        let goal = snapshot.focused.unwrap().goal.unwrap();
        assert_eq!(
            goal.status,
            qq_protocol::GoalStatus::Paused {
                reason: if needs_consent {
                    qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::CheckApproval,
                    }
                } else {
                    qq_protocol::GoalPauseReason::User
                }
            }
        );
        assert_eq!(goal.revision, if needs_consent { 1 } else { 2 });
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_goal_owner_abort_durably_pauses_accepted_goal() {
    struct AcceptanceSignal(Option<tokio::sync::oneshot::Sender<()>>);
    impl Write for AcceptanceSignal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if let Some(entered) = self.0.take() {
                entered.send(()).unwrap();
            }
            Ok(())
        }
    }
    let fixture = fixture(|| HangingProvider).await;
    let mut options = options(&fixture.workspace);
    let handle = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
        .await
        .unwrap();
    options.session = Some(handle.session_id);
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    let (entered, accepted) = tokio::sync::oneshot::channel();
    let sessions = fixture.sessions.clone();
    let owner = tokio::spawn(async move {
        run_goal(
            &sessions,
            options,
            goal_request(),
            std::future::pending(),
            &mut AcceptanceSignal(Some(entered)),
            &mut Vec::new(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), accepted)
        .await
        .unwrap()
        .unwrap();
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::GoalUpdated { goal } = event.event
                && goal.session_id == handle.session_id
                && goal.status
                    == (qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::User,
                    })
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    fixture.sessions.close().await.unwrap();
}
