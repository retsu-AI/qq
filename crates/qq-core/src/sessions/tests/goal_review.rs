use super::*;


#[tokio::test]
async fn managed_goal_prompt_limits_are_clamped_at_claim_and_start() {
    for explicit in [false, true] {
        let (_directory, store, initial) = super::goals::model_goal_claim(true).await;
        let session = initial.identity.session_id;
        store.finish_run(&initial, RunOutcome::Completed, None, TeardownComplete::nothing_ran()).await.unwrap();
        let caps = RunLimits {
            max_duration_ms: Some(30_000), max_model_turns: Some(3), max_tool_calls: Some(4),
            max_total_tokens: Some(50_000), max_cost_usd_nanos: Some(60_000),
            max_input_tokens: Some(40_000), max_output_tokens: Some(10_000),
            max_tool_output_bytes: Some(2048), max_children: Some(2), max_concurrent_children: Some(1),
        };
        store.call(Priority::Control, move |connection| {
            let goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            let (_, at) = goal_policy::load(connection, goal.goal_id)?;
            goal_policy::save(connection, goal.goal_id, GoalPolicy { per_run: caps, ..GoalPolicy::default() }, at)
        }).await.unwrap();
        let mut requested = RunLimits::default();
        if explicit {
            requested = caps;
            requested.max_model_turns = Some(100);
            requested.max_output_tokens = Some(2); // A stricter caller limit survives.
        }
        store.command(CommandId::generate().unwrap(), SessionCommand::SubmitPrompt {
            session_id: session, input: vec![InputPart::text("manual goal work")], limits: requested,
            correlation: Correlation::default(), output: None,
        }).await.unwrap();
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        let mut expected = caps;
        if explicit { expected.max_output_tokens = Some(2); }
        assert_eq!(claim.limits, expected, "explicit={explicit}");
        store.finish_reserved_run(&claim, RunOutcome::Cancelled).await.unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn neutral_goal_stops_preserve_retry_streaks() {
    for outcome in [RunOutcome::Interrupted, RunOutcome::Cancelled] {
        let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
        let session = claim.identity.session_id;
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.counters.failure_streak = 2;
                goal.counters.no_progress_streak = 1;
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        store
            .finish_run(
                &claim,
                outcome.clone(),
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
        store.close().await.unwrap();
        assert_eq!(
            (
                goal.counters.failure_streak,
                goal.counters.no_progress_streak
            ),
            (2, 1),
            "neutral outcome={outcome:?}"
        );
    }
}

#[tokio::test]
async fn goal_scheduler_waits_for_uncancelled_sponsored_children() {
    for running in [false, true] {
        let (_directory, store, parent) = super::goals::model_goal_claim(true).await;
        let session = parent.identity.session_id;
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "sponsored work".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        let child_claim = if running {
            Some(store.claim_next_run(true).await.unwrap().unwrap())
        } else {
            None
        };
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let queued = store.queue_due_goals(now_ms()).await.unwrap();
        let pending = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?
                    .unwrap()
                    .pending_run_id)
            })
            .await
            .unwrap();
        assert!(
            queued.is_none() && pending.is_none(),
            "running={running}: do not reuse allowance while sponsored work is unsettled"
        );
        let receipt = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![InputPart::text(
                        "fresh prompt must share the remaining allowance",
                    )],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id: prompt, .. } = receipt.receipt.outcome else {
            panic!("prompt")
        };
        assert!(
            store.reserve_next_run(false).await.unwrap().is_none(),
            "a user prompt cannot bypass the sponsored-spending fence"
        );
        let child_claim = match child_claim {
            Some(claim) => claim,
            None => store.claim_next_run(true).await.unwrap().unwrap(),
        };
        assert_eq!(child_claim.identity.run_id, child.run_id);
        store
            .finish_run(
                &child_claim,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.queue_due_goals(now_ms()).await.unwrap();
        let next = store
            .reserve_next_run(false)
            .await
            .unwrap()
            .expect("settled child releases the scheduling fence");
        assert_eq!(next.identity.session_id, session);
        assert_eq!(next.identity.run_id, prompt);
        assert!(matches!(
            next.origin,
            Some(qq_protocol::RunOrigin::UserPrompt {})
        ));
        store
            .finish_reserved_run(&next, RunOutcome::Cancelled)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn replayed_sponsored_child_cancel_wakes_the_goal_scheduler() {
    let (_directory, store, parent) = super::goals::model_goal_claim(true).await;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "queued sponsored work".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
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
    assert!(store.queue_due_goals(now_ms()).await.unwrap().is_none());
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::CancelRun {
        run_id: child.run_id,
    };
    // Persist cancellation without the command waiter's post-commit schedule signal.
    let first = store.command(command_id, command.clone()).await.unwrap();
    let replay = store.command(command_id, command).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first.receipt);
    assert!(
        replay.schedule,
        "a queued child's cancellation has no executor to wake scheduling"
    );
    store.queue_due_goals(now_ms()).await.unwrap();
    let next = store
        .reserve_next_run(false)
        .await
        .unwrap()
        .expect("replayed wake releases the goal");
    assert_eq!(next.identity.session_id, parent.identity.session_id);
    assert!(matches!(
        next.origin,
        Some(qq_protocol::RunOrigin::Goal { .. })
    ));
    store
        .finish_reserved_run(&next, RunOutcome::Cancelled)
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_run_start_seeds_zero_before_dispatch_and_preserves_paid_routing_uncertainty() {
    for routing in ["none", "measured", "unknown"] {
        let (directory, store, initial) = super::goals::model_goal_claim(false).await;
        let session = initial.identity.session_id;
        store
            .finish_run(
                &initial,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.call(Priority::Control,move |connection| {
            connection.execute("UPDATE runs SET usage_json=?2,estimated_cost_usd_nanos=0 WHERE session_id=?1 AND outcome_json IS NOT NULL",params![session.to_string(),serde_json::to_string(&TokenUsage::default())?])?;
            Ok(())
        }).await.unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![InputPart::text("start without dispatch")],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        let run = claim.identity.run_id;
        let expected = match routing {
            "measured" => Some(TokenUsage {
                input_tokens: 7,
                ..Default::default()
            }),
            "unknown" => None,
            _ => Some(TokenUsage::default()),
        };
        let cost = match routing {
            "measured" => Some(9),
            "unknown" => None,
            _ => Some(0),
        };
        if routing != "none" {
            store
                .record_routing_started(&claim, true, None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            store
                .record_routing_completed(
                    &claim,
                    qq_protocol::RoutingDecision {
                        model: claim.model.clone(),
                        reasoning_effort: None,
                        outcome: qq_protocol::RoutingOutcome::Fallback,
                        reason: "deterministic routing prefix".to_owned(),
                        usage: expected,
                        estimated_cost_usd_nanos: cost,
                    },
                )
                .await
                .unwrap();
        }
        store
            .start_reserved_run(&claim, test_prepared_audit(&claim), None)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let actual = store
            .call(Priority::Control, move |connection| {
                let (usage, cost): (Option<String>, Option<u64>) = connection.query_row(
                    "SELECT usage_json,estimated_cost_usd_nanos FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok((
                    usage
                        .map(|usage| serde_json::from_str::<TokenUsage>(&usage))
                        .transpose()?,
                    cost,
                ))
            })
            .await
            .unwrap();
        if routing != "none" {
            store
                .finish_run(
                    &claim,
                    RunOutcome::Interrupted,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            store.close().await.unwrap();
            assert_eq!(actual, (expected, cost), "routing={routing}");
            continue;
        }
        store.close().await.unwrap();
        assert_eq!(
            actual,
            (expected, cost),
            "route-less start must atomically establish known zero"
        );
        let reopened = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        reopened.recover_interrupted_runs().await.unwrap();
        let accounting = reopened
            .call(Priority::Control, move |connection| {
                load_session_accounting(connection, session)
            })
            .await
            .unwrap();
        let zero = qq_protocol::AccountingTotal {
            usage: Some(TokenUsage::default()),
            estimated_cost_usd_nanos: Some(0),
        };
        assert_eq!(accounting.direct, zero);
        assert_eq!(accounting.inclusive, zero);
        assert!(
            reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn sponsored_children_recheck_absolute_deadlines_at_paid_boundaries() {
    for (boundary, own_goal) in [
        ("claim", false),
        ("routing", false),
        ("start", false),
        ("claim", true),
        ("routing", true),
        ("start", true),
    ] {
        let (_directory, store, parent) = super::goals::model_goal_claim(true).await;
        let session = parent.identity.session_id;
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "delayed sponsored work".to_owned(),
                    limits: RunLimits {
                        max_duration_ms: Some(60_000),
                        ..RunLimits::default()
                    },
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        if own_goal {
            store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({"type":"set_goal","session_id":child.session_id,"objective":"independent child goal","budget":{"max_goal_runs":10,"deadline_ms":now_ms()+120_000}})).unwrap()).await.unwrap();
        }
        let early_claim = if boundary == "claim" {
            None
        } else {
            Some(store.reserve_next_run(true).await.unwrap().unwrap())
        };
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.budget.deadline_ms = now_ms().saturating_sub(1);
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        let claim = match early_claim {
            Some(claim) => claim,
            None => store.reserve_next_run(true).await.unwrap().unwrap(),
        };
        assert_eq!(claim.identity.run_id, child.run_id);
        assert_eq!(claim.goal.is_some(), own_goal);
        if !own_goal {
            assert!(claim.goal_initial_notice.is_none());
        }
        assert_eq!(
            claim.goal_spend_owner,
            parent.goal.as_ref().map(|goal| goal.goal_id)
        );
        match boundary {
            "claim" => assert!(
                matches!(
                    claim.goal_admission,
                    Some(crate::sessions::goals::GoalAdmission::Exhausted(
                        qq_protocol::GoalExhaustion::Deadline
                    ))
                ),
                "queued sponsor deadline must not restart"
            ),
            "routing" => assert!(
                matches!(
                    store
                        .record_routing_started(&claim, true, Some(0))
                        .await
                        .unwrap(),
                    Err(claim::GoalStartRefusal::Exhausted(
                        qq_protocol::GoalExhaustion::Deadline
                    ))
                ),
                "loading cannot extend the sponsor deadline before routing"
            ),
            "start" => assert!(
                matches!(
                    store
                        .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                        .await
                        .unwrap(),
                    Err(claim::GoalStartRefusal::Exhausted(
                        qq_protocol::GoalExhaustion::Deadline
                    ))
                ),
                "loading cannot extend the sponsor deadline before main work"
            ),
            _ => unreachable!(),
        }
        let untouched = store.call(Priority::Control, move |connection| {
            Ok(connection.query_row("SELECT routing_json IS NULL AND started_at_ms IS NULL AND goal_helper_admitted=0 AND goal_request_pending=0 FROM runs WHERE id=?1", [child.run_id.to_string()], |row| row.get::<_,bool>(0))?)
        }).await.unwrap();
        assert!(untouched);
        store
            .finish_reserved_run(&claim, RunOutcome::Cancelled)
            .await
            .unwrap();
        store
            .finish_run(
                &parent,
                RunOutcome::Cancelled,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn automatic_goal_routing_uses_objective_and_manual_routing_uses_prompt() {
    struct Router(Arc<StdMutex<Vec<String>>>);
    impl TaskRouter for Router {
        fn identity(&self) -> &'static str {
            "fixture/goal-objective"
        }
        fn configuration_identity(&self) -> &str {
            self.identity()
        }
        fn max_cost_usd_nanos(&self) -> Option<u64> {
            Some(0)
        }
        fn route(&self, task: String) -> TaskRoutingFuture {
            self.0.lock().unwrap().push(task);
            Box::pin(async {
                qq_protocol::RoutingDecision {
                    model: ModelSelection::default(),
                    reasoning_effort: None,
                    outcome: qq_protocol::RoutingOutcome::Fallback,
                    reason: "retain model".to_owned(),
                    usage: Some(usage(0, 0)),
                    estimated_cost_usd_nanos: Some(0),
                }
            })
        }
    }
    struct Loader(Arc<StdMutex<Vec<String>>>);
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let tasks = Arc::clone(&self.0);
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(StaticTextProvider, "test-model", 256)
                        .unwrap()
                        .with_task_router(Arc::new(Router(tasks))),
                    &request.workspace,
                    Some(budget_pricing()),
                ))
            })
        }
    }
    for (objective, manual) in [
        ("implement the Rust parser", false),
        ("write the release announcement", false),
        ("implement the Rust parser", true),
    ] {
        let directory = TempDir::new().unwrap();
        let tasks = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader(Arc::clone(&tasks))),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({"type":"set_goal","session_id":session_id,"objective":objective,"budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000}})).unwrap()).await.unwrap();
        runtime.inner.store.drive_goals(now_ms()).await.unwrap();
        let run = if manual {
            queue_prompt(
                &runtime,
                session_id,
                "explain the release schedule".to_owned(),
            )
            .await
        } else {
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session_id)?
                        .unwrap()
                        .pending_run_id
                        .unwrap())
                })
                .await
                .unwrap()
        };
        drop(held);
        runtime.inner.schedule.send(()).await.unwrap();
        collect_until_run_finished(&mut events, run).await;
        runtime.shutdown().await.unwrap();
        let tasks = tasks.lock().unwrap();
        assert!(!tasks.is_empty());
        assert_eq!(
            tasks[0].trim(),
            if manual {
                "explain the release schedule"
            } else {
                objective
            }
        );
        assert!(tasks.iter().all(|task| task.len() <= 16 * 1024));
    }
}

