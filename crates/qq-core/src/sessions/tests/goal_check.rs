use super::*;

async fn explicit_check_session(
    runtime: &SessionRuntime,
    workspace_id: WorkspaceId,
    parent_id: Option<SessionId>,
) -> CommandReceipt {
    create_session_with_mode(runtime, workspace_id, parent_id, ApprovalMode::Full).await
}

struct Loader {
    failure_tail: bool,
    loads: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}
struct Model {
    failure_tail: bool,
    calls: Arc<AtomicUsize>,
    turns: AtomicUsize,
}
impl Provider for Model {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        assert!(
            !request_texts(&request)
                .join("\n")
                .contains("hunter2hunter2"),
            "raw check credentials must never enter provider context"
        );
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failure_tail && call >= 2 {
            assert!(
                request_texts(&request)
                    .iter()
                    .any(|text| text.contains("exit 7")),
                "failed check exit status must reach the next goal run"
            );
            assert_eq!(
                request_texts(&request)
                    .join("\n")
                    .matches("check-output-unique")
                    .count(),
                1,
                "check-only result messages must not leak into model context alongside the goal notice"
            );
        }
        let mut events = if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "claim".to_owned(), name: "update_goal".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "claim".to_owned(),
                    json: r#"{"status":{"type":"achieved","evidence":"work is ready for independent verification"}}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted { id: "claim".to_owned() }),
            ]
        } else {
            vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "check my work".to_owned(),
            })]
        };
        events.push(Ok(qq_provider::ProviderEvent::Completed {
            usage: Some(qq_provider::ProviderUsage {
                input_tokens: 3,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: None,
            }),
        }));
        Box::pin(stream::iter(events))
    }
}
impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.loads.fetch_add(1, Ordering::SeqCst);
        let calls = self.calls.clone();
        let failure_tail = self.failure_tail;
        Box::pin(async move {
            Ok(loaded_runtime(
                Runtime::new(
                    Model {
                        failure_tail,
                        calls,
                        turns: AtomicUsize::new(0),
                    },
                    "test-model",
                    256,
                )
                .unwrap(),
                &request.workspace,
                None,
            ))
        })
    }
}
struct Authority(&'static str);
impl WorkspacePolicyAuthority for Authority {
    fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
        Box::pin(std::future::ready(Ok(GoalPolicy::default())))
    }

    fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
        assert_eq!(command, self.0);
        Box::pin(std::future::ready(Ok(())))
    }
}

#[tokio::test]
async fn goal_check_once_executes_without_a_model_and_achieves_durably() {
    once_check(true).await;
}

#[tokio::test]
async fn goal_check_consumed_once_failure_requires_new_authority_without_requeue_loop() {
    once_check(false).await;
}

