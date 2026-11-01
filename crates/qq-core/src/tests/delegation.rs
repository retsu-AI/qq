use super::*;

struct SpawnCallProvider {
    turn: Mutex<usize>,
    model: Option<&'static str>,
}

impl Provider for SpawnCallProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0".to_owned(),
                    json: self.model.map_or_else(
                        || r#"{"task":"count the widgets"}"#.to_owned(),
                        |model| format!(r#"{{"task":"count the widgets","model":"{model}"}}"#),
                    ),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "call_0".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}

struct DeliveringSpawner {
    delivered: Mutex<Vec<runtime::DeliveredChild>>,
}

impl SubagentSpawner for DeliveringSpawner {
    fn acknowledge(&self, _: ToolCallId) {}
    fn drain(&self) -> runtime::ChildDrainFuture {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn spawn(&self, _: SpawnRequest) -> SpawnAgentFuture {
        unreachable!("this spawner only delivers")
    }
    fn deliver(&self, _: u32, _: runtime::ReportDelivery) -> runtime::DeliverFuture {
        let delivered = std::mem::take(&mut *self.delivered.lock().unwrap());
        Box::pin(std::future::ready(Ok(delivered)))
    }
}

#[tokio::test]
async fn only_a_delivered_answer_restarts_the_stall_count() {
    let child = |answered: bool, interim: bool| runtime::DeliveredChild {
        notice: format!("child answered: {answered}"),
        answered,
        interim,
        spend: SpawnAgentSpend::NONE,
    };
    for (answered, interim, restarts) in [
        (false, false, false),
        (true, false, true),
        (false, true, false),
    ] {
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(DeliveringSpawner {
            delivered: Mutex::new(vec![child(answered, interim)]),
        });
        let mut stall = runtime::StallScope::new(runtime::StallPolicy::Root);
        for _ in 0..runtime::STALL_REPORT_CALLS {
            stall.settled(false);
        }
        assert_eq!(stall.due(false), runtime::ReportDue::Report);
        let mut messages = vec![Message::user("prompt")];
        let mut bytes = 0;
        let mut budget = BudgetMeter::new(RunLimits::default(), None, tokio::time::Instant::now());
        let delivered = deliver_children(
            &spawner,
            Boundary {
                turn_ordinal: 2,
                reports: runtime::ReportDelivery::Always,
            },
            &mut messages,
            &mut bytes,
            &mut budget,
            &mut stall,
        )
        .await
        .unwrap();
        assert_eq!(
            delivered,
            Delivered {
                answers: usize::from(!interim),
                reports: usize::from(interim),
            }
        );
        assert_eq!(
            messages.last().unwrap(),
            &Message::user(format!("child answered: {answered}"))
        );
        assert_eq!(
            stall.due(false) == runtime::ReportDue::None,
            restarts,
            "answered: {answered}"
        );
    }
}

#[tokio::test]
async fn spawner_less_runs_neither_declare_nor_dispatch_spawn_agent() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));

    struct CapturingSpawnProvider {
        inner: SpawnCallProvider,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for CapturingSpawnProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            self.inner.stream(request)
        }
    }

    let runtime = Runtime::new(
        CapturingSpawnProvider {
            inner: SpawnCallProvider {
                turn: Mutex::new(0),
                model: None,
            },
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
            if result == SPAWN_UNAVAILABLE_RESULT
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    for tool in [tools::SPAWN_AGENT_TOOL, "wait_agents", "cancel_agent"] {
        assert!(
            !requests[0].tools().iter().any(|spec| spec.name() == tool),
            "{tool}"
        );
    }
    let system = requests[0].system().unwrap();
    assert!(!system.contains("Delegation:"));
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == runtime::SEARCH_HISTORY_TOOL)
    );
}

