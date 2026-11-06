use super::*;

struct SummarizeScript {
    turns: Mutex<std::collections::VecDeque<Vec<Result<ProviderEvent, ProviderError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for SummarizeScript {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted turn");
        Box::pin(stream::iter(turn))
    }
}

fn summarize_call(id: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::ToolCallStarted {
            id: id.to_owned(),
            name: "read_file".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: id.to_owned(),
            json: r#"{"path":"x"}"#.to_owned(),
        }),
        Ok(ProviderEvent::ToolCallCompleted { id: id.to_owned() }),
    ]
}

async fn run_summarize(
    turns: Vec<Vec<Result<ProviderEvent, ProviderError>>>,
) -> (Result<String, String>, Vec<ModelRequest>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        SummarizeScript {
            turns: Mutex::new(turns.into()),
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let tools: Arc<[ToolSpec]> = Arc::from([ToolSpec::new(
        "read_file",
        "read",
        serde_json::json!({"type": "object"}),
    )]);
    let steps = runtime.summarize(
        vec![Message::user("summarize")],
        Arc::from("system"),
        tools,
        None,
        256,
        qq_provider::RetryOwner::Provider,
    );
    tokio::pin!(steps);
    let mut result = Err("summarizer ended without a summary".to_owned());
    while let Some(step) = steps.next().await {
        match step {
            Ok(runtime::SummarizeStep::Summary(summary)) => {
                result = Ok(summary);
                break;
            }
            Ok(_) => {}
            Err(error) => {
                result = Err(error.to_string());
                break;
            }
        }
    }
    let requests = requests.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test]
async fn summary_fold_preserves_the_cumulative_run_text_bound() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let portion = "x".repeat(MAX_RUN_MODEL_TEXT_BYTES / 2 + 1);
    let turn = || {
        vec![
            Ok(ProviderEvent::OutputTextDelta {
                text: portion.clone(),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]
    };
    let runtime = Runtime::new(
        SummarizeScript {
            turns: Mutex::new(
                vec![
                    vec![Err(ProviderError::ResponseFailed {
                        kind: qq_provider::ProviderErrorKind::ContextExceeded,
                        message: "overflow".to_owned(),
                    })],
                    turn(),
                    turn(),
                ]
                .into(),
            ),
            requests: requests.clone(),
        },
        "test-model",
        256,
    )
    .unwrap();
    let steps = runtime.summarize(
        vec![
            Message::user("evidence ".repeat(2000)),
            Message::user(sessions::COMPACTION_INSTRUCTION),
        ],
        Arc::from("system"),
        Arc::from([]),
        None,
        256,
        qq_provider::RetryOwner::Provider,
    );
    tokio::pin!(steps);
    let mut rejected = false;
    let mut completed = false;
    while let Some(step) = steps.next().await {
        match step {
            Err(runtime::SummarizeError::Rejected(message)) => {
                rejected = message.contains("run text bound")
            }
            Ok(runtime::SummarizeStep::Summary(_)) => completed = true,
            _ => {}
        }
    }
    assert!(rejected);
    assert!(!completed);
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn summary_fold_rejects_empty_intermediate_portions_before_more_provider_work() {
    for partial in ["", " \n\t"] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            SummarizeScript {
                requests: requests.clone(),
                turns: Mutex::new(
                    vec![
                        vec![Err(ProviderError::ResponseFailed {
                            kind: qq_provider::ProviderErrorKind::ContextExceeded,
                            message: "context length exceeded".to_owned(),
                        })],
                        vec![
                            Ok(ProviderEvent::OutputTextDelta {
                                text: partial.to_owned(),
                            }),
                            Ok(ProviderEvent::Completed { usage: None }),
                        ],
                        vec![
                            Ok(ProviderEvent::OutputTextDelta {
                                text: "final without earlier evidence".to_owned(),
                            }),
                            Ok(ProviderEvent::Completed { usage: None }),
                        ],
                    ]
                    .into(),
                ),
            },
            "test-model",
            256,
        )
        .unwrap();
        let steps = runtime.summarize(
            vec![
                Message::user("evidence ".repeat(2000)),
                Message::user(sessions::COMPACTION_INSTRUCTION),
            ],
            Arc::from("summarize"),
            Arc::from([]),
            None,
            256,
            qq_provider::RetryOwner::Provider,
        );
        tokio::pin!(steps);
        let mut rejected = false;
        while let Some(step) = steps.next().await {
            match step {
                Err(runtime::SummarizeError::Rejected(message)) => {
                    assert!(message.contains("empty summary"), "{message}");
                    rejected = true;
                }
                Ok(runtime::SummarizeStep::Summary(text)) => {
                    panic!("discarded earlier evidence: {text}")
                }
                other => {
                    other.expect("summary work must persist before rejection");
                }
            }
        }
        assert!(rejected);
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "never send the later portion after dropping its predecessor"
        );
    }
}