async fn once_check(success: bool) {
    let command = match (success, cfg!(windows)) {
        (true, false) => "printf checked >> goal-check-marker",
        (false, false) => {
            "printf checked >> goal-check-marker; printf '%s%s' check-output- unique; exit 7"
        }
        (true, true) => "(echo|set /p=checked)>>goal-check-marker",
        (false, true) => {
            "(echo|set /p=checked)>>goal-check-marker& set a=check-output-& call echo %a%unique& exit 7"
        }
    };
    let directory = TempDir::new().unwrap();
    let database = directory
        .path()
        .canonicalize()
        .unwrap()
        .join("sessions.sqlite3");
    let loads = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut options = SessionRuntimeOptions::new(database.clone());
    options.grant_authority = Some(Arc::new(Authority(command)));
    let runtime = SessionRuntime::open(
        options,
        Arc::new(Loader {
            failure_tail: !success,
            loads: loads.clone(),
            calls: calls.clone(),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = explicit_check_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"verify checked completion",
                "check":{"command":command,"timeout_ms":5000},
                "budget":{"max_goal_runs":if success { 1 } else { 10 },"deadline_ms":now_ms()+60_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing goal")
    };
    assert!(!directory.path().join("goal-check-marker").exists());
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::Once,
                    command: command.to_owned(),
                },
            },
        )
        .await
        .unwrap();
    let work = collect_through_finished_generously(&mut events).await;
    assert!(
        work.iter().any(|event| matches!(&event.event,
            SessionEvent::GoalUpdated { goal }
                if goal.audit_pending && goal.achievement_claim.is_some()
                    && goal.status == qq_protocol::GoalStatus::Active
        )),
        "a model achievement proposal must remain active pending independent verification"
    );
    let achieved = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::GoalUpdated { goal } = event.event
                && ((success && goal.status == qq_protocol::GoalStatus::Achieved)
                    || (!success
                        && matches!(
                            goal.status,
                            qq_protocol::GoalStatus::Paused {
                                reason: qq_protocol::GoalPauseReason::NeedsUser {
                                    cause: qq_protocol::GoalNeedsUser::CheckApproval
                                }
                            }
                        )))
            {
                break goal;
            }
        }
    })
    .await
    .expect("authorized exit-zero check must execute and achieve the goal");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("goal-check-marker")).unwrap(),
        "checked"
    );
    assert_eq!(
        achieved.check_authorization,
        Some(qq_protocol::GoalCheckAuthorization::Consumed)
    );
    assert_eq!(achieved.counters.audits, 1);
    if success {
        use crate::sessions::goals::{goal_driver_exhaustion, goal_exhaustion};
        use qq_protocol::GoalExhaustion;
        let mut pending = achieved.clone();
        pending.audit_pending = true;
        pending.audit_revision = Some(pending.revision);
        let now = now_ms();
        assert_eq!(goal_driver_exhaustion(&pending, now), None);
        assert_eq!(goal_exhaustion(&pending, now), Some(GoalExhaustion::Runs));
        for reason in [
            GoalExhaustion::Deadline,
            GoalExhaustion::Tokens,
            GoalExhaustion::TokensUnknown,
            GoalExhaustion::Cost,
            GoalExhaustion::CostUnknown,
        ] {
            let mut bounded = pending.clone();
            match reason {
                GoalExhaustion::Deadline => bounded.budget.deadline_ms = now,
                GoalExhaustion::Tokens => bounded.budget.max_total_tokens = Some(1),
                GoalExhaustion::TokensUnknown => {
                    bounded.budget.max_total_tokens = Some(1);
                    bounded.counters.usage = None;
                }
                GoalExhaustion::Cost => {
                    bounded.budget.max_cost_usd_nanos = Some(1);
                    bounded.counters.estimated_cost_usd_nanos = Some(1);
                }
                GoalExhaustion::CostUnknown => {
                    bounded.budget.max_cost_usd_nanos = Some(1);
                    bounded.counters.estimated_cost_usd_nanos = None;
                }
                _ => unreachable!(),
            }
            assert_eq!(goal_driver_exhaustion(&bounded, now), Some(reason));
        }
        for invalid in 0..3 {
            let mut invalidated = pending.clone();
            match invalid {
                0 => invalidated.audit_pending = false,
                1 => invalidated.audit_revision = None,
                _ => invalidated.check = None,
            }
            assert_eq!(
                goal_driver_exhaustion(&invalidated, now),
                Some(GoalExhaustion::Runs)
            );
        }
    }

    assert_eq!(
        achieved.counters.runs,
        if success { 1 } else { 2 },
        "checks are not model runs"
    );
    assert_eq!(
        achieved.counters.check_failures,
        if success { 0 } else { 1 }
    );
    assert_eq!(
        loads.load(Ordering::SeqCst),
        if success { 1 } else { 2 },
        "a check must not load a model"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        if success { 3 } else { 4 },
        "only the work run and its final-slot wrap-up may call the model"
    );
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(snapshot.runs.len(), if success { 2 } else { 4 });
    assert_eq!(
        snapshot
            .runs
            .iter()
            .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .count(),
        if success { 1 } else { 2 }
    );
    assert!(snapshot.runs.iter().all(|run| if success {
        run.outcome == Some(RunOutcome::Completed)
    } else {
        run.outcome.is_some()
    }));
    runtime.shutdown().await.unwrap();
    drop(events);
    drop(runtime);
    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(database),
        Arc::new(Loader {
            failure_tail: !success,
            loads: loads.clone(),
            calls: calls.clone(),
        }),
    )
    .await
    .unwrap();
    let restored = reopened
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(restored.goal, Some(achieved));
    assert_eq!(restored.runs, snapshot.runs);
    assert_eq!(loads.load(Ordering::SeqCst), if success { 1 } else { 2 });
    reopened.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_process_keeps_exit_status_and_only_a_four_kib_tail() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let result = run_process(
        directory.path().to_owned(),
        "i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done; printf '\\n'; i=0; while [ \"$i\" -lt 4093 ]; do printf x; i=$((i+1)); done; printf end; exit 7"
            .to_owned(),
        tokio::time::Instant::now() + Duration::from_secs(5),
        RunCancellation::new(),
        Arc::new(crate::ProcessProducer::new("test")),
        execution::RunResources::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(7) });
    assert_eq!(result.tail.len(), 4096);
    assert!(result.tail.ends_with("end"));
}