#[tokio::test]
async fn a_tool_free_wait_takes_reports_only_with_an_answer() {
    struct AllowAllGate;
    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }
    struct Reporter {
        asked: Mutex<Vec<runtime::ReportDelivery>>,
        wakes: Mutex<usize>,
        answered: std::sync::atomic::AtomicBool,
    }
    impl SubagentSpawner for Reporter {
        fn acknowledge(&self, _: ToolCallId) {}
        fn drain(&self) -> runtime::ChildDrainFuture {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn spawn(&self, _: SpawnRequest) -> SpawnAgentFuture {
            Box::pin(std::future::ready(SpawnAgentOutcome {
                content: "started".to_owned(),
                is_error: false,
                spend: SpawnAgentSpend::NONE,
                session_id: None,
                detached: true,
            }))
        }
        fn outstanding_detached(&self) -> usize {
            usize::from(!self.answered.load(std::sync::atomic::Ordering::SeqCst))
        }
        fn child_settled(&self) -> runtime::ChildWaitFuture {
            Box::pin(std::future::ready(()))
        }
        fn deliver(&self, _: u32, reports: runtime::ReportDelivery) -> runtime::DeliverFuture {
            self.asked.lock().unwrap().push(reports);
            let report = runtime::DeliveredChild {
                notice: "interim report".to_owned(),
                answered: false,
                interim: true,
                spend: SpawnAgentSpend::NONE,
            };
            let batch = match reports {
                runtime::ReportDelivery::Always => vec![report],
                runtime::ReportDelivery::WithAnswers => {
                    let mut wakes = self.wakes.lock().unwrap();
                    *wakes += 1;
                    if *wakes < 3 {
                        Vec::new()
                    } else {
                        self.answered
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        vec![
                            runtime::DeliveredChild {
                                notice: "the answer".to_owned(),
                                answered: true,
                                interim: false,
                                spend: SpawnAgentSpend::NONE,
                            },
                            report,
                        ]
                    }
                }
            };
            Box::pin(std::future::ready(Ok(batch)))
        }
    }
    let spawner = Arc::new(Reporter {
        asked: Mutex::new(Vec::new()),
        wakes: Mutex::new(0),
        answered: std::sync::atomic::AtomicBool::new(false),
    });
    let requests = Arc::new(Mutex::new(Vec::new()));
    struct Recording {
        inner: SpawnCallProvider,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }
    impl Provider for Recording {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request.clone());
            self.inner.stream(request)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(
        Recording {
            inner: SpawnCallProvider {
                turn: Mutex::new(0),
                model: None,
            },
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("go")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>)),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    use runtime::ReportDelivery::{Always, WithAnswers};
    assert_eq!(
        spawner.asked.lock().unwrap().as_slice(),
        [Always, Always, WithAnswers, WithAnswers, WithAnswers]
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let tail = requests[2]
        .messages()
        .iter()
        .rev()
        .take(3)
        .map(|message| (message.role(), message.content().to_vec()))
        .collect::<Vec<_>>();
    let text = |text: &str| {
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }]
    };
    assert_eq!(
        tail,
        [
            (Role::User, text("interim report")),
            (Role::User, text("the answer")),
            (Role::Assistant, text("done")),
        ]
    );
}

#[tokio::test]
async fn spawner_runs_declare_the_tool_and_truncate_oversized_child_answers() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let tasks = Arc::new(Mutex::new(Vec::new()));
    let spawner = Arc::new(StubSpawner::new(
        SpawnAgentOutcome {
            content: format!("{}\n", "x".repeat(1_023)).repeat(200),
            is_error: false,
            spend: SpawnAgentSpend::NONE,
            session_id: None,
            detached: false,
        },
        Arc::clone(&tasks),
    ));
    let runtime = Runtime::new(
        SpawnCallProvider {
            turn: Mutex::new(0),
            model: None,
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("go")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(Some(spawner)),
        )
        .collect::<Vec<_>>()
        .await;

    let result = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished {
                result,
                is_error: false,
                ..
            } => Some(result.clone()),
            _ => None,
        })
        .expect("the spawn call must finish successfully");
    assert!(result.len() <= tools::MAX_MODEL_TEXT_BYTES);
    assert!(result.contains("…[qq: "));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(
        tasks.lock().unwrap().as_slice(),
        [("count the widgets".to_owned(), None)]
    );
}