#[tokio::test]
async fn summary_fold_resets_continuations_and_reports_rejected_work() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let complete = |text: &str| {
        vec![
            Ok(ProviderEvent::OutputTextDelta {
                text: text.to_owned(),
            }),
            Ok(ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
            }),
        ]
    };
    let truncated = || {
        vec![
            Ok(ProviderEvent::OutputTextDelta {
                text: "part ".to_owned(),
            }),
            Ok(ProviderEvent::Incomplete {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
                reason: qq_provider::IncompleteReason::OutputTokens,
            }),
        ]
    };
    let mut turns = vec![vec![Err(ProviderError::ResponseFailed {
        kind: qq_provider::ProviderErrorKind::ContextExceeded,
        message: "context length exceeded".to_owned(),
    })]];
    for _ in 0..2 {
        for _ in 0..MAX_OUTPUT_CONTINUATIONS {
            turns.push(truncated());
        }
        turns.push(complete("finished"));
    }
    let runtime = Runtime::new(
        SummarizeScript {
            turns: Mutex::new(turns.into()),
            requests: requests.clone(),
        },
        "test-model",
        256,
    )
    .unwrap();
    let steps = runtime.summarize(
        vec![
            Message::user("evidence ".repeat(2000)),
            Message::user(sessions::COMPACTION_INSTRUCTION),
        ],
        Arc::from("system"),
        Arc::from([]),
        None,
        256,
        qq_provider::RetryOwner::Provider,
    );
    tokio::pin!(steps);
    let mut rejected = 0;
    let mut summary = None;
    while let Some(step) = steps.next().await {
        match step.unwrap() {
            runtime::SummarizeStep::TurnCompleted {
                usage: Some(usage),
                message,
                ..
            } if !message.has_content() => {
                rejected += 1;
                assert_eq!(usage, TokenUsage::default());
            }
            runtime::SummarizeStep::Summary(text) => summary = Some(text),
            _ => {}
        }
    }
    assert_eq!(rejected, 1);
    assert!(summary.unwrap().ends_with("finished"));
    assert_eq!(
        requests.lock().unwrap().len(),
        1 + 2 * (usize::from(MAX_OUTPUT_CONTINUATIONS) + 1)
    );
}

#[tokio::test]
async fn summary_fold_can_split_after_many_completed_continuation_turns() {
    struct SmallWindow {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }
    impl Provider for SmallWindow {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let oversized = measure_messages(request.messages()) > 5_000;
            let fragments = request
                .messages()
                .iter()
                .filter(|m| m.role() == Role::Assistant)
                .count();
            self.requests.lock().unwrap().push(request);
            if oversized {
                return Box::pin(stream::iter([Err(ProviderError::ResponseFailed {
                    kind: qq_provider::ProviderErrorKind::ContextExceeded,
                    message: "context length exceeded".to_owned(),
                })]));
            }
            let terminal = if fragments < usize::from(MAX_OUTPUT_CONTINUATIONS) {
                ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                }
            } else {
                ProviderEvent::Completed { usage: None }
            };
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "fragment ".to_owned(),
                }),
                Ok(terminal),
            ]))
        }
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        SmallWindow {
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let steps = runtime.summarize(
        vec![
            Message::user("evidence ".repeat(8_192)),
            Message::user(sessions::COMPACTION_INSTRUCTION),
        ],
        Arc::from("system"),
        Arc::from([]),
        None,
        256,
        qq_provider::RetryOwner::Provider,
    );
    tokio::pin!(steps);
    let mut summary = None;
    while let Some(step) = steps.next().await {
        if let runtime::SummarizeStep::Summary(text) =
            step.expect("remaining split capacity must survive earlier continuations")
        {
            summary = Some(text);
        }
    }
    assert!(summary.is_some());
    assert!(requests.lock().unwrap().len() > 64);
}

