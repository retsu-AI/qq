use super::*;
use crate::test_support::within;

#[tokio::test]
async fn goal_check_policy_denies_new_authority_but_not_committed_replays() {
    struct Authority {
        allow: AtomicBool,
        unavailable: AtomicBool,
        checks: AtomicUsize,
    }
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy::default())))
        }

        fn validate_goal_check(&self, workspace: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert!(workspace.is_absolute());
            assert_eq!(command, "true");
            self.checks.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(
                if self.unavailable.load(Ordering::SeqCst) {
                    Err(GoalCheckPolicyError::Unavailable)
                } else if self.allow.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(GoalCheckPolicyError::ShellDenied)
                },
            ))
        }
    }
    let authority = Arc::new(Authority {
        allow: AtomicBool::new(false),
        unavailable: AtomicBool::new(false),
        checks: AtomicUsize::new(0),
    });
    let directory = TempDir::new().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.policy_authority = Some(authority.clone());
    let runtime = SessionRuntime::open(options, Arc::new(ScriptedLoader))
        .await
        .unwrap();
    let held = runtime.hold_scheduling().await;
    let (workspace_id, session_id, _) =
        new_session(&runtime, directory.path(), ApprovalMode::Ask).await;
    let set = goal_command(
        serde_json::json!({"session_id":session_id, "objective":"checked work",
            "check":{"command":"true"},
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
        }),
    );
    let receipt = runtime
        .command(CommandId::generate().unwrap(), set)
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing goal")
    };
    assert_eq!(authority.checks.load(Ordering::SeqCst), 0);
    let approve: SessionCommand = serde_json::from_value(serde_json::json!({
        "type":"goal_control", "session_id":session_id,
        "expected":{"goal_id":goal.goal_id,"revision":1},
        "action":{"type":"approve_check","scope":"for_goal","command":"true"}
    }))
    .unwrap();
    assert!(matches!(
        runtime
            .command(CommandId::generate().unwrap(), approve.clone())
            .await,
        Err(SessionRuntimeError::GoalCheckPolicy(
            GoalCheckPolicyError::ShellDenied
        ))
    ));
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.focused.unwrap().goal, Some(goal));
    authority.allow.store(true, Ordering::SeqCst);
    let id = CommandId::generate().unwrap();
    let receipt = runtime.command(id, approve.clone()).await.unwrap();
    runtime.store().drive_goals(now_ms()).await.unwrap();
    let stable = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let stable_goal = stable.focused.as_ref().unwrap().goal.as_ref().unwrap();
    let CommandOutcome::GoalUpdated { goal: approved, .. } = &receipt.outcome else {
        panic!("approved goal")
    };
    let mut expected = approved.clone();
    expected.pending_run_id = stable_goal.pending_run_id;
    assert_eq!(stable_goal, &expected);
    let pending = stable_goal
        .pending_run_id
        .expect("authorized active goal queues work");
    assert!(stable.focused.as_ref().unwrap().runs.iter().any(|run|run.id==pending && matches!(run.origin,Some(qq_protocol::RunOrigin::Goal{goal_id,revision}) if goal_id==approved.goal_id && revision==approved.revision)));
    authority.allow.store(false, Ordering::SeqCst);
    for unavailable in [false, true] {
        authority.unavailable.store(unavailable, Ordering::SeqCst);
        assert_eq!(runtime.command(id, approve.clone()).await.unwrap(), receipt);
        let mut fresh = serde_json::to_value(&approve).unwrap();
        fresh["expected"]["revision"] = 2.into();
        let error = runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(fresh).unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionRuntimeError::GoalCheckPolicy(ref error)
            if *error == if unavailable { GoalCheckPolicyError::Unavailable }
                else { GoalCheckPolicyError::ShellDenied })
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
            .await
            .unwrap();
        assert_eq!(snapshot.cursor, stable.cursor);
        assert_eq!(snapshot.focused.unwrap().goal.as_ref(), Some(stable_goal));
    }
    let mut changed = serde_json::to_value(&approve).unwrap();
    changed["expected"]["revision"] = 2.into();
    assert!(matches!(
        runtime
            .command(id, serde_json::from_value(changed).unwrap())
            .await,
        Err(SessionRuntimeError::IdempotencyConflict)
    ));
    goal_control(
        &runtime,
        session_id,
        identity_of(stable_goal),
        qq_protocol::GoalControl::Pause,
    )
    .await
    .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession { session_id },
        )
        .await
        .unwrap();
    assert_eq!(runtime.command(id, approve).await.unwrap(), receipt);
    {
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(matches!(
            futures_util::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));
        drop(held);
        shutdown.await.unwrap();
    }
}

