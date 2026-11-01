use super::*;

#[tokio::test(start_paused = true)]
async fn turns_provider_errors_into_failed_events() {
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::new(Mutex::new(None)),
            fails: true,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events[0], RunEvent::Started);
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::ProviderTransport,
            message,
        }) if message.contains("offline")
    ));
}

fn overloaded() -> ProviderError {
    ProviderError::Api {
        status: 503,
        message: "provider overloaded".to_owned(),
    }
}

struct FailingProvider {
    calls: Arc<std::sync::atomic::AtomicU32>,
    failure: fn() -> Option<ProviderError>,
}

impl Provider for FailingProvider {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match (self.failure)() {
            Some(error) => Box::pin(stream::once(async move { Err(error) })),
            None => Box::pin(stream::iter(Vec::new())),
        }
    }
}

struct EmptyCompletionProvider {
    calls: Arc<std::sync::atomic::AtomicU32>,
    usage: Option<qq_provider::ProviderUsage>,
    prelude: Option<&'static str>,
}

impl Provider for EmptyCompletionProvider {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        let turn = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if turn == 0 {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "read".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "read".to_owned(),
                    json: r#"{"path":"note.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "read".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let usage = self.usage;
        let mut events = Vec::with_capacity(2);
        if let Some(text) = self.prelude {
            events.push(Ok(ProviderEvent::OutputTextDelta {
                text: text.to_owned(),
            }));
        }
        events.push(Ok(ProviderEvent::Completed { usage }));
        Box::pin(stream::iter(events))
    }
}

#[tokio::test(start_paused = true)]
async fn an_empty_completion_without_usage_is_a_transient_fault_not_an_answer() {
    let fast = TurnRecoveryPolicy::new(Duration::from_millis(1), Duration::from_millis(1));
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "note\n").unwrap();
    for prelude in [None, Some("\n"), Some(" \n\t")] {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            EmptyCompletionProvider {
                calls: Arc::clone(&calls),
                usage: None,
                prelude,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("read it")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            u32::from(MAX_TURN_RETRIES) + 2,
            "prelude {prelude:?}: the read turn, one empty reply, and every retry of it"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Completed { .. })),
            "prelude {prelude:?}: an empty reply must never settle completed: {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(RuntimeEvent::Paused { pause })
                    if matches!(pause.as_ref(), qq_protocol::RunPause::ProviderRetry { kind: RunFailureKind::ProviderTransport, message, .. } if message.contains("no content and no usage"))
            ),
            "prelude {prelude:?}: {events:?}"
        );
    }

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runtime = Runtime::new(
        EmptyCompletionProvider {
            calls: Arc::clone(&calls),
            usage: None,
            prelude: Some("ok"),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_turn_recovery(fast);
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("read it")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runtime = Runtime::new(
        EmptyCompletionProvider {
            calls: Arc::clone(&calls),
            usage: Some(qq_provider::ProviderUsage {
                input_tokens: 12,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: None,
            }),
            prelude: None,
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_turn_recovery(fast);
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("read it")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn credential_load_timeout_does_not_retry_the_turn() {
    for message in [
        "credential loading timed out",
        "credential loading capacity is exhausted",
    ] {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        struct CredentialFailure {
            calls: Arc<std::sync::atomic::AtomicU32>,
            message: &'static str,
        }
        impl Provider for CredentialFailure {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let message = self.message.to_owned();
                Box::pin(stream::once(async move {
                    Err(ProviderError::ResponseFailed {
                        kind: qq_provider::ProviderErrorKind::Authentication,
                        message,
                    })
                }))
            }
        }
        let runtime = Runtime::new(
            CredentialFailure {
                calls: Arc::clone(&calls),
                message,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(TurnRecoveryPolicy::new(
            Duration::from_millis(1),
            Duration::from_millis(1),
        ));
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Failed {
                    kind: RunFailureKind::ProviderAuthentication,
                    message: got,
                }) if got.contains(message) && !got.contains("paused after")
            ),
            "{events:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_empty_completion_after_tool_results_and_steering_is_still_a_fault() {
    let fast = TurnRecoveryPolicy::new(Duration::from_millis(1), Duration::from_millis(1));
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "note\n").unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runtime = Runtime::new(
        EmptyCompletionProvider {
            calls: Arc::clone(&calls),
            usage: None,
            prelude: None,
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_turn_recovery(fast);
    let (sender, receiver) = runtime::steering_channel();
    let message_id = qq_protocol::MessageId::from_bytes([7; 16]);
    sender
        .messages
        .send(runtime::SteeringMessage::text(message_id, "also summarize"))
        .await
        .unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("read it")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Ask)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_steering(receiver),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        events.iter().any(|event| matches!(event,
            RuntimeEvent::SteeringApplied { message_id: applied, .. } if *applied == message_id
        )),
        "steering must join after the tool result: {events:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        u32::from(MAX_TURN_RETRIES) + 2,
        "the read turn, one empty reply, and every retry of it"
    );
    assert!(
        matches!(
            events.last(),
            Some(RuntimeEvent::Paused { pause })
                if matches!(pause.as_ref(), qq_protocol::RunPause::ProviderRetry { kind: RunFailureKind::ProviderTransport, message, .. } if message.contains("no content and no usage"))
        ),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn transient_faults_retry_the_turn_and_exhaustion_pauses() {
    let fast = TurnRecoveryPolicy::new(Duration::from_millis(1), Duration::from_millis(1));
    type Case = (fn() -> Option<ProviderError>, RunFailureKind, &'static str);
    let transient: [Case; 3] = [
        (
            || Some(overloaded()),
            RunFailureKind::ProviderUnavailable,
            "provider overloaded",
        ),
        (
            || Some(ProviderError::Transport("offline".to_owned())),
            RunFailureKind::ProviderTransport,
            "offline",
        ),
        (
            || None,
            RunFailureKind::ProviderTransport,
            "ended without a terminal event",
        ),
    ];
    for (failure, kind, needle) in transient {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            FailingProvider {
                calls: Arc::clone(&calls),
                failure,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            u32::from(MAX_TURN_RETRIES) + 1,
            "{kind:?}: one send plus every retry"
        );
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Failed { kind: got, message })
                    if *got == kind
                        && message.contains(needle)
                        && message.contains("paused after 5 turn retries")
            ),
            "{kind:?}: {events:?}"
        );
    }

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runtime = Runtime::new(
        FailingProvider {
            calls: Arc::clone(&calls),
            failure: || {
                Some(ProviderError::Api {
                    status: 401,
                    message: "bad key".to_owned(),
                })
            },
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_turn_recovery(fast);
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::ProviderAuthentication,
            ..
        })
    ));

    struct RecoversMidStream {
        calls: Arc<std::sync::atomic::AtomicU32>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for RecoversMidStream {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            match call {
                0 | 1 => Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: format!("part{call} "),
                    }),
                    Err(ProviderError::Api {
                        status: 529,
                        message: "overloaded_error".to_owned(),
                    }),
                ])),
                _ => Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ])),
            }
        }
    }

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        RecoversMidStream {
            calls: Arc::clone(&calls),
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_turn_recovery(fast);
    let events = runtime
        .run(RunCommand::new("hello"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            RunEvent::OutputTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "part0 part1 done");
    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    let requests = requests.lock().unwrap();
    let third = requests[2].messages();
    assert!(matches!(
        third[third.len() - 1].content(),
        [ContentBlock::Text { text }] if text == TURN_RETRY_CONTINUE_NOTICE
    ));
    assert!(matches!(
        third[third.len() - 2].content(),
        [ContentBlock::Text { text }] if text == "part1 "
    ));
}

#[tokio::test(start_paused = true)]
async fn the_turn_retry_allowance_resets_on_a_completed_turn() {
    struct BlipEveryTurn {
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    impl Provider for BlipEveryTurn {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tool_results = request
                .messages()
                .iter()
                .flat_map(|message| message.content())
                .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
                .count();
            if call.is_multiple_of(2) {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "thinking".to_owned(),
                    }),
                    Err(ProviderError::Transport("blip".to_owned())),
                ]));
            }
            if tool_results >= 8 {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "finished".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let id = format!("call-{tool_results}");
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: r#"{"path":"note.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runtime = Runtime::new(
        BlipEveryTurn {
            calls: Arc::clone(&calls),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_turn_recovery(TurnRecoveryPolicy::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
    ));
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("read a lot")], directory.path().into())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 18);
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{:?}",
        events.last()
    );
    let retries = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::TurnRetrying { attempt: 1, .. }))
        .count();
    assert_eq!(retries, 9);
    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeEvent::TurnRetrying { attempt: 2.., .. } | RuntimeEvent::Paused { .. }
    )));
}

