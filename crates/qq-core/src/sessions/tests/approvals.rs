use super::*;
use crate::test_support::within;

struct AdmissionLoader {
    question: bool,
    starts: mpsc::Sender<(String, usize)>,
}

impl RuntimeLoader for AdmissionLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        load_provider(
            AdmissionProvider {
                question: self.question,
                starts: self.starts.clone(),
                turn: AtomicUsize::new(0),
            },
            &request,
        )
    }
}

struct AdmissionProvider {
    question: bool,
    starts: mpsc::Sender<(String, usize)>,
    turn: AtomicUsize,
}

impl Provider for AdmissionProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let prompt = request_texts(&request)[0].clone();
        let turn = self.turn.fetch_add(1, Ordering::SeqCst);
        self.starts.try_send((prompt.clone(), turn)).unwrap();
        if prompt.starts_with("held") && turn == 0 {
            provider_script(
                tool_call(
                    "hold",
                    if self.question {
                        "ask_user"
                    } else {
                        "__test_mutate"
                    },
                    if self.question { ASK_ARGUMENTS } else { "{}" },
                )
                .into_iter()
                .chain([completed(None)]),
            )
        } else {
            Box::pin(stream::pending())
        }
    }
}

#[tokio::test]
async fn admission_holds_release_slots_and_answers_resume_before_new_runs() {
    for question in [false, true] {
        let (starts, mut started) = mpsc::channel(16);
        let mut fixture = SessionFixture::open_with(
            Arc::new(AdmissionLoader { question, starts }),
            ApprovalMode::Ask,
            |options| options.max_active_runs = 2,
        )
        .await;
        let first = fixture.submit("held first").await;
        assert_eq!(
            within(started.recv()).await.unwrap().unwrap(),
            ("held first".into(), 0)
        );
        let (_, first_hold) = collect_until_approval_requested(&mut fixture.events).await;
        let second_session = create_session_with_mode(
            &fixture.runtime,
            fixture.workspace_id,
            None,
            ApprovalMode::Ask,
        )
        .await;
        let CommandOutcome::SessionCreated {
            session_id: second_session,
        } = second_session.outcome
        else {
            panic!("session")
        };
        submit_prompt_to(&fixture.runtime, second_session, "held second").await;
        assert_eq!(
            within(started.recv()).await.unwrap().unwrap(),
            ("held second".into(), 0)
        );
        collect_until_approval_requested(&mut fixture.events).await;
        let third_session = create_session(&fixture.runtime, fixture.workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: third_session,
        } = third_session.outcome
        else {
            panic!("session")
        };
        submit_prompt_to(&fixture.runtime, third_session, "third").await;
        assert_eq!(
            within(started.recv()).await.unwrap().unwrap(),
            ("third".into(), 0)
        );
        let occupied = within(Arc::clone(&fixture.runtime.inner.permits).acquire_owned())
            .await
            .unwrap()
            .unwrap();
        let fourth_session = create_session(&fixture.runtime, fixture.workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: fourth_session,
        } = fourth_session.outcome
        else {
            panic!("session")
        };
        submit_prompt_to(&fixture.runtime, fourth_session, "fourth").await;
        respond_approval(
            &fixture.runtime,
            first,
            first_hold.id,
            if question {
                ApprovalDecision::Answer {
                    answers: vec!["qq-core".into(), "capacity".into()],
                }
            } else {
                ApprovalDecision::ApproveOnce
            },
        )
        .await
        .unwrap();
        drop(occupied);
        fixture.runtime.request_schedule();
        assert_eq!(
            within(started.recv()).await.unwrap().unwrap(),
            ("held first".into(), 1)
        );
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        fixture.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn admission_approval_timeout_is_persisted_before_reacquiring_a_slot() {
    for question in [false, true] {
        let (starts, mut started) = mpsc::channel(8);
        let mut fixture = SessionFixture::open_with(
            Arc::new(AdmissionLoader { question, starts }),
            ApprovalMode::Ask,
            |options| {
                options.max_active_runs = 1;
                options.approval_timeout = Some(Duration::from_secs(60));
            },
        )
        .await;
        let waiting = probe::PROBE.observe(probe::Checkpoint::ApprovalWaiting(fixture.session_id));
        let held = fixture.submit("held timeout").await;
        within(started.recv()).await.unwrap().unwrap();
        let (_, hold) = collect_until_approval_requested(&mut fixture.events).await;
        let occupied = within(Arc::clone(&fixture.runtime.inner.permits).acquire_owned())
            .await
            .unwrap()
            .unwrap();
        within(waiting).await.unwrap().unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        let observed = within(collect_until(&mut fixture.events, |event| {
            matches!(event, SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::DeniedTimeout,
            } if tool_call.id == hold.id)
        }))
        .await;
        tokio::time::resume();
        observed.unwrap();
        assert!(matches!(
            respond_approval(
                &fixture.runtime,
                held,
                hold.id,
                if question {
                    ApprovalDecision::Answer {
                        answers: vec!["qq-core".into(), "late".into()],
                    }
                } else {
                    ApprovalDecision::ApproveOnce
                },
            )
            .await
            .unwrap()
            .outcome,
            CommandOutcome::ToolApprovalResolved {
                resolution: ApprovalResolution::DeniedTimeout,
                ..
            }
        ));
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(occupied);
        assert_eq!(
            within(started.recv()).await.unwrap().unwrap(),
            ("held timeout".into(), 1)
        );
        fixture.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn admission_reacquisition_can_be_cancelled_without_a_free_slot() {
    for question in [false, true] {
        let (starts, mut started) = mpsc::channel(8);
        let mut fixture = SessionFixture::open_with(
            Arc::new(AdmissionLoader { question, starts }),
            ApprovalMode::Ask,
            |options| options.max_active_runs = 1,
        )
        .await;
        let held = fixture.submit("held first").await;
        within(started.recv()).await.unwrap().unwrap();
        let (_, hold) = collect_until_approval_requested(&mut fixture.events).await;
        let created = create_session(&fixture.runtime, fixture.workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        submit_prompt_to(&fixture.runtime, session_id, "occupant").await;
        assert_eq!(
            within(started.recv()).await.unwrap().unwrap(),
            ("occupant".into(), 0)
        );
        respond_approval(
            &fixture.runtime,
            held,
            hold.id,
            if question {
                ApprovalDecision::Answer {
                    answers: vec!["qq-core".into(), "cancel".into()],
                }
            } else {
                ApprovalDecision::ApproveOnce
            },
        )
        .await
        .unwrap();
        fixture
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id: held },
            )
            .await
            .unwrap();
        let observed = collect_until_run_finished(&mut fixture.events, held).await;
        assert!(matches!(
            observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Cancelled,
                ..
            }
        ));
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        fixture.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn admission_deadline_expires_while_answer_waits_for_a_slot() {
    let (starts, mut started) = mpsc::channel(8);
    let mut fixture = SessionFixture::open_with(
        Arc::new(AdmissionLoader {
            question: false,
            starts,
        }),
        ApprovalMode::Ask,
        |options| options.max_active_runs = 1,
    )
    .await;
    let held = fixture
        .submit(Prompt::text("held deadline").limits(RunLimits {
            max_duration_ms: Some(10000),
            ..RunLimits::default()
        }))
        .await;
    within(started.recv()).await.unwrap().unwrap();
    let (_, hold) = collect_until_approval_requested(&mut fixture.events).await;
    let created = create_session(&fixture.runtime, fixture.workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    submit_prompt_to(&fixture.runtime, session_id, "occupant").await;
    within(started.recv()).await.unwrap().unwrap();
    respond_approval(
        &fixture.runtime,
        held,
        hold.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    let observed = collect_until_run_finished(&mut fixture.events, held).await;
    assert!(matches!(
        observed.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::BudgetExhausted { .. },
            ..
        }
    ));
    assert!(matches!(
        started.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    tokio::time::resume();
    fixture.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn admission_steering_cannot_bypass_reacquisition() {
    let (starts, mut started) = mpsc::channel(8);
    let mut fixture = SessionFixture::open_with(
        Arc::new(AdmissionLoader {
            question: false,
            starts,
        }),
        ApprovalMode::Ask,
        |options| options.max_active_runs = 1,
    )
    .await;
    let held = fixture.submit("held first").await;
    within(started.recv()).await.unwrap().unwrap();
    collect_until_approval_requested(&mut fixture.events).await;
    let occupied = Arc::clone(&fixture.runtime.inner.permits)
        .acquire_owned()
        .await
        .unwrap();
    fixture
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SteerRun {
                run_id: held,
                input: vec![InputPart::text("change approach")],
                interrupt: true,
            },
        )
        .await
        .unwrap();
    within(async {
        loop {
            let resuming = fixture
                .runtime
                .inner
                .admissions
                .lock()
                .unwrap()
                .get(&held)
                .unwrap()
                .is_resuming();
            if resuming {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        started.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(occupied);
    assert_eq!(
        within(started.recv()).await.unwrap().unwrap(),
        ("held first".into(), 1)
    );
    fixture.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn admission_interrupted_hold_can_be_cancelled_while_reacquisition_is_parked() {
    let (starts, mut started) = mpsc::channel(8);
    let mut fixture = SessionFixture::open_with(
        Arc::new(AdmissionLoader {
            question: false,
            starts,
        }),
        ApprovalMode::Ask,
        |options| options.max_active_runs = 1,
    )
    .await;
    let held = fixture.submit("held first").await;
    within(started.recv()).await.unwrap().unwrap();
    collect_until_approval_requested(&mut fixture.events).await;
    let occupied = Arc::clone(&fixture.runtime.inner.permits)
        .acquire_owned()
        .await
        .unwrap();
    let parked = probe::PROBE.observe(probe::Checkpoint::AdmissionWaiting(held));
    fixture
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SteerRun {
                run_id: held,
                input: vec![InputPart::text("change approach")],
                interrupt: true,
            },
        )
        .await
        .unwrap();
    within(parked).await.unwrap().unwrap();
    fixture
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: held },
        )
        .await
        .unwrap();
    let observed = within(collect_until_run_finished(&mut fixture.events, held))
        .await
        .unwrap();
    assert!(matches!(
        finished_outcome(&observed, held),
        Some(RunOutcome::Cancelled)
    ));
    assert!(matches!(
        started.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(occupied);
    fixture.runtime.shutdown().await.unwrap();
}

fn assert_no_human_permission_request(events: &[SessionEventEnvelope]) {
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "Auto must never ask a human"
    );
}

#[tokio::test]
async fn auto_denies_a_fixed_stop_and_continues_without_approval() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![
            (
                "__test_shell",
                r#"{"command":"cd sub && sudo make install"}"#.to_owned(),
            ),
            ("__test_mutate", "{}".to_owned()),
        ]],
    )
    .await;
    let run_id = submit_prompt(&harness, "try an allowed alternative").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert_no_human_permission_request(&observed);
    assert!(!observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallStarted { tool_call } if tool_call.name == "__test_shell"
    )));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.name == "__test_shell" && tool_call.is_error
                && tool_call.result.as_deref().is_some_and(|result| result.starts_with("permission_denied: fixed_stop:"))
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

#[tokio::test]
async fn auto_children_execute_authorized_edits_and_obey_parent_revocation() {
    for revoke_parent in [false, true] {
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
            r#"{"task":"edit within parent authority","model":"test/child","authority":"write"}"#.into())],
            ApprovalMode::Auto).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
        let child_id = within(async {
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
        if revoke_parent {
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
        }
        release.send(()).unwrap();
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        assert_no_human_permission_request(&observed);
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
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
            .expect("child edit settles");
        assert_eq!(completed.is_error, revoke_parent, "{:?}", completed.result);
        if !revoke_parent {
            assert_eq!(completed.result.as_deref(), Some("mutated"));
        }
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn auto_returns_questions_as_tool_errors_and_continues_without_a_human() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![
            ("ask_user", ASK_ARGUMENTS.into()),
            ("__test_mutate", "{}".into()),
        ]],
    )
    .await;
    let run_id = submit_prompt(&harness, "make a reasonable choice and continue").await;
    let observed = within(collect_until_run_finished(&mut harness.events, run_id))
        .await
        .expect("Auto questions must return a tool error instead of waiting for a human");
    assert_no_human_permission_request(&observed);
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "ask_user" && tool_call.is_error
            && tool_call.result.as_deref() == Some(approval::UNATTENDED_QUESTION_RESULT)
    )));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "__test_mutate" && !tool_call.is_error
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

struct ManagedShellDenyLoader {
    script: Vec<(&'static str, String)>,
}

impl RuntimeLoader for ManagedShellDenyLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: self.script.clone(),
            turn: StdMutex::new(0),
        };
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_shell_policy(crate::ShellPolicy {
                            deny_prefixes: Arc::from(["managed-tool --dangerous".to_owned()]),
                            ..Default::default()
                        }),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(load_error)
        })
    }
}