#[tokio::test]
async fn goal_check_approval_requires_exact_command_and_revision() {
    let (directory, runtime) = test_runtime().await;
    let held = runtime.hold_scheduling().await;
    let (workspace_id, session_id, _) =
        new_session(&runtime, directory.path(), ApprovalMode::Ask).await;
    let set: SessionCommand = goal_command(
        serde_json::json!({"session_id":session_id, "objective":"checked work",
            "check":{"command":"true"},
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
        }),
    );
    let receipt = runtime
        .command(CommandId::generate().unwrap(), set)
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing goal")
    };
    assert!(matches!(
        goal.status,
        qq_protocol::GoalStatus::Paused { .. }
    ));
    let approve: SessionCommand = serde_json::from_value(serde_json::json!({
        "type":"goal_control", "session_id":session_id,
        "expected":{"goal_id":goal.goal_id,"revision":1},
        "action":{"type":"approve_check","scope":"once","command":"true"}
    }))
    .expect("exact-command approval is public wire data");
    let receipt = runtime
        .command(CommandId::generate().unwrap(), approve)
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing updated goal")
    };
    assert_eq!(goal.revision, 2);
    assert_eq!(
        goal.check_authorization,
        Some(qq_protocol::GoalCheckAuthorization::Once)
    );
    assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
    for (command, revision) in [("false", 2), ("true", 1)] {
        let approve: SessionCommand = serde_json::from_value(serde_json::json!({
            "type":"goal_control", "session_id":session_id,
            "expected":{"goal_id":goal.goal_id,"revision":revision},
            "action":{"type":"approve_check","scope":"for_goal","command":command}
        }))
        .unwrap();
        assert!(
            runtime
                .command(CommandId::generate().unwrap(), approve)
                .await
                .is_err()
        );
    }
    runtime.store().drive_goals(now_ms()).await.unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let current = focused.goal.as_ref().unwrap();
    let mut expected = goal.clone();
    expected.pending_run_id = current.pending_run_id;
    assert_eq!(current, &expected);
    let pending = current
        .pending_run_id
        .expect("once authorization permits goal work queueing");
    assert!(focused.runs.iter().any(|run|run.id==pending && matches!(run.origin,Some(qq_protocol::RunOrigin::Goal{goal_id,revision}) if goal_id==goal.goal_id && revision==goal.revision)));
    {
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(matches!(
            futures_util::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));
        drop(held);
        shutdown.await.unwrap();
    }
}

