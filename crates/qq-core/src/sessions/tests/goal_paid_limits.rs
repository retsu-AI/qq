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
            let mut events = if first {
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
        for kind in ["output", "total", "input", "cost"] {
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
            if kind == "output" {
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
