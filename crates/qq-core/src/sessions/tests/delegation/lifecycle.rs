use super::*;
use crate::test_support::within;

#[tokio::test]
async fn cancelling_the_parent_run_cancels_its_in_flight_child() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![("spawn_agent", r#"{"task":"long research"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let hanging: Arc<dyn Provider> = Arc::new(HangingProvider);
    let mut harness = spawn_harness(Vec::new(), vec![parent, hanging], 8).await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate forever").await;

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
    cancel_run(&harness.runtime, parent_run).await.unwrap();
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
    let (child_run, child_call) = within(async {
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
    cancel_run(&harness.runtime, parent_run).await.unwrap();
    within(async {
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
    let (child_session_id, child_run) = within(async {
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
    within(second_turn_started.notified())
        .await
        .expect("the parent did not consume the child answer");
    cancel_run(&harness.runtime, parent_run).await.unwrap();
    within(async {
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
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(child_session_id),
            8,
            4,
        ))
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
    let (child_session_id, child_run) = within(async {
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

    within(harness.runtime.shutdown())
        .await
        .expect("shutdown must settle the parent and child")
        .unwrap();

    let mut terminal_count = 0;
    within(async {
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
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(child_session_id),
            8,
            4,
        ))
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
                Ok(ProviderLoader::new(StaticTextProvider).loaded(&request.workspace))
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
    within(entered.notified()).await.unwrap();
    let stopped = PROBE.observe(Checkpoint::ExecutionStop(run_id));
    let runtime = harness.runtime.clone();
    let mut shutdown = tokio::spawn(async move { runtime.shutdown().await });
    within(stopped).await.unwrap().unwrap();
    let premature = tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
        .await
        .is_ok();
    let completed_before_release = completed.load(Ordering::Acquire);
    release.send(()).unwrap();
    assert!(
        !premature,
        "shutdown cannot finish while admitted loader work is running"
    );
    within(shutdown).await.unwrap().unwrap().unwrap();
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
                Ok(ProviderLoader::new(StaticTextProvider).loaded(&request.workspace))
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
    let (workspace_id, session_id, _) =
        new_session(&runtime, directory.path(), ApprovalMode::default()).await;
    let parent_run = RunId::generate().unwrap();
    let parent = ClaimedRun {
        origin: None,
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
        message_effects: Vec::new(),
        history_admission_failure: None,
        context_compaction_attempted: 0,
        context_compaction_failed: false,
        context_compaction_remaining: false,
        compaction_cutoff_ordinal: None,
        context_compaction_oversized_unit_bytes: None,
        in_run_turn_cutoff: None,
        context_overflow_basis: None,
        context_occupancy: None,
        limits: RunLimits::default(),
        input: Vec::new(),
        resolved_input: None,
        profile: AgentProfileId::default(),
        reasoning_effort: None,
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
            advertised_models: Arc::from([]),
            slots: Arc::new(Semaphore::new(1)),
            write_slot: Arc::new(Semaphore::new(1)),
            write_children: false,
            spawned: Arc::new(AtomicUsize::new(0)),
            max_children: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
            tasks: Arc::new(subagents::ChildTasks::default()),
        },
        SpawnRequest {
            call_id: ToolCallId::from_bytes([0x5a; 16]),
            reasoning_effort: None,
            task: "research".to_owned(),
            model: None,
            authority: qq_protocol::ChildAuthority::Read,
            budget: crate::runtime::ChildBudget::default(),
            purpose: SessionPurpose::Task,
            detached: false,
        },
    ));
    within(entered.notified())
        .await
        .expect("child admission must reach its pre-commit load");

    within(runtime.shutdown())
        .await
        .expect("shutdown must complete while pre-commit child work is paused")
        .unwrap();
    release.notify_one();
    let outcome = within(child)
        .await
        .expect("released child admission must observe shutdown")
        .unwrap();
    assert!(outcome.is_error);
    assert!(outcome.content.contains("shutting down"));

    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 1))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].id, session_id);
    assert_eq!(snapshot.sessions[0].active_run_id, None);
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
    let (delivered, release) = PROBE.hold(Checkpoint::ChildDelivery(harness.session_id));
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
    within(delivered).await.unwrap().unwrap();
    steer(
        &harness.runtime,
        run_id,
        "continue with your own answer",
        true,
    )
    .await
    .unwrap();
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

#[tokio::test(start_paused = true)]
#[should_panic(expected = "frozen-clock operation did not complete")]
async fn frozen_collection_watchdog_expires_without_virtual_time_advancing() {
    await_frozen(std::future::pending::<()>(), Duration::ZERO).await;
}

#[tokio::test]
async fn child_mutation_drains_before_steering_or_a_replacement_run_can_write() {
    for interruption in [0, 1, 2, 3, 4] {
        let shutting_down = interruption == 3;
        let deadline = interruption == 4;
        let cancel_parent = interruption != 0;
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
            ApprovalMode::Full,
        )
        .await;
        let workspace = std::fs::canonicalize(harness.directory.path()).unwrap();

        let keep_clock = if deadline {
            tokio::time::pause();
            Some(tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            }))
        } else {
            None
        };
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
        await_frozen(applying, Duration::from_secs(2))
            .await
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
        let mut stopping = Some(PROBE.observe(Checkpoint::ExecutionStop(child_run)));
        if interruption == 2 {
            steer(&harness.runtime, parent_run, "stop the child", true)
                .await
                .unwrap();
            within(stopping.take().unwrap()).await.unwrap().unwrap();
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
            cancel_run(&harness.runtime, parent_run).await.unwrap();
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
            tokio::time::advance(Duration::from_millis(1_200)).await;
        } else if interruption != 2 {
            within(stopping.take().unwrap()).await.unwrap().unwrap();
        }
        if let Some(keep_clock) = keep_clock {
            tokio::time::resume();
            keep_clock.abort();
        }

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
            within(shutdown).await.unwrap().unwrap().unwrap();
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
            std::fs::read_to_string(harness.directory.path().join("child.txt")).unwrap(),
            "child"
        );
        if shutting_down {
            assert!(!harness.directory.path().join("parent.txt").exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(harness.directory.path().join("parent.txt")).unwrap(),
                "parent"
            );
        }
    }
}