#[tokio::test]
async fn in_run_summarize_answers_one_call_turn_with_rejections_and_keeps_only_the_new_reply() {
    let mut call_turn = vec![
        Ok(ProviderEvent::Replay {
            data: Arc::from("thinking-signature"),
        }),
        Ok(ProviderEvent::OutputTextDelta {
            text: "let me check".to_owned(),
        }),
    ];
    call_turn.extend(summarize_call("call_1"));
    call_turn.push(Ok(ProviderEvent::Completed { usage: None }));
    let (result, requests) = run_summarize(vec![
        vec![
            Ok(ProviderEvent::OutputTextDelta {
                text: "1. Intent: half".to_owned(),
            }),
            Ok(ProviderEvent::Incomplete {
                usage: None,
                reason: qq_provider::IncompleteReason::OutputTokens,
            }),
        ],
        call_turn,
        vec![
            Ok(ProviderEvent::OutputTextDelta {
                text: "the summary".to_owned(),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ],
    ])
    .await;
    assert_eq!(result.as_deref(), Ok("the summary"));
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.system(), Some("system"));
        assert_eq!(request.tools().len(), 1);
    }
    let messages = requests[2].messages();
    let (call, result) = (&messages[messages.len() - 2], &messages[messages.len() - 1]);
    assert_eq!(call.replay(), Some("thinking-signature"));
    assert!(call.content().iter().any(|block| matches!(
        block,
        ContentBlock::ToolCall { id, name, .. } if id == "call_1" && name == "read_file"
    )));
    assert!(matches!(
        result.content(),
        [ContentBlock::ToolResult { call_id, is_error: true, content }]
            if call_id == "call_1" && content == SUMMARIZER_TOOL_REJECTION
    ));
}

#[tokio::test]
async fn in_run_summarize_fails_on_a_second_call_turn_and_on_a_cut_call_turn() {
    let mut first = summarize_call("call_1");
    first.push(Ok(ProviderEvent::Completed { usage: None }));
    let mut second = summarize_call("call_2");
    second.push(Ok(ProviderEvent::Completed { usage: None }));
    let (result, requests) = run_summarize(vec![first, second]).await;
    assert_eq!(
        result,
        Err("summarizer called a tool on two turns".to_owned())
    );
    assert_eq!(requests.len(), 2);

    let mut cut = summarize_call("call_1");
    cut.push(Ok(ProviderEvent::Incomplete {
        usage: None,
        reason: qq_provider::IncompleteReason::OutputTokens,
    }));
    let (result, _) = run_summarize(vec![cut]).await;
    assert_eq!(
        result,
        Err("summarizer called a tool and was cut off".to_owned())
    );
}

