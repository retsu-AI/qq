use super::*;

fn run_code(code: &str) -> String {
    serde_json::json!({ "code": code }).to_string()
}

async fn run_script(
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
    .with_code_language(CodeLanguage::Python);
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
