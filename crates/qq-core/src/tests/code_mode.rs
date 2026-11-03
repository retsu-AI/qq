use super::*;

fn run_code(code: &str) -> String {
    serde_json::json!({ "code": code }).to_string()
}

async fn run_script(
    code: &str,
    mode: ApprovalMode,
) -> (Vec<RuntimeEvent>, Vec<ModelRequest>, tempfile::TempDir) {
    run_script_in(CodeLanguage::Python, code, mode).await
}

async fn run_script_in(
    language: CodeLanguage,
    code: &str,
    mode: ApprovalMode,
) -> (Vec<RuntimeEvent>, Vec<ModelRequest>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "beta\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![vec![("run_code", run_code(code))], Vec::new()],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_code_language(language);
    let network = Arc::clone(&runtime.network);
    let events = runtime
        .run_loop(
            vec![Message::user("inspect")],
            std::fs::canonicalize(directory.path()).unwrap(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode,
                tools: std::collections::HashSet::new(),
                network,
            }),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    let requests = requests.lock().unwrap().clone();
    (events, requests, directory)
}

fn script_result(requests: &[ModelRequest]) -> (String, bool) {
    requests[1]
        .messages()
        .iter()
        .flat_map(Message::content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("the second request carries the run_code result")
}

#[tokio::test]
async fn the_model_sees_one_run_code_tool_that_declares_the_rest() {
    let (events, requests, _directory) = run_script("print('hi')", ApprovalMode::Auto).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    assert_eq!(tool_names(&requests[0]), ["run_code"]);
    let spec = &requests[0].tools()[0];
    assert!(spec.description().contains("async def read_file(path: str"));
    assert!(
        spec.description()
            .contains("async def run_program(program: str")
    );
    assert!(!spec.description().contains("async def run_code("));
    let system = requests[0].system().unwrap();
    assert!(system.contains("Available tools: run_code. Inside run_code scripts"));
    assert!(system.contains("Code mode:"));
    assert_eq!(script_result(&requests), ("hi\n".to_owned(), false));
}

#[tokio::test]
async fn gathered_reads_run_as_child_calls_and_return_their_text() {
    let (events, requests, _directory) = run_script(
        "import asyncio\na, b = await asyncio.gather(read_file('a.txt'), read_file(path='b.txt'))\nprint(a.splitlines()[-1])\nprint(b.splitlines()[-1])",
        ApprovalMode::Auto,
    )
    .await;
    let children: Vec<(&str, u16)> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ScriptCallRequested { call, seq, .. } => Some((call.name.as_str(), *seq)),
            _ => None,
        })
        .collect();
    assert_eq!(children, [("read_file", 0), ("read_file", 1)]);
    let started = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        .count();
    assert_eq!(started, 3);
    assert_eq!(
        script_result(&requests),
        ("1\talpha\n1\tbeta\n".to_owned(), false)
    );
}

#[tokio::test]
async fn a_denied_child_call_raises_permission_error_and_changes_nothing() {
    let (events, requests, directory) = run_script(
        "try:\n    await write_file(path='c.txt', content='x')\nexcept PermissionError as e:\n    print('denied')",
        ApprovalMode::ReadOnly,
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. })),
        "{events:?}"
    );
    assert!(!directory.path().join("c.txt").exists());
    assert_eq!(script_result(&requests), ("denied\n".to_owned(), false));
}

#[tokio::test]
async fn an_uncaught_exception_is_an_error_result_with_printed_output() {
    let (_, requests, _directory) = run_script(
        "print('before')\nawait read_file('missing.txt')",
        ApprovalMode::Auto,
    )
    .await;
    let (text, is_error) = script_result(&requests);
    assert!(is_error);
    assert!(text.starts_with("before\n[script raised]\n"), "{text}");
    assert!(text.contains("FileNotFoundError: path_not_found"), "{text}");
}

#[tokio::test]
async fn a_syntax_error_reports_that_the_script_did_not_start() {
    let (events, requests, _directory) = run_script("def f(:\n  pass", ApprovalMode::Auto).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ScriptCallRequested { .. }))
    );
    let (text, is_error) = script_result(&requests);
    assert!(is_error);
    assert!(text.starts_with("[script did not start]\n"), "{text}");
}

#[tokio::test]
async fn direct_tools_are_unknown_in_code_mode() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![("read_file", r#"{"path":"a.txt"}"#.to_owned())],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_code_language(CodeLanguage::Python);
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let (text, is_error) = script_result(&requests);
    assert!(is_error);
    assert_eq!(text, "unknown tool \"read_file\"");
}

struct StructuredHost(MockMcpRegistry);

impl ExternalToolHost for StructuredHost {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn catalog_blocking(&self) -> HostCatalog {
        self.0.catalog_blocking()
    }
    fn catalog_is_current(&self, generation: u64) -> bool {
        self.0.catalog_is_current(generation)
    }
    fn config_grants(&self) -> Vec<String> {
        self.0.config_grants()
    }
    fn call(&self, name: String, arguments: String, cancelled: RunCancellation) -> HostCallFuture {
        self.0.call(name, arguments, cancelled)
    }
    fn call_observed(
        &self,
        name: String,
        arguments: String,
        cancelled: RunCancellation,
    ) -> crate::HostCallObservation {
        let future = self.0.call(name, arguments, cancelled);
        crate::HostCallObservation::Observed(Box::pin(async move {
            future.await.map(|result| crate::ObservedHostToolResult {
                result,
                result_digest: None,
                structured: Some(serde_json::json!({"pong": 2, "tags": ["a", "b"]})),
            })
        }))
    }
    fn readiness(&self) -> HostReadiness {
        self.0.readiness()
    }
    fn shutdown(&self) -> HostShutdownFuture {
        self.0.shutdown()
    }
}

struct ExecuteAll;

impl ToolGate for ExecuteAll {
    fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
        Box::pin(std::future::ready(GateDecision::Execute))
    }
}

