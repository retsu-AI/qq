use super::*;

#[tokio::test]
async fn goal_tenth_run_reserves_a_durable_tool_free_handoff_after_early_completion() {
    struct Model {
        ordinal: usize,
        prompt_final: bool,
        scenario: &'static str,
        admission: Arc<(tokio::sync::Notify, tokio::sync::Notify)>,
        requests: Arc<StdMutex<Vec<Vec<ModelRequest>>>>,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let (first, handoff, attempt) = {
                let mut requests = self.requests.lock().unwrap();
                let first = requests[self.ordinal].is_empty();
                let handoff = request.tool_choice() == qq_provider::ToolChoice::None;
                requests[self.ordinal].push(request);
                (first, handoff, requests[self.ordinal].len())
            };
            let mut events = if self.ordinal == 9
                && handoff
                && ((self.scenario == "tools" && attempt == 2)
                    || (self.scenario == "tighten" && attempt >= 2))
            {
                vec![
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "durable final-slot handoff".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "forbidden-update".to_owned(),
                        name: "update_goal".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "forbidden-update".to_owned(),
                        json: r#"{"notes":"handoff tool unexpectedly executed"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "forbidden-update".to_owned(),
                    }),
                ]
            } else if self.prompt_final && self.ordinal == 8 && first {
                vec![
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "wait".to_owned(),
                        name: "update_goal".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "wait".to_owned(),
                        json: r#"{"wait":{"seconds":60,"reason":"await final user input"}}"#
                            .to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "wait".to_owned(),
                    }),
                ]
            } else {
                vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: if handoff {
                        "durable final-slot handoff"
                    } else {
                        "ordinary early completion"
                    }
                    .to_owned(),
                })]
            };
            events.push(Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
            }));
            let held = self.ordinal == 9 && first && self.scenario == "tighten";
            let admission = Arc::clone(&self.admission);
            Box::pin(async_stream::stream! {
                if held {
                    admission.0.notify_one();
                    admission.1.notified().await;
                }
                for event in events { yield event; }
            })
        }
    }
    struct Loader {
        prompt_final: bool,
        scenario: &'static str,
        admission: Arc<(tokio::sync::Notify, tokio::sync::Notify)>,
        requests: Arc<StdMutex<Vec<Vec<ModelRequest>>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            let ordinal = {
                let mut requests = requests.lock().unwrap();
                let ordinal = requests.len();
                requests.push(Vec::new());
                ordinal
            };
            let prompt_final = self.prompt_final;
            let scenario = self.scenario;
            let admission = Arc::clone(&self.admission);
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(
                        Model {
                            ordinal,
                            prompt_final,
                            scenario,
                            admission,
                            requests,
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
    for (prompt_final, scenario) in [false, true].into_iter().flat_map(|prompt| {
        ["early", "one_turn", "tools", "tighten"].map(|scenario| (prompt, scenario))
    }) {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let database = directory.path().join("sessions.sqlite3");
        let admission = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let final_turns = match scenario {
            "one_turn" => 1,
            "tools" | "tighten" => 3,
            _ => 2,
        };
        let loader = Arc::new(Loader {
            prompt_final,
            scenario,
            admission: Arc::clone(&admission),
            requests: Arc::clone(&requests),
        });
        let runtime =
            SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader.clone())
                .await
                .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::ReadOnly).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal", "session_id":session_id, "objective":"finish exactly ten charged slices with a handoff",
            "budget":{"max_goal_runs":if scenario=="tighten" {11} else {10}, "deadline_ms":now_ms()+60_000,
                "per_run":{"max_model_turns":if scenario=="one_turn" && !prompt_final {1} else {3}}}
        })).unwrap()).await.unwrap();
        let mut starts = Vec::new();
        let mut submitted = false;
        let mut tightened = false;
        let goal = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = tokio::select! {
                    _ = admission.0.notified(), if scenario == "tighten" && !tightened => {
                        let current = runtime.snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 256, 256))
                            .await.unwrap().focused.unwrap().goal.unwrap();
                        let expected = qq_protocol::GoalIdentity { goal_id: current.goal_id, revision: current.revision };
                        let mut budget = current.budget;
                        budget.max_goal_runs = 10;
                        runtime.command(CommandId::generate().unwrap(), SessionCommand::SetGoal {
                            session_id, objective: current.objective, check: current.check,
                            budget: Box::new(budget), expected: Some(expected), replace: false,
                        }).await.unwrap();
                        tightened = true;
                        admission.1.notify_one();
                        continue;
                    }
                    event = events.next() => event.expect("goal stream ended before exhaustion"),
                };
                match event.unwrap().event {
                    SessionEvent::RunStarted { run_id, origin, .. } => {
                        starts.push((run_id, origin))
                    }
                    SessionEvent::RunFinished { .. }
                        if prompt_final && starts.len() == 9 && !submitted =>
                    {
                        let receipt = runtime
                            .command(
                                CommandId::generate().unwrap(),
                                SessionCommand::SubmitPrompt {
                                    session_id,
                                    input: vec![InputPart::text("give the final handoff")],
                                    limits: RunLimits {
                                        max_model_turns: (scenario=="one_turn").then_some(1),
                                        ..RunLimits::default()
                                    },
                                    correlation: Correlation::default(),
                                    output: None,
                                },
                            )
                            .await
                            .unwrap();
                        assert!(matches!(
                            receipt.outcome,
                            CommandOutcome::PromptQueued { .. }
                        ));
                        submitted = true;
                    }
                    SessionEvent::GoalUpdated { goal }
                        if matches!(
                            goal.status,
                            qq_protocol::GoalStatus::BudgetExhausted { .. }
                        ) =>
                    {
                        return goal;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("ten-run goal must exhaust its finite allowance");
        assert_eq!(
            goal.counters.runs, 10,
            "{scenario}, prompt_final={prompt_final}"
        );
        assert!(!goal.notes.contains("unexpectedly executed"));
        assert_eq!(
            goal.status,
            qq_protocol::GoalStatus::BudgetExhausted {
                reason: qq_protocol::GoalExhaustion::Runs
            }
        );
        assert_eq!(starts.len(), 10);
        let final_run = starts[9].0;
        assert_eq!(
            starts
                .iter()
                .map(|(run, _)| *run)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            10
        );
        assert!(
            starts[..9]
                .iter()
                .all(|(_, origin)| matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. })))
        );
        assert!(if prompt_final {
            matches!(starts[9].1, Some(qq_protocol::RunOrigin::UserPrompt {}))
        } else {
            matches!(starts[9].1, Some(qq_protocol::RunOrigin::Goal { .. }))
        });
        runtime.close().await.unwrap();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 10, "no eleventh run may start");
            assert_eq!(
                requests[9].len(),
                final_turns,
                "final slot must honor its caller cap: {scenario}, prompt_final={prompt_final}"
            );
            if scenario != "tighten" {
                assert!(
                    request_texts(&requests[9][0])
                        .iter()
                        .any(|text| text.contains("final budgeted run")),
                    "the final slot is announced before its first request"
                );
            }
            let handoff = &requests[9][usize::from(scenario != "one_turn")];
            assert_eq!(handoff.tool_choice(), qq_provider::ToolChoice::None);
            assert!(
                request_texts(handoff)
                    .iter()
                    .any(|text| text.contains("final budgeted run"))
            );
        }
        let connection = Connection::open(&database).unwrap();
        let limits_json: String = connection
            .query_row(
                "SELECT limits_json FROM runs WHERE id=?1",
                [final_run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let limits: RunLimits = serde_json::from_str(&limits_json).unwrap();
        assert_eq!(
            limits.max_model_turns,
            if prompt_final && scenario == "tighten" {
                None
            } else {
                Some(if scenario == "one_turn" { 1 } else { 3 })
            },
            "admission applies the final-slot cap only when already classified final: {scenario}, user_prompt={prompt_final}"
        );
        let turns: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM model_turns WHERE run_id=?1",
                [final_run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            turns, final_turns as u32,
            "all final-slot model turns must be durable: {scenario}"
        );
        drop(connection);
        let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database), loader)
            .await
            .unwrap();
        let replayed = reopened
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
            replayed
                .messages
                .iter()
                .any(|message| message.run_id == final_run
                    && message.output == "durable final-slot handoff")
        );
        let restored = reopened
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session_id)?.unwrap())
            })
            .await
            .unwrap();
        assert_eq!(restored, *goal);
        reopened.shutdown().await.unwrap();
    }
}