#[tokio::test]
async fn goal_workspace_policy_fences_direct_commands_and_preserves_replays_and_stops() {
    struct Authority {
        limits: StdMutex<HashMap<PathBuf, GoalPolicy>>,
        unavailable: AtomicBool,
        pending: AtomicBool,
    }
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, workspace: &Path) -> GoalPolicyFuture {
            if self.pending.load(Ordering::SeqCst) {
                return Box::pin(std::future::pending());
            }
            let result = if self.unavailable.load(Ordering::SeqCst) {
                Err(GoalPolicyError::Unavailable)
            } else {
                Ok(self.limits.lock().unwrap()[workspace])
            };
            Box::pin(std::future::ready(result))
        }
    }
    let directory = TempDir::new().unwrap();
    let other = directory.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let authority = Arc::new(Authority {
        limits: StdMutex::new(HashMap::from([
            (
                directory.path().to_owned(),
                GoalPolicy {
                    max_goal_runs: 20,
                    ..GoalPolicy::default()
                },
            ),
            (
                other.clone(),
                GoalPolicy {
                    max_goal_runs: 100,
                    ..GoalPolicy::default()
                },
            ),
        ])),
        unavailable: AtomicBool::new(false),
        pending: AtomicBool::new(false),
    });
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.policy_authority = Some(authority.clone());
    let runtime = SessionRuntime::open(options, Arc::new(ScriptedLoader))
        .await
        .unwrap();
    let _held = runtime.hold_scheduling().await;
    for (path, ceiling) in [(directory.path(), 20_u32), (other.as_path(), 100)] {
        let (_, session_id, _) = new_session(&runtime, path, ApprovalMode::default()).await;
        let deadline = now_ms() + 60_000;
        let set = |runs| {
            goal_command(
                serde_json::json!({"session_id":session_id, "objective":"workspace-bound",
                    "budget":{"max_goal_runs":runs,"deadline_ms":deadline}
                }),
            )
        };
        assert!(matches!(
            runtime
                .command(CommandId::generate().unwrap(), set(ceiling + 1))
                .await,
            Err(SessionRuntimeError::InvalidGoal(
                GoalValidationError::Budget
            ))
        ));
        let command = CommandId::generate().unwrap();
        let accepted = runtime.command(command, set(ceiling)).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = &accepted.outcome else {
            panic!("goal")
        };
        let identity = qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        authority.unavailable.store(true, Ordering::SeqCst);
        authority.pending.store(true, Ordering::SeqCst);
        assert_eq!(
            within(runtime.command(command, set(ceiling)))
                .await
                .unwrap()
                .unwrap(),
            accepted
        );
        authority.pending.store(false, Ordering::SeqCst);
        assert!(matches!(
            runtime
                .command(CommandId::generate().unwrap(), set(ceiling))
                .await,
            Err(SessionRuntimeError::GoalPolicy(
                GoalPolicyError::Unavailable
            ))
        ));
        let paused = goal_control(
            &runtime,
            session_id,
            identity,
            qq_protocol::GoalControl::Pause,
        )
        .await
        .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = paused.outcome else {
            panic!("paused")
        };
        let expected = qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        authority.unavailable.store(false, Ordering::SeqCst);
        authority
            .limits
            .lock()
            .unwrap()
            .get_mut(path)
            .unwrap()
            .max_goal_runs = ceiling - 1;
        assert!(matches!(
            goal_control(
                &runtime,
                session_id,
                expected,
                qq_protocol::GoalControl::Resume
            )
            .await,
            Err(SessionRuntimeError::InvalidGoal(
                GoalValidationError::Budget
            ))
        ));
        authority.unavailable.store(true, Ordering::SeqCst);
        goal_control(
            &runtime,
            session_id,
            expected,
            qq_protocol::GoalControl::Clear,
        )
        .await
        .unwrap();
        authority.unavailable.store(false, Ordering::SeqCst);
    }
    drop(_held);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn goal_policy_schema_rejects_bad_shapes_without_advancing_the_version() {
    let previous = STORE_SCHEMA_VERSION;
    for definition in ["INTEGER", "TEXT"] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (connection, _) = open_database(&path).unwrap();
        connection
            .execute_batch(&format!(
                "ALTER TABLE session_goals DROP COLUMN specification_at_ms;
            ALTER TABLE session_goals DROP COLUMN policy_json;
            ALTER TABLE session_goals ADD COLUMN policy_json {definition};
            UPDATE metadata SET value='{previous}' WHERE key='schema_version';"
            ))
            .unwrap();
        drop(connection);
        assert!(matches!(
            open_database(&path),
            Err(SessionRuntimeError::CONSTRAINT)
        ));
        let connection = Connection::open(&path).unwrap();
        let version: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, previous.to_string());
        let columns: u32 = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('session_goals') WHERE name='specification_at_ms'",
            [], |row| row.get(0)
        ).unwrap();
        assert_eq!(
            columns, 0,
            "a failed migration must roll back the new anchor column"
        );
    }
}

#[test]
fn goal_policy_checks_each_budget_dimension_against_its_fixed_anchor() {
    let budget = qq_protocol::GoalBudget {
        max_goal_runs: 5,
        deadline_ms: 5000,
        max_total_tokens: None,
        max_cost_usd_nanos: None,
        per_run: RunLimits {
            max_model_turns: Some(4),
            ..RunLimits::default()
        },
    };
    let check = qq_protocol::GoalCheck {
        command: "true".to_owned(),
        timeout_ms: 100,
    };
    let policy = GoalPolicy {
        max_duration_ms: 4000,
        max_goal_runs: 5,
        max_check_timeout_ms: 100,
        per_run: RunLimits {
            max_model_turns: Some(4),
            ..RunLimits::default()
        },
        ..GoalPolicy::default()
    };
    policy
        .validate_specification(&budget, Some(&check), Some(1000))
        .unwrap();
    assert!(matches!(
        policy.validate_specification(&budget, Some(&check), Some(999)),
        Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Budget
        ))
    ));
    let mut changed = budget.clone();
    changed.per_run.max_model_turns = Some(5);
    assert!(matches!(
        policy.validate_specification(&changed, Some(&check), Some(1000)),
        Err(SessionRuntimeError::InvalidRunLimits)
    ));
    changed.per_run.max_model_turns = None;
    assert!(matches!(
        policy.validate_specification(&changed, Some(&check), Some(1000)),
        Err(SessionRuntimeError::InvalidRunLimits)
    ));
    let check = qq_protocol::GoalCheck {
        timeout_ms: 101,
        ..check
    };
    assert!(matches!(
        policy.validate_specification(&budget, Some(&check), Some(1000)),
        Err(SessionRuntimeError::InvalidGoal(GoalValidationError::Check))
    ));
}