#[tokio::test]
async fn interrupting_steering_owns_a_child_whose_creation_reply_is_pending() {
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
        ApprovalMode::Full,
    )
    .await;
    let (created, release) =
        PROBE.hold_reporting_run(Checkpoint::ChildCreated(harness.session_id), 0);
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let child_run = within(created).await.unwrap().unwrap();
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
        std::fs::read_to_string(harness.directory.path().join("parent.txt")).unwrap(),
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
        ApprovalMode::Full,
    )
    .await;
    let parent_run =
        submit_prompt_to(&harness.runtime, harness.session_id, "delegate then write").await;
    within(entered.notified()).await.unwrap();
    let mut observed = Vec::new();
    let (child_run, cursor) = loop {
        let event = within(harness.events.next())
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
    let (read_entered, release_read, read_attempted) = hold_outcome_read(child_run);
    harness.runtime.inner.notify(cursor);
    within(read_entered).await.unwrap().unwrap();

    let blocked_store = harness.runtime.store().clone();
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
        let store = harness.runtime.store().clone();
        let mut job = Box::pin(async move { store.call(Priority::Control, |_| Ok(())).await });
        assert!(futures_util::poll!(job.as_mut()).is_pending());
        queued.push(job);
    }
    release_read.send(()).unwrap();
    within(read_attempted).await.unwrap().unwrap();
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
        std::fs::read_to_string(harness.directory.path().join("parent.txt")).unwrap(),
        "parent resumed"
    );
}

#[tokio::test]
async fn child_cancellation_persistence_failure_prevents_parent_continuation() {
    let (mut harness, requests) = write_child_harness(
        Arc::new(HangingProvider),
        vec![(
            "spawn_agent",
            r#"{"task":"wait","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::Full,
    )
    .await;
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let child_run = within(async {
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
    FAULTS.inject(
        StoreFault::ChildCancellation(child_run),
        [SessionRuntimeError::CONSTRAINT],
    );
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
    let unavailable = within(async {
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
        ApprovalMode::Full,
    )
    .await;
    let workspace = std::fs::canonicalize(harness.directory.path()).unwrap();
    let spawned = crate::tools::observe_shell_spawn(&workspace, true);
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "run a command").await;
    let pid = within(spawned).await.unwrap().unwrap();
    let unavailable = within(async {
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
    let (mut harness, parent_requests) = write_child_harness(
        Arc::new(HangingProvider),
        vec![(
            "spawn_agent",
            r#"{"task":"hold the checkout","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::Full,
    )
    .await;
    let parent_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let (child_run, cursor) = loop {
        let event = within(harness.events.next())
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
    let (read_entered, release_read, _read_attempted) = hold_outcome_read(child_run);
    harness.runtime.inner.notify(cursor);
    within(read_entered).await.unwrap().unwrap();
    harness
        .runtime
        .store()
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
    let unavailable = within(async {
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
        cancel_run(&harness.runtime, parent_run).await,
        Err(SessionRuntimeError::Unavailable)
    );
}