#[tokio::test]
async fn shell_prefix_denial_precedes_auto_and_full_and_allows_continuation() {
    const TOOL: &str = "shell";
    const ARGUMENTS: &str =
        r#"{"command":"unlisted-tool; managed-tool --dangerous && another-tool"}"#;
    for mode in [ApprovalMode::Auto, ApprovalMode::Full] {
        let directory = test_directory();
        let SessionFixture {
            directory: _directory,
            runtime,
            session_id,
            mut events,
            ..
        } = SessionFixture::open_at(
            directory,
            Arc::new(ManagedShellDenyLoader {
                script: vec![(TOOL, ARGUMENTS.into()), ("__test_mutate", "{}".into())],
            }),
            mode,
        )
        .await;
        let run_id =
            submit_prompt_to(&runtime, session_id, "continue with an allowed alternative").await;
        let observed = collect_until_run_finished(&mut events, run_id).await;
        assert_no_human_permission_request(&observed);
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(!observed.iter().any(|event| matches!(&event.event,
            SessionEvent::ToolCallStarted { tool_call } if tool_call.name == TOOL
        )));
        assert!(observed.iter().any(|event| matches!(&event.event,
            SessionEvent::ToolCallFinished { tool_call } if tool_call.name == TOOL && tool_call.is_error
                && tool_call.result.as_deref().is_some_and(|result| result == approval::SHELL_PREFIX_DENIED_RESULT)
        )));
        assert!(observed.iter().any(|event| matches!(&event.event,
            SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "__test_mutate" && !tool_call.is_error
        )));
        assert!(matches!(
            observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        runtime.shutdown().await.unwrap();
    }
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
            ApprovalMode::Auto).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
        let child_id = within(async {
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
        } else {
            assert!(completed.is_error);
            assert!(
                completed
                    .result
                    .as_deref()
                    .unwrap()
                    .starts_with("permission_denied: mode: this action is outside the authority")
            );
        }
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ask_requires_human_approval_for_each_call() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Ask,
        vec![vec![
            ("__test_mutate", "{}".to_owned()),
            ("__test_mutate", "{}".to_owned()),
        ]],
    )
    .await;
    let run_id = submit_prompt(&harness, "mutate twice").await;
    let (_, first) = collect_until_approval_requested(&mut harness.events).await;
    let receipt = respond_approval(
        &harness.runtime,
        run_id,
        first.id,
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
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            1,
            8,
        ))
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
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            1,
            8,
        ))
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
async fn shutdown_never_reports_success_after_the_runtime_has_failed() {
    let (_directory, runtime) = test_runtime().await;
    runtime.inner.failed.send_replace(true);

    assert_eq!(
        runtime.shutdown().await.unwrap_err(),
        SessionRuntimeError::Unavailable
    );
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
        ApprovalDecision::ApproveOnce,
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
async fn auto_settles_a_fixed_stop_before_any_hold() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![(
            "__test_shell",
            r#"{"command":"git push --force"}"#.to_owned(),
        )]],
    )
    .await;
    submit_prompt(&harness, "force push").await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "a fixed stop is settled before any hold"
    );
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Denied
                && tool_call.result.as_deref().is_some_and(|result| result.starts_with("permission_denied: fixed_stop:"))
    )));
}