#[tokio::test]
async fn spawn_model_overrides_are_normalized_and_forwarded_to_the_spawner() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    let cases = [
        (None, None),
        (Some(""), None),
        (Some("   "), None),
        (Some("openai-codex/gpt-test"), Some("openai-codex/gpt-test")),
        (Some("openai/gpt-guessed"), Some("openai/gpt-guessed")),
    ];
    for (requested, expected) in cases {
        let directory = tempfile::tempdir().unwrap();
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let spawner = Arc::new(StubSpawner::new(
            SpawnAgentOutcome {
                content: "child answer".to_owned(),
                is_error: false,
                spend: SpawnAgentSpend::NONE,
                session_id: None,
                detached: false,
            },
            Arc::clone(&tasks),
        ));
        let runtime = Runtime::new(
            SpawnCallProvider {
                turn: Mutex::new(0),
                model: requested,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_spawn_model_routes(vec!["openai-codex/gpt-test".to_owned()]);

        let _events = runtime
            .run_loop_with_spawner(
                vec![Message::user("go")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(Some(spawner)),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            tasks.lock().unwrap().as_slice(),
            &[("count the widgets".to_owned(), expected.map(str::to_owned))]
        );
    }
}

#[tokio::test]
async fn children_receive_the_parents_remaining_budget_and_their_usage_rolls_up() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct MeteredSpawnProvider {
        turn: Mutex<usize>,
    }

    impl Provider for MeteredSpawnProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            let usage = Some(qq_provider::ProviderUsage {
                input_tokens: 100,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 50,
                reasoning_tokens: None,
            });
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"task":"count the widgets"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let spawner = Arc::new(StubSpawner::new(
        SpawnAgentOutcome {
            content: "child answer".to_owned(),
            is_error: false,
            spend: SpawnAgentSpend {
                cost_usd_nanos: Some(0),
                usage: Some(TokenUsage {
                    input_tokens: 700,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 200,
                    reasoning_tokens: None,
                }),
            },
            session_id: None,
            detached: false,
        },
        Arc::new(Mutex::new(Vec::new())),
    ));
    let limits = RunLimits {
        max_total_tokens: Some(1_000),
        max_input_tokens: Some(1_000),
        max_model_turns: Some(10),
        ..RunLimits::default()
    };
    let runtime = Runtime::new(
        MeteredSpawnProvider {
            turn: Mutex::new(0),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("go")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>))
                .with_limits(limits, None),
        )
        .collect::<Vec<_>>()
        .await;

    let requests = spawner.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].budget.limits.max_total_tokens, Some(850));
    assert_eq!(requests[0].budget.limits.max_input_tokens, Some(900));
    assert_eq!(requests[0].budget.limits.max_model_turns, None);
    assert_eq!(requests[0].budget.limits.max_duration_ms, None);
    drop(requests);

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::BudgetExhausted { exhaustion })
            if exhaustion.limit == BudgetLimitKind::TotalTokens
    ));
}

#[tokio::test]
async fn a_spawn_the_parent_cannot_afford_is_refused_as_a_tool_error() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct ExhaustedSpawnProvider {
        turn: Mutex<usize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for ExhaustedSpawnProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"task":"count the widgets"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 500,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 10,
                            reasoning_tokens: None,
                        }),
                    }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let spawner = Arc::new(StubSpawner::new(
        SpawnAgentOutcome {
            content: "never runs".to_owned(),
            is_error: false,
            spend: SpawnAgentSpend::NONE,
            session_id: None,
            detached: false,
        },
        Arc::new(Mutex::new(Vec::new())),
    ));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        ExhaustedSpawnProvider {
            turn: Mutex::new(0),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let limits = RunLimits {
        max_input_tokens: Some(500),
        ..RunLimits::default()
    };
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("go")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>))
                .with_limits(limits, None),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(
        spawner.requests.lock().unwrap().is_empty(),
        "no child was admitted"
    );
    let refusal = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished {
                result, is_error, ..
            } => Some((result.clone(), *is_error)),
            _ => None,
        })
        .expect("the spawn call settles as a tool result");
    assert!(refusal.1);
    assert!(
        refusal.0.contains("cannot afford a sub-agent") && refusal.0.contains("input_tokens"),
        "{}",
        refusal.0
    );
}