#[tokio::test]
async fn a_script_receives_mcp_structured_content_as_a_dict() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = MockMcpRegistry::returning(HostToolResult {
        content: "pong".to_owned(),
        is_error: false,
    });
    registry.grants = vec!["mcp__srv__ping".to_owned()];
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![(
                    "run_code",
                    run_code("r = await mcp__srv__ping()\nprint(r['pong'] + 1, r['tags'][1])"),
                )],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(StructuredHost(registry)))
    .with_code_language(CodeLanguage::Python);
    let events = runtime
        .run_loop(
            vec![Message::user("ping")],
            std::fs::canonicalize(directory.path()).unwrap(),
            RunCancellation::new(),
            Arc::new(ExecuteAll),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, .. } if result == "pong"
    )));
    let requests = requests.lock().unwrap();
    assert_eq!(script_result(&requests), ("3 b\n".to_owned(), false));
}

#[tokio::test]
async fn javascript_scripts_run_child_calls_and_see_denials() {
    let (events, requests, directory) = run_script_in(
        CodeLanguage::JavaScript,
        "const [a, b] = await Promise.all([read_file('a.txt'), read_file({ path: 'b.txt' })]);\nconsole.log(a.trim().split('\\n').pop(), b.trim().split('\\n').pop());\ntry { await write_file({ path: 'c.txt', content: 'x' }); } catch (e) { console.log(e.name); }",
        ApprovalMode::ReadOnly,
    )
    .await;
    let spec = &requests[0].tools()[0];
    assert!(
        spec.description().contains("declare function read_file("),
        "{}",
        spec.description()
    );
    assert!(!requests[0].system().unwrap().contains("asyncio"));
    let children = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ScriptCallRequested { .. }))
        .count();
    assert_eq!(children, 3);
    assert!(!directory.path().join("c.txt").exists());
    assert_eq!(
        script_result(&requests),
        ("1\talpha 1\tbeta\nPermissionError\n".to_owned(), false)
    );
}

async fn run_with_capabilities(
    code: &str,
    gate: Arc<dyn ToolGate>,
    capabilities: RunCapabilities,
) -> (Vec<RuntimeEvent>, Vec<ModelRequest>) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![vec![("run_code", run_code(code))], Vec::new()],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_code_language(CodeLanguage::Python);
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("inspect")],
            std::fs::canonicalize(directory.path()).unwrap(),
            RunCancellation::new(),
            gate,
            Arc::new(workspace::FileState::default()),
            capabilities,
        )
        .collect::<Vec<_>>()
        .await;
    let requests = requests.lock().unwrap().clone();
    (events, requests)
}

fn child_count(events: &[RuntimeEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ScriptCallRequested { .. }))
        .count()
}

#[tokio::test]
async fn a_script_stops_when_the_run_tool_call_budget_is_spent() {
    let limits = qq_protocol::RunLimits {
        max_tool_calls: Some(20),
        ..qq_protocol::RunLimits::default()
    };
    let (events, requests) = run_with_capabilities(
        "for i in range(40):\n    await search(query='q' + str(i))\nprint('never')",
        Arc::new(ExecuteAll),
        RunCapabilities::user(None).with_limits(limits, None),
    )
    .await;
    assert_eq!(child_count(&events), 19, "{events:?}");
    let (text, is_error) = script_result(&requests);
    assert!(is_error);
    assert!(
        text.contains("[script stopped at a limit] the run's tool_calls budget is spent"),
        "{text}"
    );
}

#[tokio::test]
async fn identical_repeated_child_calls_are_refused_by_the_loop_guard() {
    let (events, requests) = run_with_capabilities(
        "n = 0\nfor i in range(10):\n    try:\n        await read_file('a.txt')\n    except RuntimeError:\n        n += 1\nprint('refused', n)",
        Arc::new(ExecuteAll),
        RunCapabilities::user(None),
    )
    .await;
    assert_eq!(child_count(&events), 4, "{events:?}");
    assert_eq!(script_result(&requests), ("refused 6\n".to_owned(), false));
}

struct NeverAnswers(tokio::sync::mpsc::Sender<()>);

impl ToolGate for NeverAnswers {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        if call.name == "run_code" {
            return Box::pin(std::future::ready(GateDecision::Execute));
        }
        let reached = self.0.clone();
        Box::pin(async move {
            let _ = reached.send(()).await;
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn an_interrupt_while_a_child_awaits_approval_ends_the_script() {
    let (sender, receiver) = runtime::steering_channel();
    let (reached_sender, mut reached) = tokio::sync::mpsc::channel(1);
    let gate = Arc::new(NeverAnswers(reached_sender));
    let run = tokio::spawn(run_with_capabilities(
        "await read_file('a.txt')\nprint('never')",
        gate,
        RunCapabilities::user(None).with_steering(receiver),
    ));
    reached.recv().await.unwrap();
    sender.interrupt();
    let (events, _) = run.await.unwrap();
    let interrupted: Vec<bool> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallUnexecuted { interrupted, .. } => Some(*interrupted),
            _ => None,
        })
        .collect();
    assert_eq!(interrupted, [true], "{events:?}");
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result.contains("[script raised]")
        )),
        "{events:?}"
    );
}