#[tokio::test]
async fn goal_policy_tightening_before_load_or_main_start_prevents_provider_work() {
    struct Authority(Arc<AtomicUsize>);
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            let max = self.0.load(Ordering::SeqCst);
            Box::pin(std::future::ready(if max == 0 {
                Err(GoalPolicyError::Unavailable)
            } else {
                Ok(GoalPolicy {
                    max_goal_runs: max as u32,
                    ..GoalPolicy::default()
                })
            }))
        }
    }
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            provider_script([completed(None)])
        }
    }
    struct Loader {
        loads: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        hold: bool,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.loads.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&self.requests);
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let hold = self.hold;
            Box::pin(async move {
                entered.notify_one();
                if hold {
                    release.notified().await;
                }
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for (during_loading, lowered) in [(false, 19), (true, 19), (false, 0), (true, 0)] {
        let directory = TempDir::new().unwrap();
        let ceiling = Arc::new(AtomicUsize::new(20));
        let loads = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.policy_authority = Some(Arc::new(Authority(Arc::clone(&ceiling))));
        let runtime = SessionRuntime::open(
            options,
            Arc::new(Loader {
                loads: Arc::clone(&loads),
                requests: Arc::clone(&requests),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                hold: during_loading,
            }),
        )
        .await
        .unwrap();
        let held = runtime.hold_scheduling().await;
        let (workspace, session_id, created) =
            new_session(&runtime, directory.path(), ApprovalMode::default()).await;
        let mut events = subscribe_after(&runtime, workspace, created.committed_through);
        let command = goal_command(
            serde_json::json!({"session_id":session_id,"objective":"honor current policy",
                "budget":{"max_goal_runs":20,"deadline_ms":now_ms()+60_000}
            }),
        );
        runtime
            .command(CommandId::generate().unwrap(), command)
            .await
            .unwrap();
        if during_loading {
            drop(held);
            within(entered.notified()).await.unwrap();
            ceiling.store(lowered, Ordering::SeqCst);
            release.notify_one();
        } else {
            ceiling.store(lowered, Ordering::SeqCst);
            drop(held);
        }
        let _ = collect_through_finished(&mut events).await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "no provider request after policy changed"
        );
        assert_eq!(loads.load(Ordering::SeqCst), usize::from(during_loading));
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace, Some(session_id), 1, 1))
            .await
            .unwrap();
        assert!(matches!(
            snapshot.focused.unwrap().goal.unwrap().status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::NeedsUser {
                    cause: qq_protocol::GoalNeedsUser::Configuration { .. }
                }
            }
        ));
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn goal_provider_backoff_uses_durable_policy_without_shortening_model_wait() {
    for wait_ms in [0, 60_000] {
        let (directory, store, claim) = model_goal_claim(true).await;
        let session = claim.identity.session_id;
        let id = claim.goal.as_ref().unwrap().goal_id;
        store
            .call(Priority::Control, move |connection| {
                let (_, at) = goal_policy::load(connection, id)?;
                goal_policy::save(
                    connection,
                    id,
                    GoalPolicy {
                        max_backoff_ms: 1000,
                        ..GoalPolicy::default()
                    },
                    at,
                )?;
                if wait_ms > 0 {
                    let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                    goal.wait = Some(qq_protocol::GoalWait {
                        until_ms: now_ms() + wait_ms,
                        reason: "model wait".to_owned(),
                    });
                    crate::sessions::goals::save(connection, &goal)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let before = now_ms();
        store
            .finish_run(
                &claim,
                RunOutcome::Paused {
                    pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                        kind: RunFailureKind::ProviderUnavailable,
                        message: "offline".to_owned(),
                        turn_ordinal: 1,
                        attempts: 1,
                    }),
                },
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let goal = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert!(goal.next_run_at_ms >= before + 1000);
        if wait_ms == 0 {
            assert!(goal.next_run_at_ms <= now_ms() + 1000);
        } else {
            assert_eq!(goal.next_run_at_ms, goal.wait.as_ref().unwrap().until_ms);
        }
        store.close().await.unwrap();
        let (connection, _) = open_database(&directory.path().join("sessions.sqlite3")).unwrap();
        assert_eq!(
            goal_policy::load(&connection, id).unwrap().0.max_backoff_ms,
            1000
        );
        assert_eq!(
            crate::sessions::goals::load_live(&connection, session)
                .unwrap()
                .unwrap(),
            goal
        );
    }
}

#[tokio::test]
async fn auto_goal_checks_derive_contained_authority_without_human_consent() {
    struct Authority(bool);
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy::default())))
        }
        fn validate_goal_check(&self, _: &Path, _: &str) -> GoalCheckPolicyFuture {
            Box::pin(std::future::ready(if self.0 {
                Err(GoalCheckPolicyError::ShellDenied)
            } else {
                Ok(())
            }))
        }
    }
    let directory = TempDir::new().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut runtimes = Vec::new();
    for denied in [false, true] {
        let mut options =
            SessionRuntimeOptions::new(root.join(format!("sessions-{denied}.sqlite3")));
        options.policy_authority = Some(Arc::new(Authority(denied)));
        let runtime = SessionRuntime::open(options, Arc::new(ScriptedLoader))
            .await
            .unwrap();
        let held = runtime.hold_scheduling().await;
        runtimes.push((runtime, held));
    }
    let mut row = 0;
    for (mode, denied, command, expected) in [
        (ApprovalMode::Auto, false, "true", "task_scoped"),
        (ApprovalMode::Auto, true, "true", "denied"),
        (ApprovalMode::Auto, false, "sudo true", "forbidden"),
        (ApprovalMode::Ask, false, "true", "pending"),
        (ApprovalMode::Full, false, "true", "pending"),
    ] {
        for scope in [
            None,
            Some(qq_protocol::GoalCheckScope::Once),
            Some(qq_protocol::GoalCheckScope::ForGoal),
        ] {
            row += 1;
            let workspace = root.join(format!("work-{row}"));
            std::fs::create_dir(&workspace).unwrap();
            let runtime = &runtimes[usize::from(denied)].0;
            let (workspace_id, _) = resolve_workspace(runtime, &workspace).await;
            let created = create_session_with_mode(runtime, workspace_id, None, mode).await;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("session")
            };
            let id = CommandId::generate().unwrap();
            let command: SessionCommand = goal_command(serde_json::json!({"session_id":session_id,
                "objective":"verify the task without permission prompts",
                "check":{"command":command,"timeout_ms":1000},
                "check_approval":scope,
                "budget":{"max_goal_runs":2,"deadline_ms":now_ms()+60_000}
            }));
            let result = runtime.command(id, command.clone()).await;
            match expected {
                "denied" => assert!(matches!(
                    result,
                    Err(SessionRuntimeError::GoalCheckPolicy(
                        GoalCheckPolicyError::ShellDenied
                    ))
                )),
                "forbidden" => assert!(matches!(
                    result,
                    Err(SessionRuntimeError::GoalCheckForbidden)
                )),
                authorization => {
                    let authorization = match (authorization, scope) {
                        ("task_scoped" | "pending", Some(qq_protocol::GoalCheckScope::Once)) => {
                            "once"
                        }
                        ("task_scoped" | "pending", Some(qq_protocol::GoalCheckScope::ForGoal)) => {
                            "for_goal"
                        }
                        (value, _) => value,
                    };
                    let receipt = result.unwrap();
                    let CommandOutcome::GoalUpdated { goal, .. } = &receipt.outcome else {
                        panic!("goal receipt")
                    };
                    assert_eq!(
                        serde_json::to_value(goal.check_authorization).unwrap(),
                        authorization
                    );
                    assert_eq!(
                        goal.status == qq_protocol::GoalStatus::Active,
                        authorization != "pending"
                    );
                    assert_eq!(goal.revision, 1);
                    assert_eq!(runtime.command(id, command).await.unwrap(), receipt);
                    if authorization != "pending" {
                        goal_control(
                            runtime,
                            session_id,
                            identity_of(goal),
                            qq_protocol::GoalControl::Pause,
                        )
                        .await
                        .unwrap();
                    }
                }
            }
            if matches!(expected, "denied" | "forbidden") {
                let snapshot = runtime
                    .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
                    .await
                    .unwrap();
                assert!(
                    snapshot.focused.unwrap().goal.is_none(),
                    "refused checks must not leave a paused goal requiring consent"
                );
            }
        }
    }
    for (runtime, held) in runtimes {
        drop(held);
        runtime.shutdown().await.unwrap();
    }
}
