use super::*;

struct TruncatingProvider {
    truncations: usize,
    cut_tool_call: bool,
    empty: bool,
    empty_from: usize,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

struct CallThenCutProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    then_empty: bool,
}

impl Provider for CallThenCutProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        if turn == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call".to_owned(),
                    json: r#"{"path":"AGENTS.md"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "call".to_owned(),
                }),
                Ok(ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                }),
            ]))
        } else if self.then_empty {
            Box::pin(stream::iter([Ok(ProviderEvent::Incomplete {
                usage: None,
                reason: qq_provider::IncompleteReason::OutputTokens,
            })]))
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

impl Provider for TruncatingProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        let usage = Some(qq_provider::ProviderUsage {
            input_tokens: 10,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 7,
            reasoning_tokens: None,
        });
        if turn < self.truncations {
            let mut events = if self.empty || turn >= self.empty_from {
                Vec::new()
            } else {
                vec![Ok(ProviderEvent::OutputTextDelta {
                    text: format!("part{turn} "),
                })]
            };
            if turn == 0 && self.cut_tool_call {
                events.push(Ok(ProviderEvent::ToolCallStarted {
                    id: "cut".to_owned(),
                    name: "read_file".to_owned(),
                }));
                events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "cut".to_owned(),
                    json: r#"{"path":"AGEN"#.to_owned(),
                }));
            }
            events.push(Ok(ProviderEvent::Incomplete {
                usage,
                reason: qq_provider::IncompleteReason::OutputTokens,
            }));
            Box::pin(stream::iter(events))
        } else {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "end".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage }),
            ]))
        }
    }
}

#[tokio::test]
async fn truncated_turns_are_committed_and_continued_within_the_cap() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: 2,
            cut_tool_call: true,
            empty: false,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("write a long answer")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    let turns = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::AssistantTurnCompleted {
                turn_ordinal,
                calls,
                truncated,
                ..
            } => Some((*turn_ordinal, calls.len(), *truncated)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(turns, vec![(1, 0, true), (2, 0, true), (3, 0, false)]);
    let continuations = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::OutputTruncated {
                turn_ordinal,
                continuation,
            } => Some((*turn_ordinal, *continuation)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(continuations, vec![(1, 1), (2, 2)]);
    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallStarted { .. } | RuntimeEvent::ToolCallDenied { .. }
    )));

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let transcript = requests[2]
        .messages()
        .iter()
        .map(|message| {
            let text = match message.content().first() {
                Some(ContentBlock::Text { text }) => text.as_str(),
                _ => "",
            };
            (message.role(), text)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transcript,
        vec![
            (Role::User, "write a long answer"),
            (Role::Assistant, "part0 "),
            (Role::User, OUTPUT_TRUNCATED_CONTINUE_NOTICE),
            (Role::Assistant, "part1 "),
            (Role::User, OUTPUT_TRUNCATED_CONTINUE_NOTICE),
        ]
    );
    assert!(!requests[2].tools().is_empty());
}

#[tokio::test]
async fn truncation_past_the_cap_settles_with_a_typed_failure() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: false,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("write a long answer")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    let expected_turns = usize::from(MAX_OUTPUT_CONTINUATIONS) + 1;
    assert_eq!(requests.lock().unwrap().len(), expected_turns);
    let committed = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::AssistantTurnCompleted {
                    truncated: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!(committed, expected_turns, "every partial turn is durable");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::OutputTruncated { .. }))
            .count(),
        usize::from(MAX_OUTPUT_CONTINUATIONS)
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderOutputTruncated,
            message,
        }) if message.contains("256 tokens") && message.contains("4 consecutive turns")
    ));
}

#[tokio::test]
async fn an_empty_truncated_turn_raises_the_cap_once_then_completes() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: 1,
            cut_tool_call: false,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        1024,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("think hard")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_max_output_tokens(256),
        )
        .collect::<Vec<_>>()
        .await;

    let caps = requests
        .lock()
        .unwrap()
        .iter()
        .map(ModelRequest::max_output_tokens)
        .collect::<Vec<_>>();
    assert_eq!(caps, vec![256, 512], "the retry carries a larger cap");
    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::OutputTruncated {
            continuation: 1,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantTurnCompleted {
            truncated: true,
            ..
        }
    )));
    let requests = requests.lock().unwrap();
    assert_eq!(requests[1].messages().len(), 1);
}