#[tokio::test]
async fn sponsored_child_requests_respect_positive_inherited_token_allowance() {
    struct Child(Arc<StdMutex<Vec<u32>>>);
    impl Provider for Child {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.0.lock().unwrap().push(request.max_output_tokens());
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(provider_usage_of(usage(1, 1))),
            })]))
        }
    }
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(MultiSpawnProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            spawns: 1,
            arguments: |_| r#"{"task":"research","model":"test/child"}"#.to_owned(),
            turn: StdMutex::new(0),
        }),
        usage: Some(usage(30, 10)),
    });
    let mut harness = child_budget_harness_with_window(
        parent,
        Arc::new(Child(Arc::clone(&requests))),
        false,
        crate::runtime::AuditMode::Off,
        Some(8192),
    )
    .await;
    let held = Arc::clone(&harness.runtime.inner.permits)
        .acquire_many_owned(harness.runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    harness.runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({"type":"set_goal","session_id":harness.session_id,"objective":"delegate bounded research","budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"max_total_tokens":100_000}})).unwrap()).await.unwrap();
    let run = submit_child_budget_prompt(
        &harness,
        RunLimits {
            max_output_tokens: Some(70),
            ..RunLimits::default()
        },
    )
    .await;
    drop(held);
    harness.runtime.inner.schedule.send(()).await.unwrap();
    let events = collect_until_run_finished(&mut harness.events, run).await;
    harness.runtime.shutdown().await.unwrap();
    let child = events
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(run)
            }
            _ => None,
        })
        .expect("positive remainder admits child");
    assert_eq!(
        child.limits.as_ref().unwrap().max_total_tokens,
        Some(99_960)
    );
    assert_eq!(child.limits.as_ref().unwrap().max_output_tokens, Some(60));
    assert_eq!(
        *requests.lock().unwrap(),
        vec![60],
        "provider request must honor the child's inherited allowance"
    );
}