#[tokio::test]
async fn an_interactive_hold_has_no_server_deadline_by_default() {
    let mut harness = approval_harness(ApprovalMode::Ask, "__test_mutate", "{}", 1, None).await;
    let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
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

#[cfg(unix)]
#[tokio::test]
async fn auto_shell_calls_stream_output_before_they_finish() {
    let mut harness = approval_harness(
        ApprovalMode::Auto,
        "shell",
        r#"{"command":"rm -f not-present; echo approved-output"}"#,
        2,
        DEFAULT_APPROVAL_TIMEOUT,
    )
    .await;
    let observed = collect_through_finished_generously(&mut harness.events).await;
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
        "Auto runs both shell calls without asking"
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
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call: finished }
                if finished.id == tool_call.id
                    && finished.state == ToolCallState::Failed
                    && finished.result.as_deref().is_some_and(|result| {
                        result.starts_with("shell exit=cancelled ") && result.contains("running")
                    })
        )),
        "{observed:?}"
    );
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
    let (buffered, release) = PROBE.hold(Checkpoint::BufferedToolOutput(tool_call.id));
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
                        && finished.state == ToolCallState::Failed
                        && finished.result.as_deref().is_some_and(|result| {
                            result.starts_with("shell exit=cancelled ")
                                && result.contains("buffered-output")
                        })
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
    let SessionFixture {
        directory: _directory,
        runtime,
        workspace_path,
        workspace_id,
        session_id,
        events,
        ..
    } = harness;
    runtime.shutdown().await.unwrap();
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

    let queued = try_submit(&reopened, session_id, "now edit it")
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

    let (workspace_id, _) = resolve_workspace(&harness.runtime, &harness.workspace_path).await;
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(harness.session_id),
            1,
            8,
        ))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let persisted = focused
        .tool_calls
        .iter()
        .find(|call| call.id == edited.id)
        .unwrap();
    assert_eq!(persisted.display, edited.display);

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
    let updated = harness.events.next().await.unwrap().unwrap();
    assert_eq!(updated.cursor, receipt.committed_through);
    assert!(matches!(
        &updated.event,
        SessionEvent::SessionUpdated { session }
            if session.id == harness.session_id
                && session.approval_mode == ApprovalMode::Auto
    ));

    try_submit(&harness.runtime, harness.session_id, "mutate again")
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
    let (workspace_id, session_id) =
        store_session(&store, directory.path(), ApprovalMode::Ask).await;
    store_command(&store, session_id, "mutate").await.unwrap();
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
            TurnCommit::new(
                1,
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::tool_call(
                        call.provider_call_id.clone(),
                        call.name.clone(),
                        &serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap(),
                    )],
                ),
            )
            .calls(vec![call])
            .build(),
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
    let mut events = subscribe_after(&runtime, workspace_id, awaiting.cursor);
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
        ApprovalMode::Ask,
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
async fn ask_fetch_has_a_preview_while_auto_fetch_needs_no_approval() {
    struct Loader {
        arguments: String,
        turns: usize,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = ScriptedRunProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                script: (0..self.turns)
                    .map(|_| ("fetch", self.arguments.clone()))
                    .collect(),
                turn: StdMutex::new(0),
            };
            Box::pin(async move {
                let runtime = Runtime::new(provider, "test-model", 256)
                    .unwrap()
                    .with_network_policy(crate::NetworkPolicy {
                        allow_private_for_tests: true,
                        ..Default::default()
                    });
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    for (mode, turns) in [(ApprovalMode::Ask, 1), (ApprovalMode::Auto, 2)] {
        let server = qq_provider::test_support::LoopbackServer::respond_sequence(
            (0..turns)
                .map(|_| {
                    (
                        200,
                        Some("text/plain"),
                        vec![b"fixture-public-docs".to_vec()],
                    )
                })
                .collect(),
        );
        let url = format!("{}/docs", server.base_url);
        let directory = test_directory();
        let SessionFixture {
            directory: _directory,
            runtime,
            session_id,
            mut events,
            ..
        } = SessionFixture::open_at(
            directory,
            Arc::new(Loader {
                arguments: serde_json::json!({"url":url}).to_string(),
                turns,
            }),
            mode,
        )
        .await;
        let run_id = submit_prompt_to(&runtime, session_id, "read docs").await;
        if mode == ApprovalMode::Ask {
            let (observed, call) = collect_until_approval_requested(&mut events).await;
            let preview = observed
                .iter()
                .find_map(|event| match &event.event {
                    SessionEvent::ToolApprovalRequested {
                        fetch, shell, edit, ..
                    } => {
                        assert!(shell.is_none() && edit.is_none());
                        fetch.as_ref()
                    }
                    _ => None,
                })
                .expect("Ask retains its fetch preview");
            assert_eq!(preview.url, url);
            assert_eq!(preview.host, "127.0.0.1");
            assert!(preview.method.is_none());
            let receipt =
                respond_approval(&runtime, run_id, call.id, ApprovalDecision::ApproveOnce)
                    .await
                    .unwrap();
            assert!(matches!(
                receipt.outcome,
                CommandOutcome::ToolApprovalResolved {
                    resolution: ApprovalResolution::ApprovedOnce,
                    ..
                }
            ));
        }
        let observed = collect_until_run_finished(&mut events, run_id).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "Auto needs no host approval and Ask has already received its fresh response"
        );
        let finished: Vec<_> = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolCallFinished { tool_call } => Some(tool_call),
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), turns);
        for call in finished {
            assert!(!call.is_error, "{:?}", call.result);
            assert!(
                call.result
                    .as_deref()
                    .is_some_and(|result| result.contains("fixture-public-docs"))
            );
        }
        let requests = tokio::task::spawn_blocking(move || server.capture_all())
            .await
            .unwrap();
        assert_eq!(requests.len(), turns);
        assert!(
            requests
                .iter()
                .all(|request| request.request_line() == Some("GET /docs HTTP/1.1"))
        );
        runtime.shutdown().await.unwrap();
    }
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
                    result.starts_with("permission_denied: config: fetch refused: host 169.254.169.254 resolves to a private")
                })
    )));
}

#[tokio::test]
async fn readonly_revocation_denies_a_late_human_approval() {
    for mode in [ApprovalMode::Ask] {
        for decision in [ApprovalDecision::ApproveOnce] {
            let mut harness =
                scripted_runs_harness(mode, vec![vec![("__test_mutate", "{}".to_owned())]]).await;
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
                    if tool_call.id == call.id && tool_call.result.as_deref().is_some_and(|text| text == approval::AUTHORITY_CHANGED_RESULT))));
            harness.runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
#[cfg(feature = "tool-fetch")]
async fn auto_returns_a_blocked_fetch_as_a_tool_error_and_completes_an_allowed_alternative() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![
            (
                "fetch",
                r#"{"url":"http://169.254.169.254/latest/meta-data"}"#.into(),
            ),
            ("__test_mutate", "{}".into()),
        ]],
    )
    .await;
    let run_id = submit_prompt(&harness, "use available information and continue").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert_no_human_permission_request(&observed);
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
    );
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "fetch"
            && tool_call.state == ToolCallState::Denied && tool_call.is_error
            && tool_call.result.as_deref().is_some_and(|result| result.contains("private"))
    )));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "__test_mutate"
            && !tool_call.is_error && tool_call.result.as_deref() == Some("mutated")
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
