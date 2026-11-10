use super::*;

struct McpCallProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for McpCallProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        if turn == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: "mcp__srv__ping".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0".to_owned(),
                    json: r#"{"value":1}"#.to_owned(),
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

#[tokio::test]
async fn merges_mcp_declarations_and_dispatches_granted_calls_to_the_registry() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = MockMcpRegistry::returning(HostToolResult {
        content: "pong".to_owned(),
        is_error: false,
        images: Vec::new(),
    });
    registry.specs.push(qq_provider::ToolSpec::new(
        "rogue_tool",
        "not namespaced",
        serde_json::json!({"type": "object"}),
    ));
    registry.grants = vec!["mcp__srv__ping".to_owned()];
    let calls = Arc::clone(&registry.calls);
    let runtime = Runtime::new(
        McpCallProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(registry));

    let events = runtime
        .run_messages_in_workspace(vec![Message::user("ping")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, is_error: false, .. } if result == "pong"
    )));
    let requests = requests.lock().unwrap();
    let names = requests[0]
        .tools()
        .iter()
        .map(qq_provider::ToolSpec::name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"mcp__srv__ping"));
    assert!(
        !names.contains(&"rogue_tool"),
        "specs outside the mcp__ namespace must be discarded"
    );
    assert_eq!(names, ["read_file", "tree", "search", "mcp__srv__ping"]);
    let system = requests[0].system().unwrap();
    assert!(system.contains("mcp__srv__ping"));
    assert!(system.contains("external tool hosts"));
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult {
            call_id,
            content,
            is_error: false,
        }] if call_id == "call_0" && content == "pong"
    ));
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [("mcp__srv__ping".to_owned(), r#"{"value":1}"#.to_owned())]
    );
}

#[tokio::test]
async fn mcp_failures_are_tool_errors_and_results_are_truncated() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let registry = MockMcpRegistry::returning(HostToolResult {
        content: "the server exploded".to_owned(),
        is_error: true,
        images: Vec::new(),
    });
    let runtime = Runtime::new(
        McpCallProvider {
            requests: Arc::new(Mutex::new(Vec::new())),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(registry));
    let events = runtime
        .run_loop(
            vec![Message::user("ping")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
            if result == "the server exploded"
    )));
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "an MCP failure must never fail the run"
    );

    let oversized = format!("{}\n", "x".repeat(1_023)).repeat(200);
    let registry = MockMcpRegistry::returning(HostToolResult {
        content: oversized,
        is_error: false,
        images: Vec::new(),
    });
    let runtime = Runtime::new(
        McpCallProvider {
            requests: Arc::new(Mutex::new(Vec::new())),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(registry));
    let events = runtime
        .run_loop(
            vec![Message::user("ping")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    let result = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the oversized MCP result must still finish");
    assert!(result.len() <= tools::MAX_MODEL_TEXT_BYTES);
    assert!(result.contains("…[qq: "));
}

#[tokio::test]
async fn ungranted_mcp_calls_are_denied_unattended_and_unknown_names_error() {
    let directory = tempfile::tempdir().unwrap();
    let registry = MockMcpRegistry::returning(HostToolResult {
        content: "pong".to_owned(),
        is_error: false,
        images: Vec::new(),
    });
    let runtime = Runtime::new(
        McpCallProvider {
            requests: Arc::new(Mutex::new(Vec::new())),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(registry));
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("ping")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallDenied { message, .. }
            if message == approval::UNATTENDED_DENIED_RESULT
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));

    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    let runtime = Runtime::new(
        McpCallProvider {
            requests: Arc::new(Mutex::new(Vec::new())),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("ping")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallUnexecuted { message: result, interrupted: false, .. }
            if result.contains("unknown tool")
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[derive(Default)]
struct RecordingImages(Mutex<std::collections::HashMap<String, Vec<u8>>>);

impl runtime::SpillReader for RecordingImages {
    fn read(&self, _: runtime::SpillHandle) -> runtime::SpillReadFuture {
        Box::pin(std::future::ready(Ok(runtime::SpillRead::Missing)))
    }

    fn put_tool_image(&self, sha256: String, bytes: Vec<u8>) -> runtime::ToolImageFuture<()> {
        self.0.lock().unwrap().insert(sha256, bytes);
        Box::pin(std::future::ready(Ok(())))
    }

    fn read_tool_image(&self, sha256: String) -> runtime::ToolImageFuture<Option<Vec<u8>>> {
        let found = self.0.lock().unwrap().get(&sha256).cloned();
        Box::pin(std::future::ready(Ok(found)))
    }
}

#[tokio::test]
async fn a_host_image_reaches_the_next_model_request_but_not_history() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sha256 = "b".repeat(64);
    let marker = qq_provider::tool_image_marker("image/png", &sha256);
    let mut registry = MockMcpRegistry::returning(HostToolResult {
        content: marker.clone(),
        is_error: false,
        images: vec![HostToolImage {
            sha256,
            bytes: b"ABC".to_vec(),
        }],
    });
    registry.grants = vec!["mcp__srv__ping".to_owned()];
    let runtime = Runtime::new(
        McpCallProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(registry));
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("ping")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::default(),
            RunCapabilities::user(None).with_spills(Arc::new(RecordingImages::default())),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, .. } if *result == marker
    )));
    let requests = requests.lock().unwrap();
    assert!(matches!(
        requests[1].messages()[2].content(),
        [
            ContentBlock::ToolResult { content, .. },
            ContentBlock::Image { media_type, data },
        ] if *content == marker && media_type == "image/png" && &**data == "QUJD"
    ));
}
