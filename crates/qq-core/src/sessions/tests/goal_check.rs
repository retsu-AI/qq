use super::*;

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
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failure_tail && call >= 2 {
            assert!(
                request_texts(&request)
                    .iter()
                    .any(|text| text.contains("exit 7")),
                "failed check exit status must reach the next goal run"
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
impl WorkspaceGrantAuthority for Authority {
    fn seed_grants(&self, _: &Path) -> GrantSeedFuture {
        Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
    }
    fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> GrantPromotionFuture {
        panic!("exact goal-check authority must not promote ordinary grants")
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
    let command = if success {
        "printf checked >> goal-check-marker"
    } else {
        "printf checked >> goal-check-marker; exit 7"
    };
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("sessions.sqlite3");
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
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"verify checked completion",
                "check":{"command":command,"timeout_ms":5000},
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
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
        if success { 2 } else { 4 },
        "only the work run may call the model"
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
        crate::workspace::Workspace::open(directory.path()).unwrap(),
        "i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done; printf end; exit 7"
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

#[cfg(unix)]
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
            crate::workspace::Workspace::open(directory.path()).unwrap(),
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
        crate::workspace::Workspace::open(directory.path()).unwrap(),
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
    cancellation.cancel();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Cancelled);
    #[cfg(target_os = "linux")]
    {
        // An orphan may await init's reap; it must no longer execute.
        if let Ok(stat) = tokio::fs::read_to_string(format!("/proc/{descendant}/stat")).await {
            assert_eq!(stat.rsplit_once(") ").unwrap().1.as_bytes()[0], b'Z');
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = descendant;
}

#[cfg(unix)]
#[tokio::test]
async fn goal_check_process_kills_background_descendants_before_success() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let result = run_process(
        crate::workspace::Workspace::open(directory.path()).unwrap(),
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

#[cfg(unix)]
#[tokio::test]
async fn goal_check_process_timeout_and_binary_output_stay_bounded() {
    use super::super::goal_check::{ProcessOutcome, run_process};
    let directory = TempDir::new().unwrap();
    let result = run_process(
        crate::workspace::Workspace::open(directory.path()).unwrap(),
        "i=0; while [ \"$i\" -lt 5000 ]; do printf '\\377'; i=$((i+1)); done; printf end; sleep 60"
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

#[tokio::test]
async fn goal_checks_prepared_together_in_one_workspace_do_not_deadlock() {
    struct CoordinatedAuthority {
        checks: AtomicUsize,
        barrier: Arc<tokio::sync::Barrier>,
    }
    impl WorkspaceGrantAuthority for CoordinatedAuthority {
        fn seed_grants(&self, path: &Path) -> GrantSeedFuture {
            Authority("printf checked >> goal-check-marker").seed_grants(path)
        }
        fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> GrantPromotionFuture {
            panic!("not an ordinary grant")
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
        let created = create_session(&runtime, workspace_id, None).await;
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
    impl WorkspaceGrantAuthority for HeldAuthority {
        fn seed_grants(&self, path: &Path) -> GrantSeedFuture {
            Authority("printf checked >> goal-check-marker").seed_grants(path)
        }
        fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> GrantPromotionFuture {
            panic!("not an ordinary grant")
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
        let created = create_session(&runtime, workspace_id, None).await;
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

#[tokio::test]
async fn goal_check_historical_result_survives_edit_replace_and_clear() {
    const COMMAND: &str = "printf historical-check-result; exit 7";
    for mutation in ["edit", "replace", "clear"] {
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
            create_session(&runtime, workspace_id, None).await.outcome
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
        let command = if mutation == "clear" {
            serde_json::json!({"type":"goal_control","session_id":session_id,"expected":expected,"action":{"type":"clear"}})
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
        if let Some(goal) = &mut changed {
            assert!(goal.pending_run_id.is_none() || goal.pending_run_id == Some(check));
            // Settlement may release only its own draining reservation.
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
        assert_eq!(message.output, "exit 7\nhistorical-check-result");
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