#[tokio::test]
async fn goal_check_process_expired_or_cancelled_before_start_never_spawns() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    for cancel in [false, true] {
        let directory = TempDir::new().unwrap();
        let cancellation = RunCancellation::new();
        if cancel {
            cancellation.cancel();
        }
        let result = run_process(
            directory.path().to_owned(),
            "printf unexpected > marker".to_owned(),
            if cancel {
                tokio::time::Instant::now() + Duration::from_secs(5)
            } else {
                tokio::time::Instant::now()
            },
            cancellation,
            Arc::new(crate::ProcessProducer::new("test")),
            execution::RunResources::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.outcome,
            if cancel {
                ProcessOutcome::Cancelled
            } else {
                ProcessOutcome::TimedOut
            }
        );
        assert!(!directory.path().join("marker").exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_process_cancellation_drains_its_process_group() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let cancellation = RunCancellation::new();
    let task = tokio::spawn(run_process(
        directory.path().to_owned(),
        "sleep 60 & printf '%s' \"$!\" > descendant; wait".to_owned(),
        tokio::time::Instant::now() + Duration::from_secs(5),
        cancellation.clone(),
        Arc::new(crate::ProcessProducer::new("test")),
        execution::RunResources::default(),
    ));
    let descendant = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(pid) = tokio::fs::read_to_string(directory.path().join("descendant")).await
                && let Ok(pid) = pid.parse::<u32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    #[cfg(target_os = "linux")]
    let descendant_exit = super::goal_driver_soak::observe_pid(descendant);
    cancellation.cancel();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Cancelled);
    #[cfg(target_os = "linux")]
    {
        let mut descriptors = [rustix::event::PollFd::new(
            descendant_exit.0.get_ref(),
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(&mut descriptors, Some(&rustix::event::Timespec::default())).unwrap();
        assert!(
            descriptors[0]
                .revents()
                .intersects(rustix::event::PollFlags::IN | rustix::event::PollFlags::HUP),
            "descendant exit must be confirmed before cancellation completes"
        );
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = tokio::fs::read_to_string(format!("/proc/{descendant}/stat")).await {
            assert_eq!(stat.rsplit_once(") ").unwrap().1.as_bytes()[0], b'Z');
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = descendant;
}

#[cfg(all(unix, not(target_os = "linux")))]
#[tokio::test]
async fn goal_check_process_kills_background_descendants_before_success() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let result = run_process(
        directory.path().to_owned(),
        "sleep 60 >/dev/null 2>&1 & printf '%s' \"$!\" > descendant; exit 0".to_owned(),
        tokio::time::Instant::now() + Duration::from_secs(5),
        RunCancellation::new(),
        Arc::new(crate::ProcessProducer::new("test")),
        execution::RunResources::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    let pid = tokio::fs::read_to_string(directory.path().join("descendant"))
        .await
        .unwrap();
    #[cfg(target_os = "linux")]
    if let Ok(stat) = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await {
        assert_eq!(stat.rsplit_once(") ").unwrap().1.as_bytes()[0], b'Z');
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn goal_check_process_kills_background_descendants_before_success() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    use super::goal_driver_soak::{check_ready, observe_pid, signal_pipe};
    let directory = TempDir::new().unwrap();
    let ready = signal_pipe(directory.path(), "descendant-ready");
    let leader_release = signal_pipe(directory.path(), "leader-release");
    let descendant_release = signal_pipe(directory.path(), "descendant-release");
    let workspace = directory.path().to_owned();
    let task = tokio::spawn(run_process(
        workspace,
        "(read -r signal < descendant-release; printf forbidden > descendant-wrote) >/dev/null 2>&1 & printf '%s' \"$!\" > descendant-pid; printf r > descendant-ready; read -r signal < leader-release; exit 0".to_owned(),
        tokio::time::Instant::now()+Duration::from_secs(5),
        RunCancellation::new(),Arc::new(crate::ProcessProducer::new("test")),execution::RunResources::default(),
    ));
    tokio::time::timeout(Duration::from_secs(5), check_ready(&ready))
        .await
        .unwrap();
    let pid: u32 = tokio::fs::read_to_string(directory.path().join("descendant-pid"))
        .await
        .unwrap()
        .parse()
        .unwrap();
    let descendant = observe_pid(pid);
    assert_eq!(
        rustix::io::write(leader_release.get_ref(), b"release\n").unwrap(),
        8
    );
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    assert_eq!(
        rustix::io::write(descendant_release.get_ref(), b"release\n").unwrap(),
        8
    );
    let mut exited = tokio::time::timeout(Duration::from_secs(5), descendant.0.readable())
        .await
        .unwrap()
        .unwrap();
    exited.retain_ready();
    assert!(
        !tokio::fs::try_exists(directory.path().join("descendant-wrote"))
            .await
            .unwrap(),
        "descendant executed after supervisor completion"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn goal_check_noisy_descendants_cannot_starve_leader_exit() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    use super::goal_driver_soak::{check_ready, observe_pid, signal_pipe};
    for redirect in ["", " >&2"] {
        let directory = TempDir::new().unwrap();
        let ready = signal_pipe(directory.path(), "ready");
        let release = signal_pipe(directory.path(), "release");
        let command = format!(
            "i=0; while [ \"$i\" -lt 32 ]; do yes flood{redirect} & i=$((i+1)); done; printf '%s' \"$!\" > descendant; printf r > ready; read -r signal < release; exit 0"
        );
        let task = tokio::spawn(run_process(
            directory.path().to_owned(),
            command,
            tokio::time::Instant::now() + Duration::from_secs(5),
            RunCancellation::new(),
            Arc::new(crate::ProcessProducer::new("test")),
            execution::RunResources::default(),
        ));
        tokio::time::timeout(Duration::from_secs(5), check_ready(&ready))
            .await
            .unwrap();
        let pid: u32 = tokio::fs::read_to_string(directory.path().join("descendant"))
            .await
            .unwrap()
            .parse()
            .unwrap();
        let exited = observe_pid(pid);
        rustix::io::write(release.get_ref(), b"release\n").unwrap();
        let result = task.await.unwrap().unwrap();
        assert_eq!(
            result.outcome,
            ProcessOutcome::Exited { code: Some(0) },
            "redirect={redirect}"
        );
        assert!(result.tail.len() <= 4096);
        let mut readiness = exited.0.readable().await.unwrap();
        readiness.retain_ready();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_process_masks_credentials_before_bounding_diagnostics() {
    use super::super::goal_check::run_process;
    let directory = TempDir::new().unwrap();
    for (command, expected) in [
        (
            "printf 'DB_PASSWORD=hunter2hunter2\nsafe-tail\n'",
            "DB_PASSWORD=[masked:credential]\nsafe-tail\n",
        ),
        (
            "printf 'DB_PASSWORD='; i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done; printf '\nsafe-tail\n'",
            "safe-tail\n",
        ),
    ] {
        let result = run_process(
            directory.path().to_owned(),
            command.to_owned(),
            tokio::time::Instant::now() + Duration::from_secs(5),
            RunCancellation::new(),
            Arc::new(crate::ProcessProducer::new("test")),
            execution::RunResources::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.tail, expected);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_process_timeout_and_binary_output_stay_bounded() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let result = run_process(
        directory.path().to_owned(),
        "i=0; while [ \"$i\" -lt 5000 ]; do printf '\\377'; i=$((i+1)); done; printf '\\n'; i=0; while [ \"$i\" -lt 1800 ]; do printf '\\377'; i=$((i+1)); done; printf end; sleep 60"
            .to_owned(),
        tokio::time::Instant::now() + Duration::from_secs(2),
        RunCancellation::new(),
        Arc::new(crate::ProcessProducer::new("test")),
        execution::RunResources::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, ProcessOutcome::TimedOut);
    assert!(result.tail.len() <= 4096);
    assert!(result.tail.ends_with("end"));
    assert!(result.tail.contains('\u{fffd}'));
}

#[cfg(unix)]
#[tokio::test]
async fn goal_checks_prepared_together_in_one_workspace_do_not_deadlock() {
    struct CoordinatedAuthority {
        checks: AtomicUsize,
        barrier: Arc<tokio::sync::Barrier>,
    }
    impl WorkspacePolicyAuthority for CoordinatedAuthority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy::default())))
        }

        fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert_eq!(command, "printf checked >> goal-check-marker");
            let call = self.checks.fetch_add(1, Ordering::SeqCst);
            let barrier = self.barrier.clone();
            Box::pin(async move {
                if (2..4).contains(&call) {
                    barrier.wait().await;
                }
                Ok(())
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.grant_authority = Some(Arc::new(CoordinatedAuthority {
        checks: AtomicUsize::new(0),
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    }));
    let loads = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        options,
        Arc::new(Loader {
            failure_tail: false,
            loads: loads.clone(),
            calls: calls.clone(),
        }),
    )
    .await
    .unwrap();
    let held = runtime
        .inner
        .permits
        .clone()
        .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    let (workspace_id, resolved) = resolve_workspace(&runtime, directory.path()).await;
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: resolved,
        })
        .unwrap();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let created = explicit_check_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("missing session")
        };
        sessions.push(session_id);
        let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"independent checked work",
            "check":{"command":"printf checked >> goal-check-marker","timeout_ms":5000},
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("missing goal")
        };
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::ApproveCheck {
                        scope: qq_protocol::GoalCheckScope::Once,
                        command: "printf checked >> goal-check-marker".to_owned(),
                    },
                },
            )
            .await
            .unwrap();
    }
    drop(held);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut achieved = std::collections::HashSet::new();
        while achieved.len() < 2 {
            let event = events.next().await.unwrap().unwrap();
            if let SessionEvent::GoalUpdated { goal } = event.event
                && goal.status == qq_protocol::GoalStatus::Achieved
            {
                achieved.insert(goal.goal_id);
            }
        }
    })
    .await
    .expect("both prepared checks must acquire workspace exclusion in turn");
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("goal-check-marker"))
            .await
            .unwrap(),
        "checkedchecked"
    );
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    for session in sessions {
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session), 256, 256))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(
            snapshot.goal.unwrap().status,
            qq_protocol::GoalStatus::Achieved
        );
        assert_eq!(snapshot.runs.len(), 2);
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn goal_check_calendar_expiry_during_policy_wait_never_refreshes_the_audit_timeout() {
    struct HeldAuthority {
        calls: AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl WorkspacePolicyAuthority for HeldAuthority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy::default())))
        }

        fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert_eq!(command, "printf checked >> goal-check-marker");
            let hold = self.calls.fetch_add(1, Ordering::SeqCst) == 1;
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                if hold {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(())
            })
        }
    }
    struct FirstWorkOnly(Loader);
    impl RuntimeLoader for FirstWorkOnly {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            if self.0.loads.load(Ordering::SeqCst) > 0 {
                return Box::pin(std::future::pending());
            }
            self.0.load(request)
        }
    }
    for goal_deadline in [false, true] {
        let start = 1_800_000_000_000;
        let epoch = super::goal_driver_soak::Epoch::new(start);
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.grant_authority = Some(Arc::new(HeldAuthority {
            calls: AtomicUsize::new(0),
            entered: entered.clone(),
            release: release.clone(),
        }));
        let runtime = SessionRuntime::open(
            options,
            Arc::new(FirstWorkOnly(Loader {
                failure_tail: false,
                loads: Arc::new(AtomicUsize::new(0)),
                calls: Arc::new(AtomicUsize::new(0)),
            })),
        )
        .await
        .unwrap();
        let (workspace_id, cursor) = resolve_workspace(&runtime, directory.path()).await;
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: cursor,
            })
            .unwrap();
        let created = explicit_check_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("missing session")
        };
        let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"verify one absolute deadline",
            "check":{"command":"printf checked >> goal-check-marker","timeout_ms":60_000},
            "budget":{"max_goal_runs":10,"deadline_ms":start+600_000}
        })).unwrap()).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("missing goal")
        };
        let approved_revision = goal.revision + 1;
        let monotonic_start = tokio::time::Instant::now();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::ApproveCheck {
                        scope: qq_protocol::GoalCheckScope::Once,
                        command: "printf checked >> goal-check-marker".to_owned(),
                    },
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("check must be reserved before the calendar changes");
        epoch.advance(start + if goal_deadline { 600_001 } else { 60_001 });
        release.notify_one();
        let observed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::GoalUpdated { goal } = event.event
                    && goal.revision == approved_revision
                    && (goal.counters.check_failures > 0
                        || goal.status != qq_protocol::GoalStatus::Active)
                {
                    break goal;
                }
            }
        })
        .await
        .expect("expired acquisition must settle rather than refresh the check");
        assert!(
            monotonic_start.elapsed() < Duration::from_secs(60),
            "the calendar-only case must not consume its monotonic check timeout"
        );
        assert_eq!(
            observed.check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::Once),
            "unstarted authority must remain unconsumed"
        );
        assert_eq!(observed.counters.audits, 0);
        assert!(
            tokio::fs::metadata(directory.path().join("goal-check-marker"))
                .await
                .is_err()
        );
        if goal_deadline {
            assert_eq!(
                observed.status,
                qq_protocol::GoalStatus::BudgetExhausted {
                    reason: qq_protocol::GoalExhaustion::Deadline
                }
            );
        } else {
            assert_eq!(observed.status, qq_protocol::GoalStatus::Active);
            assert_eq!(observed.counters.check_failures, 1);
            assert!(!observed.audit_pending);
            assert!(
                observed
                    .check_output
                    .as_ref()
                    .unwrap()
                    .contains("timed out")
            );
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::GoalControl {
                        session_id,
                        expected: qq_protocol::GoalIdentity {
                            goal_id: observed.goal_id,
                            revision: observed.revision,
                        },
                        action: qq_protocol::GoalControl::Pause,
                    },
                )
                .await
                .unwrap();
        }
        runtime.shutdown().await.unwrap();
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(
            snapshot
                .runs
                .iter()
                .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
                .count(),
            1
        );
        assert_eq!(snapshot.summary.queued_prompts, 0);
        assert!(snapshot.summary.active_run_id.is_none());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_historical_result_survives_edit_replace_and_clear() {
    const COMMAND: &str =
        "printf '%05000d\n' 0; printf '%04072d' 0; printf historical-check-result; exit 7";
    for mutation in ["edit", "replace", "clear", "pause"] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let loader = Arc::new(Loader {
            failure_tail: false,
            loads: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let mut options = SessionRuntimeOptions::new(database.clone());
        options.grant_authority = Some(Arc::new(Authority(COMMAND)));
        let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            explicit_check_session(&runtime, workspace_id, None)
                .await
                .outcome
        else {
            panic!("missing session")
        };
        let (entered, release) = super::super::goal_check::hold_result(session_id);
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal","session_id":session_id,"objective":"old objective",
                    "check":{"command":COMMAND,"timeout_ms":5000},
                    "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("missing goal")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .unwrap();
        let approved = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::ApproveCheck {
                        scope: qq_protocol::GoalCheckScope::Once,
                        command: COMMAND.to_owned(),
                    },
                },
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = approved.outcome else {
            panic!("missing approved goal")
        };
        tokio::time::timeout(Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        let before = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        let check = before
            .runs
            .iter()
            .find(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .unwrap()
            .id;
        let expected = serde_json::json!({"goal_id":goal.goal_id,"revision":goal.revision});
        let command = if matches!(mutation, "clear" | "pause") {
            serde_json::json!({"type":"goal_control","session_id":session_id,"expected":expected,"action":{"type":mutation}})
        } else {
            serde_json::json!({"type":"set_goal","session_id":session_id,"objective":"new objective","expected":expected,"replace":mutation=="replace",
                "check":{"command":"printf replacement","timeout_ms":5000},
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}})
        };
        runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(command).unwrap(),
            )
            .await
            .unwrap();
        let mut changed = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap()
            .goal;
        if mutation == "pause" {
            let paused = changed.as_ref().unwrap();
            assert_eq!(
                paused.pending_run_id,
                Some(check),
                "pause retains draining ownership"
            );
            assert!(paused.audit_pending && paused.achievement_claim.is_some());
            assert_eq!(paused.audit_revision, Some(paused.revision));
            assert_eq!(
                paused.check_authorization,
                Some(qq_protocol::GoalCheckAuthorization::Consumed)
            );
            assert!(matches!(
                runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::GoalControl {
                            session_id,
                            expected: qq_protocol::GoalIdentity {
                                goal_id: paused.goal_id,
                                revision: paused.revision
                            },
                            action: qq_protocol::GoalControl::Resume,
                        }
                    )
                    .await,
                Err(SessionRuntimeError::GoalCancellationPending)
            ));
        }
        if let Some(goal) = &mut changed {
            assert!(goal.pending_run_id.is_none() || goal.pending_run_id == Some(check));
            goal.pending_run_id = None;
        }
        release.send(()).unwrap();
        let result_events = tokio::time::timeout(Duration::from_secs(10), async {
            let mut seen = Vec::new();
            loop {
                let event = events.next().await.unwrap().unwrap();
                let done =
                    matches!(event.event,SessionEvent::RunFinished { run_id, .. } if run_id==check);
                seen.push(event);
                if done {
                    break seen;
                }
            }
        })
        .await
        .unwrap();
        if mutation == "pause" {
            let paused = changed.as_ref().unwrap();
            assert!(
                matches!(
                    runtime
                        .command(
                            CommandId::generate().unwrap(),
                            SessionCommand::GoalControl {
                                session_id,
                                expected: qq_protocol::GoalIdentity {
                                    goal_id: paused.goal_id,
                                    revision: paused.revision
                                },
                                action: qq_protocol::GoalControl::Resume,
                            }
                        )
                        .await,
                    Err(SessionRuntimeError::GoalCheckApprovalRequired)
                ),
                "draining a check cannot refund consumed Once consent"
            );
            assert_eq!(paused.counters.audits, 1);
            assert_eq!(paused.counters.check_failures, 0);
        }
        runtime.shutdown().await.unwrap();
        let finished = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(
            finished.goal, changed,
            "{mutation}: stale completion changed current goal"
        );
        let message = finished
            .messages
            .iter()
            .find(|message| message.run_id == check)
            .unwrap_or_else(|| panic!("{mutation}: historical check result missing"));
        assert_eq!(
            message.output,
            format!(
                "exit 7\n{}historical-check-result",
                "0".repeat(4096 - "exit 7\n".len() - "historical-check-result".len())
            )
        );
        assert!(message.output.len() <= 4096);
        assert!(result_events.iter().any(|event|matches!(&event.event, SessionEvent::AssistantMessageStarted { message:live } if live.id==message.id)));
        drop(events);
        drop(runtime);
        let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database), loader)
            .await
            .unwrap();
        let restored = reopened
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(restored.messages, finished.messages);
        assert_eq!(restored.runs, finished.runs);
        assert_eq!(restored.goal, finished.goal);
        reopened.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_for_goal_blocks_after_three_failed_checks_with_durable_evidence() {
    const COMMAND: &str =
        "printf checked >> three-checks; cat check-credential; printf failure-evidence; exit 7";
    let directory = TempDir::new().unwrap();
    tokio::fs::write(
        directory.path().join("check-credential"),
        b"DB_PASSWORD=hunter2hunter2\n",
    )
    .await
    .unwrap();
    let database = directory.path().join("sessions.sqlite3");
    let calls = Arc::new(AtomicUsize::new(0));
    let loader = Arc::new(Loader {
        failure_tail: false,
        loads: Arc::new(AtomicUsize::new(0)),
        calls: calls.clone(),
    });
    let mut options = SessionRuntimeOptions::new(database.clone());
    options.grant_authority = Some(Arc::new(Authority(COMMAND)));
    let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let CommandOutcome::SessionCreated { session_id } =
        explicit_check_session(&runtime, workspace_id, None)
            .await
            .outcome
    else {
        panic!("session")
    };
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session_id,"objective":"check until verified",
                "check":{"command":COMMAND,"timeout_ms":5000},
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("goal")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::ForGoal,
                    command: COMMAND.to_owned(),
                },
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEvent::GoalUpdated { goal } = events.next().await.unwrap().unwrap().event
                && matches!(
                    goal.status,
                    qq_protocol::GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::CheckFailing
                    }
                )
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    let goal = snapshot.goal.as_ref().unwrap();
    assert_eq!(goal.counters.runs, 3);
    assert_eq!(goal.counters.audits, 3);
    assert_eq!(goal.counters.check_failures, 3);
    assert_eq!(
        goal.check_authorization,
        Some(qq_protocol::GoalCheckAuthorization::ForGoal)
    );
    assert_eq!(
        goal.check_output.as_deref(),
        Some("exit 7\nDB_PASSWORD=[masked:credential]\nfailure-evidence")
    );
    assert!(!goal.audit_pending);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(snapshot.runs.len(), 6);
    assert!(snapshot.runs.iter().all(|run| run.outcome.is_some()));
    let checks: Vec<_> = snapshot
        .runs
        .iter()
        .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
        .collect();
    assert_eq!(checks.len(), 3);
    for run in checks {
        assert!(matches!(run.outcome, Some(RunOutcome::Failed { .. })));
        assert!(
            snapshot
                .messages
                .iter()
                .any(|message| message.run_id == run.id
                    && message.output
                        == "exit 7\nDB_PASSWORD=[masked:credential]\nfailure-evidence")
        );
    }
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("three-checks"))
            .await
            .unwrap(),
        "checkedcheckedchecked"
    );
    drop(events);
    drop(runtime);
    let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database), loader)
        .await
        .unwrap();
    let restored = reopened
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(restored.goal, snapshot.goal);
    assert_eq!(restored.runs, snapshot.runs);
    assert_eq!(restored.messages, snapshot.messages);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "reopen must not resume a blocked goal"
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn goal_check_policy_recheck_refuses_changed_policy_and_stale_goals_before_spawn() {
    const COMMAND: &str = "printf executed >> original-check";
    struct HeldPolicy {
        calls: AtomicUsize,
        deny: bool,
        ceiling: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl WorkspacePolicyAuthority for HeldPolicy {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            let ceiling = self.ceiling.load(Ordering::SeqCst);
            Box::pin(std::future::ready(if ceiling == 0 {
                Err(GoalPolicyError::Unavailable)
            } else {
                Ok(GoalPolicy {
                    max_goal_runs: ceiling as u32,
                    ..GoalPolicy::default()
                })
            }))
        }

        fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert_eq!(command, COMMAND);
            let execution = self.calls.fetch_add(1, Ordering::SeqCst) == 1;
            let entered = self.entered.clone();
            let release = self.release.clone();
            let deny = self.deny;
            Box::pin(async move {
                if execution {
                    entered.notify_one();
                    release.notified().await;
                    if deny {
                        return Err(GoalCheckPolicyError::ShellDenied);
                    }
                }
                Ok(())
            })
        }
    }
    for mode in [
        "managed",
        "goal_limit",
        "goal_unavailable",
        "edit",
        "replace",
        "clear",
    ] {
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let ceiling = Arc::new(AtomicUsize::new(2000));
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.grant_authority = Some(Arc::new(HeldPolicy {
            calls: AtomicUsize::new(0),
            deny: mode == "managed",
            ceiling: ceiling.clone(),
            entered: entered.clone(),
            release: release.clone(),
        }));
        let runtime = SessionRuntime::open(
            options,
            Arc::new(Loader {
                failure_tail: false,
                loads: Arc::new(AtomicUsize::new(0)),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            explicit_check_session(&runtime, workspace_id, None)
                .await
                .outcome
        else {
            panic!("session")
        };
        let receipt = runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"old objective",
            "check":{"command":COMMAND,"timeout_ms":5000},"budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal")
        };
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::ApproveCheck {
                        scope: qq_protocol::GoalCheckScope::Once,
                        command: COMMAND.to_owned(),
                    },
                },
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal: approved, .. } = receipt.outcome else {
            panic!("approved goal")
        };
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let before = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        let check_id = before
            .runs
            .iter()
            .find(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .unwrap()
            .id;
        if mode == "goal_limit" {
            ceiling.store(1, Ordering::SeqCst);
        } else if mode == "goal_unavailable" {
            ceiling.store(0, Ordering::SeqCst);
        }
        if matches!(mode, "edit" | "replace" | "clear") {
            let expected =
                serde_json::json!({"goal_id":approved.goal_id,"revision":approved.revision});
            let command = if mode == "clear" {
                serde_json::json!({"type":"goal_control","session_id":session_id,"expected":expected,"action":{"type":"clear"}})
            } else {
                serde_json::json!({"type":"set_goal","session_id":session_id,"expected":expected,"replace":mode=="replace","objective":"new objective",
                    "check":{"command":"printf forbidden >> replacement-check","timeout_ms":5000},"budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}})
            };
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    serde_json::from_value(command).unwrap(),
                )
                .await
                .unwrap();
        }
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let SessionEvent::RunFinished { run_id, .. } =
                    events.next().await.unwrap().unwrap().event
                    && run_id == check_id
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
        let after = runtime
            .snapshot(SnapshotRequest::new(
                workspace_id,
                Some(session_id),
                256,
                256,
            ))
            .await
            .unwrap()
            .focused
            .unwrap();
        assert!(
            !directory.path().join("original-check").exists(),
            "{mode}: stale/denied check executed"
        );
        assert!(!directory.path().join("replacement-check").exists());
        assert_eq!(after.runs.len(), 2);
        assert!(after.runs.iter().all(|run| run.outcome.is_some()));
        assert!(
            after
                .messages
                .iter()
                .any(|message| message.run_id == check_id && !message.output.is_empty())
        );
        if mode == "clear" {
            assert!(after.goal.is_none());
        } else {
            let goal = after.goal.unwrap();
            assert_eq!(goal.counters.audits, 0);
            assert_eq!(goal.counters.check_failures, 0);
            assert!(goal.pending_run_id.is_none());
            if matches!(mode, "managed" | "goal_limit" | "goal_unavailable") {
                assert!(matches!(
                    goal.status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::Configuration { .. }
                        }
                    }
                ));
                assert_eq!(
                    goal.check_authorization,
                    Some(qq_protocol::GoalCheckAuthorization::Once)
                );
                assert!(goal.audit_pending);
            } else {
                assert_eq!(goal.objective, "new objective");
                assert_eq!(
                    goal.check_authorization,
                    Some(qq_protocol::GoalCheckAuthorization::Pending)
                );
                assert!(!goal.audit_pending);
                assert!(goal.achievement_claim.is_none());
                assert!(matches!(
                    goal.status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::CheckApproval
                        }
                    }
                ));
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn goal_check_process_accepts_non_utf8_process_names() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let result = run_process(
        directory.path().to_owned(),
        "printf '\\377' > /proc/self/comm; printf checked".to_owned(),
        tokio::time::Instant::now() + Duration::from_secs(5),
        RunCancellation::new(),
        Arc::new(crate::ProcessProducer::new("test")),
        execution::RunResources::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    assert_eq!(result.tail, "checked");
}

