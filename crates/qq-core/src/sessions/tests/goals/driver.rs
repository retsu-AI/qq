use super::*;
use crate::test_support::within;

#[tokio::test]
async fn goal_resume_refuses_spent_unknown_or_draining_state() {
    for condition in [
        "tokens",
        "tokens_unknown",
        "cost",
        "cost_unknown",
        "draining",
    ] {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, session_id, _) =
            new_session(&runtime, directory.path(), ApprovalMode::default()).await;
        let command: SessionCommand = goal_command(
            serde_json::json!({"session_id":session_id, "objective":"bounded task",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000,
                    "max_total_tokens":20,"max_cost_usd_nanos":10}
            }),
        );
        runtime
            .command(CommandId::generate().unwrap(), command)
            .await
            .unwrap();
        let goal = runtime
            .store()
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.status = qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::User,
                };
                match condition {
                    "tokens" => goal.counters.usage = Some(usage(10, 10)),
                    "tokens_unknown" => goal.counters.usage = None,
                    "cost" => goal.counters.estimated_cost_usd_nanos = Some(10),
                    "cost_unknown" => goal.counters.estimated_cost_usd_nanos = None,
                    "draining" => goal.pending_run_id = Some(RunId::generate().unwrap()),
                    _ => unreachable!(),
                }
                crate::sessions::goals::save(connection, &goal)?;
                Ok(goal)
            })
            .await
            .unwrap();
        let resume: SessionCommand = serde_json::from_value(serde_json::json!({
            "type":"goal_control", "session_id":session_id,
            "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
            "action":{"type":"resume"}
        }))
        .unwrap();
        assert!(
            runtime
                .command(CommandId::generate().unwrap(), resume)
                .await
                .is_err(),
            "{condition}"
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
            .await
            .unwrap();
        assert_eq!(snapshot.focused.unwrap().goal.as_deref(), Some(&goal));
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn active_idle_goal_dispatches_fresh_work_without_a_user_prompt() {
    struct Model {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            provider_script([
                text_delta("completed one work slice"),
                completed(Some(qq_provider::ProviderUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                })),
            ])
        }
    }
    struct Loader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model { requests }, "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(Loader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, session_id, created) =
        new_session(&runtime, directory.path(), ApprovalMode::ReadOnly).await;
    let mut events = subscribe_after(&runtime, workspace_id, created.committed_through);
    let receipt=set_goal(&runtime, serde_json::json!({"session_id":session_id,"objective":"complete a fresh autonomous work slice",
        "budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"per_run":{"max_model_turns":1}}
    })).await.unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("goal")
    };
    let observed = within(async {
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            if let SessionEvent::RunStarted {
                run_id,
                origin: Some(qq_protocol::RunOrigin::Goal { goal_id, revision }),
                ..
            } = event.event
            {
                assert_eq!((goal_id, revision), (goal.goal_id, goal.revision));
                return run_id;
            }
        }
        panic!("event stream ended before fresh goal work");
    })
    .await;
    if observed.is_err() {
        runtime.shutdown().await.unwrap();
        panic!("active idle goal did not dispatch fresh work without SubmitPrompt");
    }
    let run_id = observed.unwrap();
    let finished = collect_until_run_finished(&mut events, run_id).await;
    assert!(finished_outcome(&finished, run_id).is_some());
    let run_count: u64 = runtime
        .store()
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM runs WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        run_count, 1,
        "one fresh run is created for the finite goal allowance"
    );
    assert!(requests.lock().unwrap().iter().any(|request| {
        request_texts(request)
            .iter()
            .any(|text| text.contains("complete a fresh autonomous work slice"))
    }));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_goal_queue_passes_commit_one_fresh_pending_run() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        store.queue_due_goals(now_ms()),
        store.queue_due_goals(now_ms())
    );
    assert_eq!(first.unwrap(), None);
    assert_eq!(second.unwrap(), None);
    let (count,pending)=store.call(Priority::Control,move |connection| {
        let count:u64=connection.query_row("SELECT COUNT(*) FROM runs WHERE session_id=?1 AND json_extract(origin_json,'$.type')='goal'",[session.to_string()],|row|row.get(0))?;
        let goal=crate::sessions::goals::load_live(connection,session)?.unwrap();
        Ok((count,goal.pending_run_id))
    }).await.unwrap();
    assert_eq!(count, 1);
    let claimed = store.reserve_next_run(false).await.unwrap().unwrap();
    assert_eq!(pending, Some(claimed.identity.run_id));
    assert!(matches!(
        claimed.origin,
        Some(qq_protocol::RunOrigin::Goal { .. })
    ));
    assert!(claimed.goal.is_some());
    store
        .finish_reserved_run(&claimed, RunOutcome::Cancelled)
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_wait_deadline_exhausts_without_queueing_expired_work() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let now = now_ms();
    store
        .call(Priority::Control, move |connection| {
            let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            goal.budget.deadline_ms = now + 1000;
            goal.next_run_at_ms = now + 60_000;
            goal.wait = Some(qq_protocol::GoalWait {
                until_ms: now + 60_000,
                reason: "external dependency".to_owned(),
            });
            crate::sessions::goals::save(connection, &goal)
        })
        .await
        .unwrap();
    assert_eq!(store.queue_due_goals(now).await.unwrap(), Some(now + 1000));
    assert_eq!(store.queue_due_goals(now + 1001).await.unwrap(), None);
    let goal = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        goal.status,
        qq_protocol::GoalStatus::BudgetExhausted {
            reason: qq_protocol::GoalExhaustion::Deadline
        }
    );
    assert!(goal.pending_run_id.is_none());
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn user_prompt_preempts_unclaimed_goal_atomically_without_spending_allowance() {
    for fault in [false, true] {
        let (_directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.queue_due_goals(now_ms()).await.unwrap();
        let before = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let autonomous = before.pending_run_id.unwrap();
        if fault {
            store.call(Priority::Control,|connection| {
                connection.execute_batch("CREATE TEMP TRIGGER refuse_preemption BEFORE INSERT ON events WHEN json_extract(NEW.envelope_json,'$.event.type')='run_finished' BEGIN SELECT RAISE(ABORT,'preemption event fault'); END;")?;
                Ok(())
            }).await.unwrap();
        }
        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::SubmitPrompt {
            session_id: session,
            input: vec![InputPart::text("user redirection first")],
            limits: RunLimits::default(),
            correlation: Correlation::default(),
            output: None,
        };
        if fault {
            assert!(
                store.command(command_id, command.clone()).await.is_err(),
                "preemption must share the prompt transaction"
            );
            let unchanged = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(unchanged, before);
            store
                .call(Priority::Control, |connection| {
                    connection.execute_batch("DROP TRIGGER refuse_preemption;")?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let receipt = store.command(command_id, command).await.unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = receipt.receipt.outcome else {
            panic!("prompt")
        };
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        assert_eq!(
            claim.identity.run_id, run_id,
            "user prompt must own the next claim"
        );
        assert!(matches!(
            claim.origin,
            Some(qq_protocol::RunOrigin::UserPrompt {})
        ));
        let (status, goal) = store
            .call(Priority::Control, move |connection| {
                let status: String = connection.query_row(
                    "SELECT status FROM runs WHERE id=?1",
                    [autonomous.to_string()],
                    |row| row.get(0),
                )?;
                Ok((
                    status,
                    crate::sessions::goals::load_live(connection, session)?.unwrap(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
        assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
        assert!(goal.pending_run_id.is_none());
        assert_eq!(goal.counters, qq_protocol::GoalCounters::default());
        store
            .finish_reserved_run(&claim, RunOutcome::Cancelled)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_pause_cancels_unclaimed_work_and_fences_resume_while_draining() {
    for phase in ["queued", "reserved", "running"] {
        let (_directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.queue_due_goals(now_ms()).await.unwrap();
        let before = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let autonomous = before.pending_run_id.unwrap();
        let claimed = if phase == "queued" {
            None
        } else {
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            if phase == "running" {
                store
                    .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
            Some(claim)
        };
        let receipt = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: before.goal_id,
                        revision: before.revision,
                    },
                    action: qq_protocol::GoalControl::Pause,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
            panic!("paused goal")
        };
        assert_eq!(
            goal.status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::User
            }
        );
        let (status, cancelled): (String, bool) = store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT status,cancel_requested FROM runs WHERE id=?1",
                    [autonomous.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        if let Some(claim) = claimed {
            assert!(
                cancelled,
                "pause requests cancellation without releasing draining ownership"
            );
            assert_eq!(goal.pending_run_id, Some(autonomous));
            assert!(matches!(
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::GoalControl {
                            session_id: session,
                            expected: qq_protocol::GoalIdentity {
                                goal_id: goal.goal_id,
                                revision: goal.revision
                            },
                            action: qq_protocol::GoalControl::Resume,
                        }
                    )
                    .await,
                Err(SessionRuntimeError::GoalCancellationPending)
            ));
            if phase == "reserved" {
                store
                    .finish_reserved_run(&claim, RunOutcome::Cancelled)
                    .await
                    .unwrap();
            } else {
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Cancelled,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
        } else {
            assert_eq!(
                status, "cancelled",
                "pause settles unclaimed goal work atomically"
            );
            assert!(goal.pending_run_id.is_none());
            assert!(store.reserve_next_run(false).await.unwrap().is_none());
        }
        let drained = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert_eq!(drained.status, goal.status);
        assert!(drained.pending_run_id.is_none());
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: drained.goal_id,
                        revision: drained.revision,
                    },
                    action: qq_protocol::GoalControl::Resume,
                },
            )
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn paused_goal_waits_for_paid_detached_child_after_parent_settlement() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "detached paid work".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let child_claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(child_claim.identity.run_id, child.run_id);
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let before = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    let paused = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id: session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: before.goal_id,
                    revision: before.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = paused.receipt.outcome else {
        panic!("goal")
    };
    let cancelled: bool = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT cancel_requested FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        cancelled,
        "settled parent does not end the detached child's paid obligation"
    );
    assert!(matches!(
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision
                    },
                    action: qq_protocol::GoalControl::Resume,
                }
            )
            .await,
        Err(SessionRuntimeError::GoalCancellationPending)
    ));
    store
        .finish_run(
            &child_claim,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let resumed = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id: session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::Resume,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = resumed.receipt.outcome else {
        panic!("resumed goal")
    };
    store_command(&store, child.session_id, "unrelated later child prompt")
        .await
        .unwrap();
    let unrelated = store.claim_next_run(true).await.unwrap().unwrap();
    assert!(unrelated.goal_spend_owner.is_none());
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id: session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    assert!(
        !store
            .cancellation_requested(unrelated.identity.run_id)
            .await
            .unwrap(),
        "old ownership never cancels an unrelated later prompt"
    );
    store
        .finish_run(
            &unrelated,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn replayed_goal_controls_restore_only_unsettled_paid_cancellation_signals() {
    for mode in ["pause", "replace", "clear"] {
        let (_directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "paid descendant".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(true).await.unwrap().unwrap();
        let audit = test_prepared_audit(&claimed);
        let (helper, _) = store
            .start_in_run_compaction(&claimed, &audit.resolved_model, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
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
        let expected = qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        let command = if mode == "replace" {
            goal_command(
                serde_json::json!({"session_id":session,"objective":"replacement", "replace":true,
                "expected":expected,"budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}}),
            )
        } else {
            SessionCommand::GoalControl {
                session_id: session,
                expected,
                action: if mode == "clear" {
                    qq_protocol::GoalControl::Clear
                } else {
                    qq_protocol::GoalControl::Pause
                },
            }
        };
        let id = CommandId::generate().unwrap();
        let original = store.command(id, command.clone()).await.unwrap();
        assert!(original.cascade_cancels.contains(&child.run_id));
        assert!(original.cascade_cancels.contains(&helper.identity.run_id));
        let before = store
            .call(Priority::Control, move |connection| {
                workspace_sequence(connection, parent.identity.workspace_id)
            })
            .await
            .unwrap();
        let replay = store.command(id, command.clone()).await.unwrap();
        assert_eq!(replay.receipt, original.receipt);
        assert!(
            replay.cascade_cancels.contains(&child.run_id),
            "{mode} replay must restore the child signal after a lost caller"
        );
        assert!(
            replay.cascade_cancels.contains(&helper.identity.run_id),
            "{mode} replay must restore the helper signal"
        );
        let after = store
            .call(Priority::Control, move |connection| {
                workspace_sequence(connection, parent.identity.workspace_id)
            })
            .await
            .unwrap();
        assert_eq!(
            before, after,
            "signal restoration appends no duplicate durable events"
        );
        assert_eq!(
            store.queue_due_goals(now_ms()).await.unwrap(),
            None,
            "replacement waits for sponsored work to drain"
        );
        store
            .finish_run(
                &claimed,
                RunOutcome::Cancelled,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let replay = store.command(id, command).await.unwrap();
        assert!(
            replay.cascade_cancels.is_empty(),
            "settled obligations receive no replayed signals"
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_terminal_outcomes_pause_backoff_and_bound_failure_streaks() {
    for case in [
        "auth",
        "config",
        "failure",
        "no_progress",
        "retry",
        "wait",
        "wait_retry",
        "wait_failure",
        "wait_no_progress",
        "wait_budget",
        "short_wait_retry",
    ] {
        let (directory, store, claim) = model_goal_claim(true).await;
        let session = claim.identity.session_id;
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.counters.failure_streak =
                    if matches!(case, "wait_failure" | "wait_no_progress" | "wait_budget") {
                        0
                    } else {
                        2
                    };
                goal.counters.no_progress_streak =
                    if matches!(case, "wait_failure" | "wait_no_progress" | "wait_budget") {
                        0
                    } else {
                        1
                    };
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        let wait = if matches!(case, "wait" | "short_wait_retry") || case.starts_with("wait_") {
            store
                .update_model_goal(
                    &claim,
                    serde_json::from_value(
                        serde_json::json!({"wait":{"seconds":if case == "short_wait_retry" { 1 } else { 100 },"reason":"external dependency"}}),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            Some(
                store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?
                            .unwrap()
                            .next_run_at_ms)
                    })
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let outcome = match case {
            "auth" => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderAuthentication,
                    message: "refresh credentials".to_owned(),
                },
            },
            "config" => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Configuration,
                    message: "configure a model".to_owned(),
                },
            },
            "failure" | "wait_failure" => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Server,
                    message: "worker failed".to_owned(),
                },
            },
            "no_progress" | "wait_no_progress" => RunOutcome::Paused {
                pause: Box::new(qq_protocol::RunPause::NoProgress {
                    turn_ordinal: 1,
                    slices: 2,
                    calls: 512,
                }),
            },
            "retry" | "wait_retry" | "short_wait_retry" => RunOutcome::Paused {
                pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                    kind: RunFailureKind::ProviderUnavailable,
                    message: "outage".to_owned(),
                    turn_ordinal: 1,
                    attempts: 1,
                }),
            },
            "wait_budget" => RunOutcome::BudgetExhausted {
                exhaustion: Box::new(qq_protocol::BudgetExhaustion {
                    limit: qq_protocol::BudgetLimitKind::ModelTurns,
                    final_response: false,
                    message: "per-run cap".to_owned(),
                }),
            },
            "wait" => RunOutcome::Completed,
            _ => unreachable!(),
        };
        let events = store
            .finish_run(&claim, outcome, None, TeardownComplete::nothing_ran())
            .await
            .unwrap();
        let goal = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        match case {
            "auth" => assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Authentication {
                            message: "refresh credentials".to_owned()
                        }
                    }
                }
            ),
            "config" => assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Configuration {
                            message: "configure a model".to_owned()
                        }
                    }
                }
            ),
            "failure" => {
                assert_eq!(
                    goal.status,
                    qq_protocol::GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::RepeatedFailure
                    }
                );
                assert_eq!(goal.counters.failure_streak, 3);
            }
            "no_progress" => {
                assert_eq!(
                    goal.status,
                    qq_protocol::GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::NoProgress
                    }
                );
                assert_eq!(goal.counters.no_progress_streak, 2);
            }
            "retry" | "short_wait_retry" => {
                assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
                assert_eq!(goal.counters.failure_streak, 0);
                assert!(
                    goal.next_run_at_ms >= now_ms() + 59_000,
                    "provider outage schedules backoff without consuming the other-failure streak"
                );
            }
            "wait" | "wait_retry" | "wait_failure" | "wait_no_progress" | "wait_budget" => {
                assert_eq!(Some(goal.next_run_at_ms), wait);
                assert_eq!(
                    goal.counters.failure_streak,
                    if matches!(case, "wait_failure" | "wait_budget") {
                        1
                    } else {
                        0
                    }
                );
                assert_eq!(
                    goal.counters.no_progress_streak,
                    if case == "wait_no_progress" { 1 } else { 0 }
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            goal.counters.runs, 0,
            "terminal policy alone proves no committed main-model work"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event.event, SessionEvent::GoalUpdated { .. })),
            "outcome-only changes publish a durable goal update"
        );
        if case.starts_with("wait_") {
            assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
            assert_eq!(
                store
                    .queue_due_goals(goal.next_run_at_ms - 1)
                    .await
                    .unwrap(),
                Some(goal.next_run_at_ms)
            );
            assert!(
                store.reserve_next_run(false).await.unwrap().is_none(),
                "model wait must prevent early work"
            );
        }
        store.close().await.unwrap();
        if case == "wait_retry" {
            let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let restored = reopened
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(restored, goal, "retry wait persists exactly across reopen");
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn driver_loader_failures_pause_or_block_without_unbounded_restart() {
    struct Loader {
        kind: RunFailureKind,
        calls: Arc<AtomicUsize>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let kind = self.kind;
            Box::pin(async move {
                Err(RuntimeLoadError {
                    kind,
                    message: "scripted preparation failure".to_owned(),
                })
            })
        }
    }
    for kind in [
        RunFailureKind::Authentication,
        RunFailureKind::Configuration,
        RunFailureKind::Server,
    ] {
        let directory = TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let SessionFixture {
            directory: _directory,
            runtime,
            session_id,
            mut events,
            ..
        } = SessionFixture::open_at(
            directory,
            Arc::new(Loader {
                kind,
                calls: Arc::clone(&calls),
            }),
            ApprovalMode::default(),
        )
        .await;
        set_goal(&runtime, serde_json::json!({"session_id":session_id,
            "objective":"bounded preparation failures","budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}})).await.unwrap();
        let goal = within(async {
            while let Some(event) = events.next().await {
                if let SessionEvent::GoalUpdated { goal } = event.unwrap().event
                    && matches!(
                        goal.status,
                        qq_protocol::GoalStatus::Paused { .. }
                            | qq_protocol::GoalStatus::Blocked { .. }
                    )
                {
                    return goal;
                }
            }
            panic!("goal stream ended");
        })
        .await
        .expect("preparation failures must stop fresh-run scheduling");
        if kind == RunFailureKind::Server {
            assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Blocked {
                    reason: qq_protocol::GoalBlockedReason::RepeatedFailure
                }
            );
            assert_eq!(goal.counters.failure_streak, 3);
            assert_eq!(calls.load(Ordering::SeqCst), 3);
        } else {
            assert!(matches!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser { .. }
                }
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        assert_eq!(goal.counters.runs, 0);
        assert!(goal.pending_run_id.is_none());
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn provider_backoff_uses_durable_streak_across_reopen_and_resets_after_completion() {
    let (directory, mut store, initial) = model_goal_claim(true).await;
    let session = initial.identity.session_id;
    let mut next = Some(initial);
    for (index, delay) in [
        60_000, 120_000, 60_000, 60_000, 120_000, 240_000, 480_000, 960_000, 1_800_000, 1_800_000,
        0, 60_000,
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 || index == 5 {
            for neutral in 0..6 {
                let goal = store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                    })
                    .await
                    .unwrap();
                store.queue_due_goals(goal.next_run_at_ms).await.unwrap();
                let neutral_claim = store.reserve_next_run(false).await.unwrap().unwrap();
                store
                    .start_reserved_run(&neutral_claim, test_prepared_audit(&neutral_claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let outcome = if neutral % 2 == 0 {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                store
                    .finish_run(
                        &neutral_claim,
                        outcome,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
            store.close().await.unwrap();
            store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
        }
        let claim = if let Some(claim) = next.take() {
            claim
        } else {
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            store.queue_due_goals(goal.next_run_at_ms).await.unwrap();
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            store
                .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            claim
        };
        if index == 2 {
            let usage = TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            };
            store
                .persist_model_turn(
                    &claim,
                    TurnCommit::new(
                        1,
                        Message::assistant("committed prefix before provider outage"),
                    )
                    .usage(Some(usage))
                    .cost(Some(37))
                    .accounting(RunAccounting {
                        goal_request_rejected: false,
                        usage: Some(usage),
                        estimated_cost_usd_nanos: Some(37),
                        saw_turn: true,
                        context_tokens: None,
                        final_output: None,
                        request_basis: ContextOccupancyBasis {
                            version: 1,
                            shape: ContentHash::from_bytes([0; 32]),
                            static_prefix: PreparedStaticPrefix::new(
                                ContentHash::from_bytes([0; 32]),
                                None,
                            ),
                            request_bytes: 0,
                        },
                    })
                    .build(),
                )
                .await
                .unwrap();
        }
        let outcome = if delay == 0 {
            RunOutcome::Completed
        } else {
            RunOutcome::Paused {
                pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                    kind: RunFailureKind::ProviderUnavailable,
                    message: "outage".to_owned(),
                    turn_ordinal: 1,
                    attempts: 1,
                }),
            }
        };
        store
            .finish_run(&claim, outcome, None, TeardownComplete::nothing_ran())
            .await
            .unwrap();
        let run = claim.identity.run_id;
        let (finished, goal) = store
            .call(Priority::Control, move |connection| {
                let finished: u64 = connection.query_row(
                    "SELECT finished_at_ms FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get(0),
                )?;
                Ok((
                    finished,
                    crate::sessions::goals::load_live(connection, session)?.unwrap(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            goal.next_run_at_ms - finished,
            delay,
            "backoff step {index}"
        );
        assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
        assert_eq!(goal.counters.failure_streak, 0);
        assert_eq!(goal.counters.runs, u32::from(index >= 2));
        if index == 2 {
            store.close().await.unwrap();
            store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let reopened = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(reopened, goal);
        }
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_failure_and_no_progress_streaks_require_consecutive_matching_outcomes() {
    for sequence in [
        &["failure", "no_progress", "failure", "failure"][..],
        &["no_progress", "retry", "no_progress"][..],
    ] {
        let (_directory, store, initial) = model_goal_claim(true).await;
        let session = initial.identity.session_id;
        let mut next = Some(initial);
        let mut failures = 0;
        let mut quiet = 0;
        for case in sequence {
            let claim = if let Some(claim) = next.take() {
                claim
            } else {
                let goal = store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                    })
                    .await
                    .unwrap();
                store.queue_due_goals(goal.next_run_at_ms).await.unwrap();
                let claim = store.reserve_next_run(false).await.unwrap().unwrap();
                store
                    .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                claim
            };
            let outcome = match *case {
                "failure" => RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::Server,
                        message: "scripted failure".to_owned(),
                    },
                },
                "no_progress" => RunOutcome::Paused {
                    pause: Box::new(qq_protocol::RunPause::NoProgress {
                        turn_ordinal: 1,
                        slices: 2,
                        calls: 512,
                    }),
                },
                "retry" => RunOutcome::Paused {
                    pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                        kind: RunFailureKind::ProviderUnavailable,
                        message: "outage".to_owned(),
                        turn_ordinal: 1,
                        attempts: 1,
                    }),
                },
                _ => unreachable!(),
            };
            failures = if *case == "failure" { failures + 1 } else { 0 };
            quiet = if *case == "no_progress" { quiet + 1 } else { 0 };
            store
                .finish_run(&claim, outcome, None, TeardownComplete::nothing_ran())
                .await
                .unwrap();
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Active,
                "sequence {sequence:?}"
            );
            assert_eq!(goal.counters.failure_streak, failures);
            assert_eq!(goal.counters.no_progress_streak, quiet);
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn paused_goal_fences_preparing_user_prompt_until_cancelled() {
    let (_directory, store, previous) = model_goal_claim(false).await;
    let session_id = previous.identity.session_id;
    store
        .finish_run(
            &previous,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store_command(&store, session_id, "next work")
        .await
        .unwrap();
    let claim = store.reserve_next_run(false).await.unwrap().unwrap();
    let captured = claim.goal.as_ref().unwrap();
    let pause = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: captured.goal_id,
                    revision: captured.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = pause.receipt.outcome else {
        panic!("paused goal")
    };
    let run_id = claim.identity.run_id;
    let cancelled: bool = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT cancel_requested FROM runs WHERE id=?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    let resume = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::Resume,
            },
        )
        .await;
    store
        .finish_reserved_run(&claim, RunOutcome::Cancelled)
        .await
        .unwrap();
    if resume.is_err() {
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::Resume,
                },
            )
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    assert!(
        cancelled,
        "pause must cancel a preparing prompt with captured goal ownership"
    );
    assert!(
        matches!(resume, Err(SessionRuntimeError::GoalCancellationPending)),
        "resume must wait for the old captured revision to drain"
    );
}

#[tokio::test]
async fn goal_check_edits_reset_only_the_changed_check_failure_streak() {
    for command in [Some("true"), Some("false"), None] {
        let (directory, runtime) = test_runtime().await;
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            create_session_with_mode(&runtime, workspace, None, ApprovalMode::Ask)
                .await
                .outcome
        else {
            panic!("session");
        };
        let receipt = set_goal(&runtime, serde_json::json!({"session_id":session_id, "objective":"checked work", "check":{"command":"true"},
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000}
        })).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal");
        };
        runtime
            .store()
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.counters.check_failures = 2;
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        let edited = set_goal(&runtime, serde_json::json!({"session_id":session_id, "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
            "objective":"edited objective", "check":command.map(|command|serde_json::json!({"command":command})),
            "budget":goal.budget
        })).await.unwrap();
        let CommandOutcome::GoalUpdated { goal: edited, .. } = edited.outcome else {
            panic!("edit");
        };
        assert_eq!(
            edited.counters.check_failures,
            if command == Some("true") { 2 } else { 0 }
        );
        assert_eq!(edited.counters.runs, goal.counters.runs);
        assert_eq!(edited.counters.audits, goal.counters.audits);
        assert_eq!(
            edited.check_authorization,
            command.map(|_| qq_protocol::GoalCheckAuthorization::Pending)
        );
        runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_check_edits_preserve_unrelated_stop_states() {
    use qq_protocol::{
        GoalBlockedReason, GoalCheckAuthorization, GoalNeedsUser, GoalPauseReason, GoalStatus,
    };
    let approval_pause = GoalStatus::Paused {
        reason: GoalPauseReason::NeedsUser {
            cause: GoalNeedsUser::CheckApproval,
        },
    };
    for remove in [false, true] {
        for status in [
            approval_pause.clone(),
            GoalStatus::Active,
            GoalStatus::Paused {
                reason: GoalPauseReason::User,
            },
            GoalStatus::Blocked {
                reason: GoalBlockedReason::RepeatedFailure,
            },
        ] {
            let (directory, runtime) = test_runtime().await;
            let (_, session_id, _) =
                new_session(&runtime, directory.path(), ApprovalMode::Ask).await;
            let receipt = set_goal(&runtime, serde_json::json!({"session_id":session_id,"objective":"checked work","check":{"command":"true"},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000}
            })).await.unwrap();
            let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
                panic!("goal")
            };
            let setup_status = status.clone();
            runtime
                .store()
                .call(Priority::Control, move |connection| {
                    let mut goal =
                        crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                    goal.status = setup_status;
                    goal.counters.failure_streak = 3;
                    goal.counters.check_failures = 2;
                    crate::sessions::goals::save(connection, &goal)
                })
                .await
                .unwrap();
            let command = if remove {
                serde_json::json!({
                    "type":"set_goal","session_id":session_id,"expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "objective":"checked work","budget":goal.budget
                })
            } else {
                serde_json::json!({
                    "type":"goal_control","session_id":session_id,"expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "action":{"type":"deny_check"}
                })
            };
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    serde_json::from_value(command).unwrap(),
                )
                .await
                .unwrap();
            let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
                panic!("updated goal")
            };
            let expected = if remove && status == approval_pause {
                GoalStatus::Paused {
                    reason: GoalPauseReason::User,
                }
            } else if !remove && status == GoalStatus::Active {
                approval_pause.clone()
            } else {
                status.clone()
            };
            assert_eq!(
                goal.status, expected,
                "remove={remove}, original={status:?}"
            );
            assert_eq!(goal.counters.failure_streak, 3);
            assert_eq!(goal.counters.check_failures, if remove { 0 } else { 2 });
            assert_eq!(
                goal.check_authorization,
                if remove {
                    None
                } else {
                    Some(GoalCheckAuthorization::Pending)
                }
            );
            if !remove {
                let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                    "type":"goal_control","session_id":session_id,"expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "action":{"type":"approve_check","scope":"once","command":"true"}
                })).unwrap()).await.unwrap();
                let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
                    panic!("approved goal")
                };
                assert_eq!(
                    goal.status,
                    if expected == approval_pause {
                        GoalStatus::Active
                    } else {
                        expected
                    }
                );
                assert_eq!(
                    goal.counters.failure_streak, 3,
                    "approval cannot apply Resume's streak resets"
                );
            }
            runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn recovered_user_prompt_survives_an_active_goal_edit() {
    let (directory, store, previous) = model_goal_claim(false).await;
    let session = previous.identity.session_id;
    store
        .finish_run(
            &previous,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store_command(&store, session, "accepted user work")
        .await
        .unwrap();
    let reserved = store.reserve_next_run(false).await.unwrap().unwrap();
    let captured = reserved.goal.clone().unwrap();
    store.close().await.unwrap();
    let reopened = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    reopened.recover_interrupted_runs().await.unwrap();
    reopened
        .command(
            CommandId::generate().unwrap(),
            goal_command(
                serde_json::json!({"session_id":session,"objective":"edited objective",
                    "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }),
            ),
        )
        .await
        .unwrap();
    let reclaimed = reopened
        .reserve_next_run(false)
        .await
        .unwrap()
        .expect("accepted user prompt remains queued after a normal edit");
    assert_eq!(reclaimed.identity.run_id, reserved.identity.run_id);
    assert_eq!(
        reclaimed.goal.as_deref(),
        Some(captured.as_ref()),
        "recovery preserves the frozen claim"
    );
    reopened
        .finish_reserved_run(&reclaimed, RunOutcome::Cancelled)
        .await
        .unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn goal_adoption_ignores_owned_compaction_but_detects_preempting_prompts() {
    for goal_origin in [false, true] {
        let (_directory, store, owner) = model_goal_claim(goal_origin).await;
        let goal = owner.goal.as_ref().unwrap();
        let audit = test_prepared_audit(&owner);
        let (helper, _) = store
            .start_in_run_compaction(&owner, &audit.resolved_model, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .has_preempting_goal_work(owner.identity.session_id, goal.goal_id)
                .await
                .unwrap(),
            !goal_origin,
            "owned summary must not turn a goal prompt into preempting work"
        );
        store
            .finish_in_run_compaction_failed(&helper, RunOutcome::Cancelled)
            .await
            .unwrap();
        store
            .finish_run(
                &owner,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_due_goal_waits_for_a_workspace_restore_and_blocks_starting_one() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    let workspace_id = parent.identity.workspace_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .begin_workspace_restore(
                CommandId::generate().unwrap(),
                SessionCommand::UndoWorkspace {
                    workspace_id,
                    snapshot: None,
                },
            )
            .await,
        Err(SessionRuntimeError::WorkspaceBusy)
    ));
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO temp.restoring_paths(path, command_id, request_json)
                 SELECT path, ?2, '{}' FROM workspaces WHERE id = ?1",
                [
                    workspace_id.to_string(),
                    CommandId::generate().unwrap().to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store.queue_due_goals(now_ms()).await.unwrap();
    let pending = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?
                .unwrap()
                .pending_run_id)
        })
        .await
        .unwrap();
    assert_eq!(pending, None);
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    store.close().await.unwrap();
}