#[tokio::test]
async fn an_empty_truncation_after_the_continuation_cap_does_not_spend_another_turn() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: false,
            empty_from: usize::from(MAX_OUTPUT_CONTINUATIONS),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        1024,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("write a long answer")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_max_output_tokens(256),
        )
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        requests.lock().unwrap().len(),
        usize::from(MAX_OUTPUT_CONTINUATIONS) + 1
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        RuntimeEvent::OutputTruncated { continuation, .. }
            if *continuation > MAX_OUTPUT_CONTINUATIONS
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderOutputTruncated,
            ..
        })
    ));
}

#[tokio::test]
async fn a_tool_call_cut_mid_arguments_with_no_text_is_an_empty_truncation() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: true,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("think hard")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderOutputTruncated,
            message,
        }) if message.contains("without producing any visible output")
    ));
}

#[tokio::test]
async fn a_summarizer_turn_spent_on_reasoning_fails_without_a_notice() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(&runtime, workspace))
            .unwrap()
    })
    .await
    .unwrap();
    let events = plan
        .execute(
            vec![Message::user(sessions::COMPACTION_INSTRUCTION)],
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
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderOutputTruncated,
            ..
        })
    ));
}

#[tokio::test]
async fn an_empty_truncated_turn_at_the_ceiling_retries_with_a_notice_then_names_the_cause() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("think hard")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert_eq!(
            request.messages().last(),
            Some(&Message::user(REASONING_TRUNCATED_NOTICE))
        );
    }
    assert_eq!(requests[3].messages().len(), 4);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderOutputTruncated,
            message,
        }) if message.contains("without producing any visible output")
            && message.contains("reasoning_effort")
            && message.contains("on 4 consecutive turns")
            && message.contains("model ceiling 256")
    ));
}
#[tokio::test]
async fn a_truncated_turn_that_streamed_a_tool_call_is_continued_not_failed() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        CallThenCutProvider {
            requests: Arc::clone(&requests),
            then_empty: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::OutputTruncated {
            continuation: 1,
            ..
        }
    )));
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_session_shaped_run_raises_an_empty_truncated_cap_toward_the_catalog_ceiling() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: 1,
            cut_tool_call: false,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_output_ceiling(Some(OutputCeiling {
        tokens: 4096,
        policy_bound: false,
    }));
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("think hard")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;
    let caps = requests
        .lock()
        .unwrap()
        .iter()
        .map(ModelRequest::max_output_tokens)
        .collect::<Vec<_>>();
    assert_eq!(caps, vec![256, 512]);
    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_output_ceiling(Some(OutputCeiling {
        tokens: 4096,
        policy_bound: false,
    }));
    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("think hard")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        requests.lock().unwrap().len(),
        4,
        "one raise, two nudged retries, then settle"
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderOutputTruncated,
            message,
        }) if message.contains("(512 tokens)") && message.contains("model ceiling 4096")
    ));
}

#[tokio::test]
async fn a_tool_call_then_cut_turn_changes_the_resend_when_it_can() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        CallThenCutProvider {
            requests: Arc::clone(&requests),
            then_empty: false,
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_output_ceiling(Some(OutputCeiling {
        tokens: 4096,
        policy_bound: false,
    }));
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].max_output_tokens(), 256);
    assert_eq!(requests[1].max_output_tokens(), 512, "the resend differs");
    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );
}

#[tokio::test]
async fn the_reasoning_diagnostic_counts_only_turns_that_produced_nothing() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        CallThenCutProvider {
            requests: Arc::clone(&requests),
            then_empty: true,
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_output_ceiling(Some(OutputCeiling {
        tokens: 512,
        policy_bound: false,
    }));
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(requests.lock().unwrap().len(), 4);
    let Some(RuntimeEvent::Failed { kind, message }) = events.last() else {
        panic!("expected a failure, got {:?}", events.last());
    };
    assert_eq!(*kind, RunFailureKind::ProviderOutputTruncated);
    assert!(message.contains("on 3 consecutive turns;"), "{message}");
    assert!(message.contains("(512 tokens)"), "{message}");
}

struct EmptyThenToolThenEmptyProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for EmptyThenToolThenEmptyProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        let cut = || {
            Ok(ProviderEvent::Incomplete {
                usage: None,
                reason: qq_provider::IncompleteReason::OutputTokens,
            })
        };
        if turn == 1 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "read".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "read".to_owned(),
                    json: r#"{"path":"notes.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "read".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        } else {
            Box::pin(stream::iter([cut()]))
        }
    }
}

#[tokio::test]
async fn a_completed_tool_turn_resets_the_reasoning_only_streak() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        EmptyThenToolThenEmptyProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_output_ceiling(Some(OutputCeiling {
        tokens: 512,
        policy_bound: false,
    }));
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "hello").unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(requests.lock().unwrap().len(), 6);
    let Some(RuntimeEvent::Failed { kind, message }) = events.last() else {
        panic!("expected a failure, got {:?}", events.last());
    };
    assert_eq!(*kind, RunFailureKind::ProviderOutputTruncated);
    assert!(message.contains("on 4 consecutive turns;"), "{message}");
}

#[tokio::test]
async fn a_policy_bound_ceiling_names_the_policy_not_max_output_tokens() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: true,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_output_ceiling(Some(OutputCeiling {
        tokens: 512,
        policy_bound: true,
    }));
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    let Some(RuntimeEvent::Failed { message, .. }) = events.last() else {
        panic!("expected a failure, got {:?}", events.last());
    };
    assert!(
        message.contains("Managed policy caps output at 512"),
        "{message}"
    );
    assert!(message.contains("policy.max_output_tokens"), "{message}");
    assert!(!message.contains("model ceiling"), "{message}");
}

#[tokio::test(start_paused = true)]
async fn completed_limited_stream_stays_finished_when_polled_after_its_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::new(Mutex::new(None)),
            fails: false,
        },
        "test-model",
        256,
    )
    .unwrap();
    let mut events = runtime.run_loop_with_spawner(
        vec![Message::user("finish")],
        directory.path().to_owned(),
        RunCancellation::new(),
        Arc::new(StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
        Arc::new(workspace::FileState::default()),
        RunCapabilities::user(None).with_limits(
            RunLimits {
                max_duration_ms: Some(500),
                ..RunLimits::default()
            },
            None,
        ),
    );
    loop {
        match events.next().await.expect("completion") {
            RuntimeEvent::Completed { .. } => break,
            RuntimeEvent::Failed { message, .. } => panic!("{message}"),
            RuntimeEvent::BudgetExhausted { .. } => panic!("expired before completion"),
            _ => {}
        }
    }
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(
        events.next().await.is_none(),
        "a finished stream must not produce a second terminal"
    );
}

#[tokio::test]
async fn a_tool_call_on_the_budget_final_turn_settles_without_running() {
    struct AlwaysCalls {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for AlwaysCalls {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let id = format!("call-{}", requests.len());
            requests.push(request);
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "__test_mutate".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: "{}".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        AlwaysCalls {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("work")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Auto)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_limits(
                RunLimits {
                    max_model_turns: Some(2),
                    ..RunLimits::default()
                },
                None,
            ),
        )
        .collect::<Vec<_>>()
        .await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tool_choice(), qq_provider::ToolChoice::None);
    assert_eq!(requests[1].tools(), requests[0].tools());
    let started = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        .count();
    assert_eq!(started, 1, "only the first turn's call runs: {events:?}");
    let turns = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::AssistantTurnCompleted { .. }))
        .count();
    assert_eq!(turns, 1, "the final turn's call is never committed");
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::BudgetExhausted { exhaustion })
            if exhaustion.limit == BudgetLimitKind::ModelTurns && !exhaustion.final_response
    ));
}