#[tokio::test]
async fn goal_check_missing_workspace_settles_without_poisoning_other_sessions() {
    const COMMAND: &str = "printf checked";
    struct HeldPolicy {
        calls: AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl WorkspacePolicyAuthority for HeldPolicy {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Authority(COMMAND).goal_policy(Path::new("."))
        }
        fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert_eq!(command, COMMAND);
            let hold = self.calls.fetch_add(1, Ordering::SeqCst) == 1;
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                if hold {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(())
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let workspace = directory.path().join("work");
    std::fs::create_dir(&workspace).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")).with_grant_authority(
            Arc::new(HeldPolicy {
                calls: AtomicUsize::new(0),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
        ),
        Arc::new(Loader {
            failure_tail: false,
            loads: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, &workspace).await;
    let CommandOutcome::SessionCreated { session_id } =
        explicit_check_session(&runtime, workspace_id, None)
            .await
            .outcome
    else {
        panic!("session")
    };
    let receipt = runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
        "type":"set_goal", "session_id":session_id, "objective":"verify work", "check":{"command":COMMAND,"timeout_ms":5000},
        "budget":{"max_goal_runs":2,"deadline_ms":now_ms()+60_000}
    })).unwrap()).await.unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("goal")
    };
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::Once,
                    command: COMMAND.into(),
                },
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    std::fs::rename(&workspace, directory.path().join("moved-work")).unwrap();
    #[cfg(unix)]
    let opened = crate::workspace::Workspace::observe_next_open(&workspace);
    release.notify_one();
    #[cfg(unix)]
    assert_ne!(
        opened.await.unwrap(),
        std::thread::current().id(),
        "check filesystem opens must not execute on the Tokio worker"
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events
                .next()
                .await
                .unwrap()
                .expect("workspace failure must not poison the runtime");
            if let SessionEvent::RunFinished {
                origin: Some(qq_protocol::RunOrigin::GoalCheck { .. }),
                outcome,
                ..
            } = event.event
            {
                assert!(matches!(outcome, RunOutcome::Failed { .. }));
                break;
            }
        }
    })
    .await
    .unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert!(focused.summary.active_run_id.is_none());
    assert!(
        focused
            .messages
            .iter()
            .any(|message| format!("{message:?}").contains(if cfg!(unix) {
                "workspace"
            } else {
                "unavailable"
            }))
    );
    let (other, _) = resolve_workspace(&runtime, directory.path()).await;
    explicit_check_session(&runtime, other, None).await;
    runtime.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn portable_check_group_confirmation_waits_until_the_group_is_absent() {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exec sleep 30"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    let waiting = crate::sessions::goal_check::confirm_reaped_group_exit(pgid);
    tokio::pin!(waiting);
    let premature = futures_util::poll!(&mut waiting).is_ready();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !premature,
        "a kill signal is not confirmation that a live group exited"
    );
    waiting.await.unwrap();
}