#[test]
fn subagent_prompts_say_a_parent_is_waiting_and_read_children_drop_the_implement_line() {
    const IMPLEMENT: &str = "- Do the work. Decide what done looks like, read what the change needs, make it when a change is requested, verify it, and report. For an analysis-only or review request, deliver the findings without changing files. A plan, a proposal, or an offer to continue is not a result: end your turn when the task is done or a concrete blocker remains, and name the blocker.\n";
    const FINAL: &str = "- End with the outcome first, in a few lines: what changed or what you found, how you verified it, and anything left undone. Cite code as path:line. When a next step is within the request, take it instead of offering it.\n";
    let instructions = workspace::WorkspaceInstructions::empty();
    let render = |subagent| {
        runtime::agent_system_prompt(
            &tools::specs(),
            runtime::PromptSections {
                subagent,
                ..runtime::PromptSections::default()
            },
            &instructions,
            None,
            None,
        )
    };
    let root = render(None);
    let read = render(Some(runtime::SubagentAuthority::Read));
    let write = render(Some(runtime::SubagentAuthority::Write));

    assert!(root.contains(IMPLEMENT));
    assert!(!root.contains("Sub-agent:"));
    for child in [&read, &write] {
        assert!(child.contains("\n\nSub-agent:\n"), "{child}");
        assert!(child.contains("only your final reply reaches it"));
        assert!(child.contains("Stop as soon as you can answer"));
        assert!(child.contains("answer first, then the evidence as path:line"));
        assert!(child.contains("Do not re-read text that is still in your context"));
    }
    assert!(!read.contains(IMPLEMENT));
    assert!(read.contains("You cannot change files or run commands"));
    assert!(write.contains(IMPLEMENT));
    let without_section = |prompt: &str| {
        let (head, _) = prompt.split_once("\n\nSub-agent:\n").unwrap();
        head.to_owned()
    };
    let root = root
        .replacen(FINAL, "", 1)
        .replacen(" QQ takes no snapshot for this run, so qq undo cannot restore it.", "", 1)
        .replacen("- Mode: Auto. Calls run without asking anyone, and ask_user gets no answer.", "- Mode: within your parent's authority. A call the parent's mode would not run without a human is denied.", 1);
    assert_eq!(without_section(&write), root);
    assert_eq!(without_section(&read), root.replacen(IMPLEMENT, "", 1).replacen("- Explore only as far as your next action needs. Once you can name the file and the change, make the change.", "- Explore only as far as the next finding needs. Once you can answer the brief, return the evidence.", 1));
}