#[tokio::test(start_paused = true)]
async fn a_retry_sleep_yields_to_cancellation_and_the_deadline() {
    struct AlwaysBlips;
    impl Provider for AlwaysBlips {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "x".to_owned(),
                }),
                Err(ProviderError::Transport("blip".to_owned())),
            ]))
        }
    }
    struct AllowAllGate;
    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }
    let slow = TurnRecoveryPolicy::new(Duration::from_secs(60), Duration::from_secs(60));
    let directory = tempfile::tempdir().unwrap();

    let runtime = Runtime::new(AlwaysBlips, "gpt-test", 256)
        .unwrap()
        .with_turn_recovery(slow);
    let cancellation = RunCancellation::new();
    let canceller = cancellation.clone();
    let cancel = tokio::spawn(async move {
        // lint: allow(sleep) virtual: this test runs on paused time
        tokio::time::sleep(Duration::from_secs(5)).await;
        canceller.cancel();
    });
    let events = runtime
        .run_loop(
            vec![Message::user("go")],
            directory.path().to_owned(),
            cancellation,
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    cancel.await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnRetrying { attempt: 1, .. }))
    );
    assert!(
        matches!(events.last(), Some(RuntimeEvent::TurnRetrying { .. })),
        "{:?}",
        events.last()
    );

    let runtime = Runtime::new(AlwaysBlips, "gpt-test", 256)
        .unwrap()
        .with_turn_recovery(slow);
    let plan = Arc::new(
        LoadedRuntime::compile_blocking(
            &runtime,
            runtime.embedded_resolved_model(),
            directory.path().to_owned(),
        )
        .unwrap()
        .plan,
    );
    let mut capabilities = RunCapabilities::user(None);
    capabilities.limits = RunLimits {
        max_duration_ms: Some(10_000),
        ..RunLimits::default()
    };
    let started = tokio::time::Instant::now();
    let events = plan
        .execute(
            vec![Message::user("go")],
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
            capabilities,
        )
        .collect::<Vec<_>>()
        .await;
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(
        matches!(
            events.last(),
            Some(RuntimeEvent::BudgetExhausted { exhaustion })
                if exhaustion.limit == BudgetLimitKind::Duration
        ),
        "{:?}",
        events.last()
    );
}
