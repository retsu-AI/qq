use super::*;

#[tokio::test]
async fn goal_paid_requests_intersect_the_callers_cumulative_remainder() {
    struct Model {
        kind: &'static str,
        calls: Arc<StdMutex<Vec<u32>>>,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let first = {
                let mut calls = self.calls.lock().unwrap();
                let first = calls.is_empty();
                if (!first && self.kind == "turns") || self.kind == "tools" {
                    assert_eq!(request.tool_choice(), qq_provider::ToolChoice::None);
                }
                calls.push(request.max_output_tokens());
                first
            };
            let usage = qq_provider::ProviderUsage {
                input_tokens: if first && self.kind != "output" {
                    90_000
                } else {
                    1
                },
                output_tokens: if first { 700 } else { 1 },
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: None,
            };
            let mut events = if first && self.kind != "tools" {
                vec![
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "read".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "read".to_owned(),
                        json: r#"{"path":"note"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "read".to_owned(),
                    }),
                ]
            } else {
                vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                })]
            };
            events.push(Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(usage),
            }));
            Box::pin(stream::iter(events))
        }
    }
    struct Loader {
        kind: &'static str,
        calls: Arc<StdMutex<Vec<u32>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let calls = Arc::clone(&self.calls);
            let kind = self.kind;
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model { kind, calls }, "test-model", 16_000).unwrap(),
                    &request.workspace,
                    Some(ModelPricing {
                        input_usd_nanos_per_token: 1,
                        output_usd_nanos_per_token: 1,
                        cache_read_usd_nanos_per_token: Some(1),
                        cache_write_usd_nanos_per_token: Some(1),
                        context_tier: None,
                        provenance: "deterministic caller-bound fixture".to_owned(),
                    }),
                ))
            })
        }
    }
    for goal_origin in [false, true] {
        for kind in ["output", "total", "input", "cost", "turns", "tools"] {
            let directory = TempDir::new().unwrap();
            std::fs::write(directory.path().join("note"), "bounded read").unwrap();
            let calls = Arc::new(StdMutex::new(Vec::new()));
            let runtime = SessionRuntime::open(
                SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
                Arc::new(Loader {
                    kind,
                    calls: Arc::clone(&calls),
                }),
            )
            .await
            .unwrap();
            let held = Arc::clone(&runtime.inner.permits)
                .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
                .await
                .unwrap();
            let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
            let created =
                create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
            let CommandOutcome::SessionCreated {
                session_id: session,
            } = created.outcome
            else {
                panic!("session")
            };
            let mut events = runtime
                .subscribe(SubscribeRequest {
                    workspace_id: workspace,
                    after: created.committed_through,
                })
                .unwrap();
            let limits = RunLimits {
                max_model_turns: (kind == "turns").then_some(2),
                max_tool_calls: (kind == "tools").then_some(1),
                max_output_tokens: (kind == "output").then_some(1000),
                max_total_tokens: (kind == "total").then_some(100_000),
                max_input_tokens: (kind == "input").then_some(100_000),
                max_cost_usd_nanos: (kind == "cost").then_some(100_000),
                ..RunLimits::default()
            };
            runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session,"objective":"respect the stricter per-run allowance",
                "budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"max_total_tokens":1_000_000,
                    "per_run":if goal_origin {limits} else {RunLimits::default()}}
            })).unwrap()).await.unwrap();
            let run = if goal_origin {
                runtime.inner.store.drive_goals(now_ms()).await.unwrap();
                runtime
                    .inner
                    .store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?
                            .unwrap()
                            .pending_run_id
                            .unwrap())
                    })
                    .await
                    .unwrap()
            } else {
                let receipt = runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::SubmitPrompt {
                            session_id: session,
                            input: vec![InputPart::text("work")],
                            limits,
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
            };
            drop(held);
            runtime.inner.schedule.send(()).await.unwrap();
            let observed = collect_until_run_finished(&mut events, run).await;
            runtime.shutdown().await.unwrap();
            let calls = calls.lock().unwrap().clone();
            if matches!(kind, "turns" | "tools") {
                assert_eq!(
                    calls.len(),
                    if kind == "turns" { 2 } else { 1 },
                    "the reserved tool-free final reply remains affordable: kind={kind}, outcome={:?}",
                    finished_outcome(&observed, run)
                );
                let expected = if kind == "turns" {
                    qq_protocol::BudgetLimitKind::ModelTurns
                } else {
                    qq_protocol::BudgetLimitKind::ToolCalls
                };
                assert!(
                    matches!(finished_outcome(&observed,run),Some(RunOutcome::BudgetExhausted {exhaustion}) if exhaustion.limit==expected && exhaustion.final_response)
                );
            } else if kind == "output" {
                assert_eq!(
                    calls,
                    vec![1000, 300],
                    "goal_origin={goal_origin}: each request must use the caller's remaining output allowance"
                );
                assert_eq!(
                    finished_outcome(&observed, run),
                    Some(RunOutcome::Completed)
                );
            } else {
                assert_eq!(
                    calls.len(),
                    1,
                    "goal_origin={goal_origin} kind={kind}: the next input cannot fit the caller's remaining allowance"
                );
                let expected = match kind {
                    "total" => qq_protocol::BudgetLimitKind::TotalTokens,
                    "input" => qq_protocol::BudgetLimitKind::InputTokens,
                    "cost" => qq_protocol::BudgetLimitKind::Cost,
                    _ => unreachable!(),
                };
                assert!(
                    matches!(finished_outcome(&observed,run),Some(RunOutcome::BudgetExhausted {exhaustion}) if exhaustion.limit==expected)
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn goal_paid_admission_rechecks_caller_deadline_after_store_queue_wait() {
    let keep_clock = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for expires in [false, true] {
        let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
        let paid = crate::sessions::goal_paid::GoalPaidInput::new(&store, &claim, None).unwrap();
        let caller = crate::runtime::ChildBudget {
            limits: RunLimits::default(),
            deadline: Some(tokio::time::Instant::now() + Duration::from_secs(1)),
        };
        assert!(matches!(
            paid.preview_request(100, 1, caller).await.unwrap(),
            crate::sessions::goal_paid::PaidRequestAdmission::Reprepare {
                max_output_tokens: 100
            }
        ));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let blocked_store = store.clone();
        let blocker = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        let admission = tokio::spawn(async move { paid.admit_request(100, 1, caller).await });
        tokio::task::yield_now().await;
        if expires {
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        let result = admission.await.unwrap().unwrap();
        if expires {
            assert!(
                matches!(result, crate::sessions::goal_paid::PaidRequestAdmission::Exhausted(exhaustion)
                if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
            );
        } else {
            assert!(matches!(
                result,
                crate::sessions::goal_paid::PaidRequestAdmission::Allowed { .. }
            ));
        }
        let run = claim.identity.run_id;
        let pending = store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_request_pending FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            pending, !expires,
            "expired admission cannot persist provider-dispatch proof"
        );
        store.close().await.unwrap();
    }
    keep_clock.abort();
}

#[tokio::test]
async fn goal_paid_admission_reserves_worst_cache_and_tier_cost_before_dispatch_proof() {
    for (remaining, expected) in [(100, Some(3)), (70, None), (69, None)] {
        let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
        let pricing = ModelPricing {
            input_usd_nanos_per_token: 1,
            output_usd_nanos_per_token: 2,
            cache_read_usd_nanos_per_token: Some(3),
            cache_write_usd_nanos_per_token: Some(4),
            context_tier: Some(qq_protocol::ModelPricingTier {
                above_input_tokens: 1000,
                input_usd_nanos_per_token: 5,
                output_usd_nanos_per_token: 9,
                cache_read_usd_nanos_per_token: Some(6),
                cache_write_usd_nanos_per_token: Some(7),
            }),
            provenance: "deterministic worst-rate bound".to_owned(),
        };
        let paid =
            crate::sessions::goal_paid::GoalPaidInput::new(&store, &claim, Some(&pricing)).unwrap();
        let caller = crate::runtime::ChildBudget {
            limits: RunLimits {
                max_cost_usd_nanos: Some(remaining),
                ..RunLimits::default()
            },
            deadline: None,
        };
        let preview = paid.preview_request(100, 10, caller).await.unwrap();
        let proposed = if let Some(cap) = expected {
            assert!(
                matches!(preview,crate::sessions::goal_paid::PaidRequestAdmission::Reprepare {max_output_tokens} if max_output_tokens==cap)
            );
            cap
        } else {
            assert!(
                matches!(preview,crate::sessions::goal_paid::PaidRequestAdmission::Exhausted(exhaustion)
                if exhaustion.limit==qq_protocol::BudgetLimitKind::Cost)
            );
            100
        };
        let admitted = paid.admit_request(proposed, 10, caller).await.unwrap();
        assert_eq!(
            matches!(
                admitted,
                crate::sessions::goal_paid::PaidRequestAdmission::Allowed { .. }
            ),
            expected.is_some()
        );
        let run = claim.identity.run_id;
        let pending = store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_request_pending FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(pending, expected.is_some());
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_paid_admission_refuses_unknown_spend_before_pending_proof() {
    for tokens_unknown in [false, true] {
        let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
        let session = claim.identity.session_id;
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                if tokens_unknown {
                    goal.budget.max_total_tokens = Some(100);
                    goal.counters.usage = None;
                } else {
                    goal.budget.max_cost_usd_nanos = Some(100);
                    goal.counters.estimated_cost_usd_nanos = None;
                }
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        let paid = crate::sessions::goal_paid::GoalPaidInput::new(&store, &claim, None).unwrap();
        let expected = if tokens_unknown {
            qq_protocol::BudgetLimitKind::TokensUnknown
        } else {
            qq_protocol::BudgetLimitKind::CostUnknown
        };
        for result in [
            paid.preview_request(100, 1, crate::runtime::ChildBudget::default())
                .await
                .unwrap(),
            paid.admit_request(100, 1, crate::runtime::ChildBudget::default())
                .await
                .unwrap(),
        ] {
            assert!(
                matches!(result,crate::sessions::goal_paid::PaidRequestAdmission::Exhausted(exhaustion)
                if exhaustion.limit==expected)
            );
        }
        let run = claim.identity.run_id;
        let pending = store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_request_pending FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert!(!pending);
        store.close().await.unwrap();
    }
}