#[tokio::test]
async fn a_summarizer_run_sends_the_prompt_runs_system_prompt_and_tools_without_context_sources() {
    struct Capture(Arc<Mutex<Vec<ModelRequest>>>);

    impl Provider for Capture {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.0.lock().unwrap().push(request);
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "{\"ok\":true}".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    struct CountingSource(Arc<AtomicUsize>);

    impl ContextSource for CountingSource {
        fn name(&self) -> &str {
            "memory"
        }
        fn version(&self) -> &str {
            "1"
        }
        fn cache_key(&self, _request: &context_source::ContextRequest) -> Option<[u8; 32]> {
            None
        }
        fn fetch(
            &self,
            _request: context_source::ContextRequest,
            _cancelled: RunCancellation,
        ) -> context_source::ContextFetchFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(context_source::ContextBundle {
                    items: vec![context_source::ContextItem {
                        provenance: "memory:0".to_owned(),
                        content: "remembered".to_owned(),
                    }],
                })
            })
        }
        fn fail_policy(&self) -> context_source::FailPolicy {
            context_source::FailPolicy::Open
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let fetches = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new(Capture(Arc::clone(&requests)), "test-model", 256)
        .unwrap()
        .with_context_source(Arc::new(CountingSource(Arc::clone(&fetches))));
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(&runtime, workspace))
            .unwrap()
    })
    .await
    .unwrap();
    let contract = output::CompiledOutputSchema::compile(&qq_protocol::OutputContract {
        schema: serde_json::json!({"type": "object"}),
        repair_turns: 0,
    })
    .unwrap();
    let user_key = plan::PromptPrefixKey {
        tools: Some(catalog::StaticFilter {
            spawn_agent: false,
            search_history: false,
            read_tool_result: false,
            load_skill: true,
            read_only: false,
            unattended: false,
        }),
        guidance: true,
        subagent: None,
        mode: ApprovalMode::Auto,
        snapshots: false,
    };
    for capabilities in [
        RunCapabilities::user(None),
        RunCapabilities::restricted()
            .with_output(Some(contract))
            .summarizer(user_key),
    ] {
        let events = plan
            .execute(
                vec![Message::user("hello")],
                RunCancellation::new(),
                Arc::new(StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
                Arc::new(workspace::FileState::default()),
                capabilities,
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
    }
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "only the prompt run fetches context"
    );
    let requests = requests.lock().unwrap();
    let (prompt, summarizer) = (&requests[0], &requests[1]);
    let prompt_system = prompt.system().unwrap();
    let summarizer_system = summarizer.system().unwrap();
    assert!(prompt_system.contains("[memory:0]"));
    assert!(!summarizer_system.contains("[memory:0]"));
    assert!(!summarizer_system.contains("## Output contract"));
    assert!(prompt_system.starts_with(summarizer_system));
    assert_eq!(summarizer.tools(), prompt.tools());
    assert!(!summarizer.tools().is_empty());
}

#[tokio::test]
async fn summary_fold_request_weights_match_each_replaced_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        SummarizeScript {
            turns: Mutex::new(
                vec![
                    vec![Err(ProviderError::ResponseFailed {
                        kind: qq_provider::ProviderErrorKind::ContextExceeded,
                        message: "overflow".to_owned(),
                    })],
                    vec![
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "PREFIX_FRAGMENT ".to_owned(),
                        }),
                        Ok(ProviderEvent::Incomplete {
                            reason: qq_provider::IncompleteReason::OutputTokens,
                            usage: None,
                        }),
                    ],
                    vec![
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "SUFFIX_FRAGMENT".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ],
                    vec![
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "final".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ],
                ]
                .into(),
            ),
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(&runtime, workspace))
            .unwrap()
    })
    .await
    .unwrap();
    let events = plan
        .execute(
            vec![
                Message::user("evidence ".repeat(12_000)),
                Message::user(sessions::COMPACTION_INSTRUCTION),
            ],
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::restricted().summarizer(plan::PromptPrefixKey {
                tools: None,
                guidance: false,
                subagent: None,
                mode: ApprovalMode::Auto,
                snapshots: false,
            }),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let weights = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Prepared { weight, .. } => Some(weight),
            _ => None,
        })
        .collect::<Vec<_>>();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(weights.len(), 4);
    let final_input = requests[3]
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(final_input.contains("PREFIX_FRAGMENT SUFFIX_FRAGMENT"));
    assert!(events.iter().any(|event| matches!(event, RuntimeEvent::AssistantTurnCompleted { usage: Some(usage), message, .. } if *usage == TokenUsage::default() && !message.has_content())));
    for (index, (weight, request)) in weights.iter().zip(requests.iter()).enumerate() {
        let boundary = if index == 2 {
            requests[1].messages().len() - 1
        } else {
            request.messages().len() - 1
        };
        assert_eq!(
            weight.reducible_message_bytes,
            measure_messages(&request.messages()[..boundary])
        );
        assert_eq!(
            weight.irreducible_message_bytes,
            measure_messages(&request.messages()[boundary..])
        );
    }
}