#[test]
fn delegation_authority_guidance_matches_the_spawn_schema() {
    let instructions = workspace::WorkspaceInstructions::empty();
    for write_children in [false, true] {
        let delegation = DelegationRoster {
            write_children,
            ..DelegationRoster::default()
        };
        let specs = [tools::spawn_agent_spec(&[], &delegation)];
        let prompt = runtime::agent_system_prompt(
            &specs,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert_eq!(prompt.contains("authority: write"), write_children);
        assert_eq!(
            prompt.contains("one-shot read-only sub-agent"),
            !write_children
        );
        if write_children {
            assert!(prompt.contains("read by default"));
            assert!(prompt.contains("Auto within the parent"));
            assert!(prompt.contains("one write sub-agent"));
            assert!(prompt.contains("implementation task"));
        }
    }
}

#[test]
fn delegation_guidance_asks_for_a_question_a_purpose_and_an_answer_shape() {
    let instructions = workspace::WorkspaceInstructions::empty();
    let mut specs = tools::specs();
    specs.push(tools::spawn_agent_spec(&[], &DelegationRoster::default()));
    let prompt = runtime::agent_system_prompt(
        &specs,
        runtime::PromptSections::default(),
        &instructions,
        None,
        None,
    );
    assert!(prompt.contains(
        "- Write the brief as a question to answer, what the answer is for, and the shape \
             you want back"
    ));
    assert!(prompt.contains("Prefer several narrow briefs over one broad one"));
    let spawn = specs.last().unwrap();
    let task = spawn.input_schema().get();
    assert!(
        task.contains("the question to answer, what the answer is for, and the answer shape"),
        "{task}"
    );
}

#[test]
fn ab4_read_only_root_states_authority_and_has_no_action_obligation() {
    let instructions = workspace::WorkspaceInstructions::empty();
    let read_only = tools::specs()
        .into_iter()
        .filter(|spec| matches!(spec.name(), "read_file" | "tree" | "search" | "ask_user"))
        .collect::<Vec<_>>();
    assert_eq!(read_only.len(), 4);
    let prompt = runtime::agent_system_prompt(
        &read_only,
        runtime::PromptSections {
            read_only: true,
            ..runtime::PromptSections::default()
        },
        &instructions,
        None,
        None,
    );
    assert!(!prompt.contains("- Do the work. Decide what done looks like"));
    assert!(prompt.contains("This run cannot change files or run commands"));
    assert!(!prompt.contains("make the change. Search an exact name once"));
    assert!(!prompt.contains("Sub-agent:"));
}

#[test]
fn agent_prompt_advertises_fetch_only_when_declared() {
    let instructions = workspace::WorkspaceInstructions::empty();
    let specs = tools::specs();
    let prompt = runtime::agent_system_prompt(
        &specs,
        runtime::PromptSections::default(),
        &instructions,
        None,
        None,
    );
    assert_eq!(
        prompt.contains("fetch reads one public"),
        cfg!(feature = "tool-fetch")
    );
    assert_eq!(
        prompt.contains("Prefer fetch over shell"),
        cfg!(feature = "tool-fetch")
    );
    let without: Vec<_> = specs
        .into_iter()
        .filter(|spec| spec.name() != "fetch")
        .collect();
    let prompt = runtime::agent_system_prompt(
        &without,
        runtime::PromptSections::default(),
        &instructions,
        None,
        None,
    );
    assert!(!prompt.contains("fetch reads one public"));
    assert!(!prompt.contains("Prefer fetch over shell"));
}

#[test]
fn agent_prompt_teaches_delegation_only_when_spawn_agent_is_declared() {
    let instructions = workspace::WorkspaceInstructions::empty();
    let without = runtime::agent_system_prompt(
        &tools::specs(),
        runtime::PromptSections::default(),
        &instructions,
        None,
        None,
    );
    assert!(!without.contains("spawn_agent"));
    assert!(!without.contains("Delegation:"));

    let mut specs = tools::specs();
    specs.push(tools::spawn_agent_spec(&[], &DelegationRoster::default()));
    let with = runtime::agent_system_prompt(
        &specs,
        runtime::PromptSections::default(),
        &instructions,
        None,
        None,
    );
    assert!(with.contains("spawn_agent"));
    assert!(with.contains("Delegation:"));
    assert!(with.contains("independent questions"));
    assert!(with.contains("read-only sub-agent"));
    assert!(with.contains("Omit spawn_agent's model argument by default"));
    assert!(with.contains("configured worker model"));
    assert!(with.contains("persisted selected model"));
    assert!(with.contains("never guess, translate, or invent one"));
}

#[test]
fn delegation_route_resolution_prefers_model_then_role_then_default() {
    use qq_protocol::{DelegationRole, DelegationRoster, DelegationRosterEntry, ReasoningEffort};
    let entry = |route: &str, role| DelegationRosterEntry {
        route: route.to_owned(),
        role,
        note: None,
        effort: None,
        context_window: None,
        max_output_tokens: None,
        relative_cost_permille: None,
    };
    let roster = DelegationRoster {
        roster: vec![
            entry("openai/fast", DelegationRole::Fast),
            entry("anthropic/balanced", DelegationRole::Balanced),
            entry("anthropic/balanced-2", DelegationRole::Balanced),
        ],
        default_role: DelegationRole::Balanced,
        max_depth: 1,
        write_children: false,
    };

    assert_eq!(
        resolve_delegation_route(
            &roster,
            Some("openai/fast".to_owned()),
            Some(DelegationRole::Balanced),
            None,
        ),
        Ok((Some("openai/fast".to_owned()), Some(ReasoningEffort::Low)))
    );
    assert!(
        resolve_delegation_route(&roster, Some("openai/other".to_owned()), None, None)
            .unwrap_err()
            .contains("not on the delegation roster")
    );
    assert_eq!(
        resolve_delegation_route(&roster, None, Some(DelegationRole::Balanced), None),
        Ok((
            Some("anthropic/balanced".to_owned()),
            Some(ReasoningEffort::Medium)
        ))
    );
    assert_eq!(
        resolve_delegation_route(&roster, None, None, None),
        Ok((
            Some("anthropic/balanced".to_owned()),
            Some(ReasoningEffort::Medium)
        ))
    );
    assert!(
        resolve_delegation_route(&roster, None, Some(DelegationRole::Strong), None)
            .unwrap_err()
            .contains("strong role")
    );

    let none = DelegationRoster::default();
    assert_eq!(
        resolve_delegation_route(&none, Some("x/y".to_owned()), None, None),
        Ok((Some("x/y".to_owned()), None))
    );
    assert_eq!(
        resolve_delegation_route(&none, None, None, None),
        Ok((None, None))
    );
    assert!(
        resolve_delegation_route(&none, None, Some(DelegationRole::Fast), None)
            .unwrap_err()
            .contains("no delegation roster")
    );
}

#[test]
fn child_effort_derives_from_the_role_and_never_exceeds_the_parent() {
    use qq_protocol::{
        DelegationRole, DelegationRosterEntry, ReasoningEffort, child_reasoning_effort,
    };
    let entry = |role, effort| DelegationRosterEntry {
        route: "p/m".to_owned(),
        role,
        note: None,
        effort,
        context_window: None,
        max_output_tokens: None,
        relative_cost_permille: None,
    };
    let fast = entry(DelegationRole::Fast, None);
    let balanced = entry(DelegationRole::Balanced, None);
    let strong = entry(DelegationRole::Strong, None);
    assert_eq!(
        child_reasoning_effort(&fast, Some(ReasoningEffort::Max)),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(
        child_reasoning_effort(&balanced, Some(ReasoningEffort::Max)),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        child_reasoning_effort(&strong, Some(ReasoningEffort::Max)),
        Some(ReasoningEffort::Max)
    );
    assert_eq!(
        child_reasoning_effort(&balanced, Some(ReasoningEffort::Minimal)),
        Some(ReasoningEffort::Minimal)
    );
    assert_eq!(
        child_reasoning_effort(&fast, None),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(
        child_reasoning_effort(&fast, Some(ReasoningEffort::Default)),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(child_reasoning_effort(&strong, None), None);
    assert_eq!(
        child_reasoning_effort(
            &entry(DelegationRole::Fast, Some(ReasoningEffort::High)),
            Some(ReasoningEffort::Low)
        ),
        Some(ReasoningEffort::High)
    );
}

#[test]
fn roster_prompt_names_the_current_model_and_each_route_with_relative_cost() {
    use qq_protocol::{DelegationRole, DelegationRoster, DelegationRosterEntry};
    let roster = DelegationRoster {
        roster: vec![
            DelegationRosterEntry {
                route: "openai/fast".to_owned(),
                role: DelegationRole::Fast,
                note: Some("lookups, breadth".to_owned()),
                effort: None,
                context_window: Some(400_000),
                max_output_tokens: None,
                relative_cost_permille: Some(150),
            },
            DelegationRosterEntry {
                route: "anthropic/same".to_owned(),
                role: DelegationRole::Balanced,
                note: None,
                effort: None,
                context_window: Some(200_000),
                max_output_tokens: None,
                relative_cost_permille: Some(1000),
            },
            DelegationRosterEntry {
                route: "anthropic/strong".to_owned(),
                role: DelegationRole::Strong,
                note: None,
                effort: None,
                context_window: None,
                max_output_tokens: None,
                relative_cost_permille: Some(2_500),
            },
            DelegationRosterEntry {
                route: "custom/unpriced".to_owned(),
                role: DelegationRole::Strong,
                note: None,
                effort: None,
                context_window: Some(1_500),
                max_output_tokens: None,
                relative_cost_permille: None,
            },
        ],
        default_role: DelegationRole::Balanced,
        max_depth: 1,
        write_children: false,
    };
    let text =
        runtime::delegation_roster_text("anthropic/current", Some(1_000_000), &roster).unwrap();
    assert_eq!(
        text,
        "         - You are running as anthropic/current (1M context). Roster (default role: balanced):\n\
             \x20          - fast: openai/fast — 400k context; ~15% of your cost; lookups, breadth\n\
             \x20          - balanced: anthropic/same — 200k context; same cost as you\n\
             \x20          - strong: anthropic/strong — ~2.5x your cost\n\
             \x20          - strong: custom/unpriced — 1500 context"
    );
    assert!(runtime::delegation_roster_text("x", None, &DelegationRoster::default()).is_none());

    let instructions = workspace::WorkspaceInstructions::empty();
    let mut specs = tools::specs();
    specs.push(tools::spawn_agent_spec(&[], &roster));
    let prompt = runtime::agent_system_prompt(
        &specs,
        runtime::PromptSections {
            roster: Some(&text),
            ..runtime::PromptSections::default()
        },
        &instructions,
        None,
        None,
    );
    assert!(prompt.contains("Delegation:"));
    assert!(prompt.contains("Choose the sub-agent by spawn_agent's role argument"));
    assert!(prompt.contains("You are running as anthropic/current"));
    assert!(prompt.contains("- fast: openai/fast"));
    assert!(!prompt.contains("configured worker model"));
    assert!(prompt.contains("independent questions"));
}
