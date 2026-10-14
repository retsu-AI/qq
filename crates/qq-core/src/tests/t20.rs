use super::*;

struct ToolOnce {
    name: &'static str,
    arguments: String,
    turn: AtomicUsize,
}

impl Provider for ToolOnce {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        if self.turn.fetch_add(1, Ordering::SeqCst) != 0 {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".into(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        Box::pin(stream::iter([
            Ok(ProviderEvent::ToolCallStarted {
                id: "t20".into(),
                name: self.name.into(),
            }),
            Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: "t20".into(),
                json: self.arguments.clone(),
            }),
            Ok(ProviderEvent::ToolCallCompleted { id: "t20".into() }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct ExecuteGate;
impl ToolGate for ExecuteGate {
    fn resolve(&self, _: &RuntimeToolCall) -> ToolGateFuture {
        Box::pin(std::future::ready(GateDecision::Execute))
    }
}

struct NoSpillLookup;
impl runtime::SpillReader for NoSpillLookup {
    fn read(&self, _: runtime::SpillHandle) -> runtime::SpillReadFuture {
        panic!("placeholder must be corrected before store lookup")
    }
}

#[tokio::test]
async fn t20_placeholder_handle_is_a_dispatch_correction_without_a_store_lookup() {
    let workspace = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(
        ToolOnce {
            name: "read_tool_result",
            arguments: r#"{"handle":"t:shell:00000000:00000000"}"#.into(),
            turn: AtomicUsize::new(0),
        },
        "test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("read result")],
            workspace.path().into(),
            RunCancellation::new(),
            Arc::new(ExecuteGate),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_spills(Arc::new(NoSpillLookup)),
        )
        .collect::<Vec<_>>()
        .await;
    let result = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished {
                result,
                is_error: true,
                ..
            } => Some(result),
            _ => None,
        })
        .expect("placeholder correction");
    assert!(result.starts_with("handle_placeholder:"), "{result}");
    assert!(result.contains("copy the handle"), "{result}");
    assert_eq!(
        qq_protocol::ToolErrorKind::of("read_tool_result", result),
        qq_protocol::ToolErrorKind::Correction
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn t20_strict_refuses_only_unprefixed_contained_reads_and_keeps_hints() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("x")).unwrap();
    std::fs::write(workspace.path().join("a"), "alpha\n").unwrap();
    std::fs::write(workspace.path().join("x/a"), "alpha\n").unwrap();
    let absolute = format!("sed -n 1,5p {}", workspace.path().join("a").display());
    for (command, refused, hinted) in [
        ("sed -n 1,5p a", true, false),
        ("cd x && sed -n 1,5p a", false, true),
        (absolute.as_str(), false, true),
        ("cd missing; sed -n 1,5p a", false, false),
        ("cd /tmp && sed -n 1,5p log", false, false),
        ("cd 'x;grep y z' && echo ok", false, false),
        ("echo ok && grep x a", false, false),
        ("echo ok | grep x", false, false),
        ("sed -n 1,5p a && echo ok", false, true),
    ] {
        let runtime = Runtime::new(
            ToolOnce {
                name: "shell",
                arguments: serde_json::json!({"command": command}).to_string(),
                turn: AtomicUsize::new(0),
            },
            "test",
            256,
        )
        .unwrap()
        .with_shell_policy(runtime::ShellPolicy {
            env_allowlist: Arc::from([]),
            builtin_preference: runtime::BuiltinPreference::Strict,
        });
        let events = runtime
            .run_loop(
                vec![Message::user("read")],
                workspace.path().into(),
                RunCancellation::new(),
                Arc::new(ExecuteGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        let result = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ToolCallFinished { result, .. } => Some(result),
                _ => None,
            })
            .expect("shell result");
        assert_eq!(
            result.starts_with("use_builtin:"),
            refused,
            "{command}: {result}"
        );
        assert_eq!(result.contains("hint: use"), hinted, "{command}: {result}");
    }
}