#[tokio::test]
async fn a_truncated_budget_final_turn_settles_as_exhausted_not_continued() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TruncatingProvider {
            truncations: usize::MAX,
            cut_tool_call: false,
            empty: false,
            empty_from: usize::MAX,
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let limits = RunLimits {
        max_model_turns: Some(1),
        ..RunLimits::default()
    };
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("write a long answer")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Ask)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_limits(limits, None),
        )
        .collect::<Vec<_>>()
        .await;

    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantTurnCompleted {
            truncated: true,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::OutputTruncated { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::BudgetExhausted { exhaustion })
            if exhaustion.limit == BudgetLimitKind::ModelTurns
    ));
}

#[tokio::test]
async fn a_paused_turn_with_no_text_resumes_from_the_original_prompt() {
    struct PausingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for PausingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([Ok(ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::Paused,
                })]))
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

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        PausingProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::OutputTruncated {
            continuation: 1,
            ..
        }
    )));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages().len(), 1);
    assert_eq!(requests[1].messages()[0].role(), Role::User);
}

#[tokio::test]
async fn prior_empty_assistant_turns_do_not_block_a_follow_up() {
    struct CompletingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for CompletingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            assert!(
                request.messages().iter().all(Message::has_content),
                "empty reconstructed turns must not reach the provider: {:?}",
                request.messages()
            );
            self.requests.lock().unwrap().push(request);
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "ok".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        CompletingProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_messages_in_workspace(
            vec![
                Message::user("hello"),
                Message::assistant(""),
                Message::user("continue"),
            ],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events.last(),
        Some(&RuntimeEvent::Completed { final_output: None })
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        RuntimeEvent::Failed { message, .. }
            if message.contains("must not be empty")
    )));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].messages(),
        [
            Message::user("hello"),
            Message::assistant(EMPTY_TURN_PLACEHOLDER),
            Message::user("continue"),
        ]
    );
}

#[tokio::test]
async fn content_filter_stops_still_fail_as_provider_response() {
    struct FilteredProvider;

    impl Provider for FilteredProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "partial".to_owned(),
                }),
                Err(ProviderError::ResponseIncomplete(
                    "content_filter".to_owned(),
                )),
            ]))
        }
    }

    let runtime = Runtime::new(FilteredProvider, "gpt-test", 256).unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::ProviderResponse,
            ..
        })
    ));
}

#[tokio::test]
async fn rejects_oversized_provider_tool_metadata() {
    struct OversizedMetadataProvider;

    impl Provider for OversizedMetadataProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Ok(ProviderEvent::ToolCallStarted {
                id: "x".repeat(MAX_TOOL_CALL_ID_BYTES + 1),
                name: "read_file".to_owned(),
            })]))
        }
    }

    let runtime = Runtime::new(OversizedMetadataProvider, "gpt-test", 256).unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events[0], RunEvent::Started);
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::ProviderProtocol,
            ..
        })
    ));
}

#[tokio::test]
async fn a_toolless_request_rejects_provider_tool_calls_before_a_second_poll() {
    struct ToolOnToollessProvider {
        calls: Arc<AtomicUsize>,
    }

    impl Provider for ToolOnToollessProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "unexpected".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "unexpected".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    struct DenyAllGate;

    impl ToolGate for DenyAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Deny {
                message: "tools disabled".to_owned(),
            }))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new(
        ToolOnToollessProvider {
            calls: Arc::clone(&calls),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("summarize")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(DenyAllGate),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).without_tools(),
        )
        .collect::<Vec<_>>()
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Failed {
            kind: RunFailureKind::ProviderProtocol,
            message,
        }) if message.contains("declared no tools")
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallStarted { .. }
            | RuntimeEvent::ToolCallDenied { .. }
            | RuntimeEvent::ToolCallFinished { .. }
    )));
}

#[tokio::test]
async fn bounds_model_text_across_the_entire_tool_loop() {
    struct OversizedTextProvider;

    impl Provider for OversizedTextProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Ok(ProviderEvent::OutputTextDelta {
                text: "x".repeat(MAX_RUN_MODEL_TEXT_BYTES + 1),
            })]))
        }
    }

    let runtime = Runtime::new(OversizedTextProvider, "gpt-test", 256).unwrap();
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events[0], RunEvent::Started);
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::Policy,
            ..
        })
    ));
}
