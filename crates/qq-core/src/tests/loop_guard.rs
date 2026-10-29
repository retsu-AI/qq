//! AC4 runtime acceptance: admission decisions use actual ordered results.
use super::*;
use crate::runtime::STALL_REPORT_CALLS;

struct ApprovedFixtureGate;
impl ToolGate for ApprovedFixtureGate {
    fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
        Box::pin(std::future::ready(GateDecision::Execute))
    }
}

struct RepeatingCalls {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    tool: &'static str,
    arguments: &'static str,
    calls_per_turn: usize,
    ordinary_turns: usize,
    turns: Mutex<usize>,
}

impl Provider for RepeatingCalls {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let requested_report = request.messages().last().is_some_and(|message| {
            message.role() == Role::User && message.content().iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text == STALL_REPORT_NOTICE || text == SLICE_CHECKPOINT_NOTICE || text == ROOT_STALL_REPORT_NOTICE)
            })
        });
        self.requests.lock().unwrap().push(request);
        if requested_report {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "still inspecting".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let mut turns = self.turns.lock().unwrap();
        let turn = *turns;
        *turns += 1;
        drop(turns);
        if turn >= self.ordinary_turns {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let mut events = Vec::new();
        for ordinal in 0..self.calls_per_turn {
            let id = format!("repeat-{turn}-{ordinal}");
            events.extend([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: self.tool.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: self.arguments.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
            ]);
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn ac4_runtime_refuses_third_identical_error_in_the_same_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "present\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::clone(&requests),
            tool: "shell",
            arguments: r#"{"command":"printf x >> attempts.txt; printf error; exit 1"}"#,
            calls_per_turn: 3,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("inspect")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(ApprovedFixtureGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        std::fs::read(directory.path().join("attempts.txt")).unwrap(),
        b"xx",
        "only the first two shell calls may actually run"
    );
    let started = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        .count();
    assert_eq!(
        started, 2,
        "the third identical error must be refused before execution"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
            .count(),
        1
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let results = requests[1]
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content, is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|(_, error)| **error));
    assert!(results[2].0.contains("not executed"));
}

#[cfg(unix)]
#[tokio::test]
async fn ac4_process_work_receipts_reset_repeated_successes() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "present\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::clone(&requests),
            tool: "shell",
            arguments: r#"{"command":"cat note.txt"}"#,
            calls_per_turn: 6,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("work")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(ApprovedFixtureGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        6,
        "executor work receipts must reset the guard even for lexically read-only commands"
    );
    assert_eq!(events.iter().filter(|event| matches!(event, RuntimeEvent::ToolCallFinished { execution_context: Some(receipt), is_error: false, .. } if receipt.is_work())).count(), 6);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[tokio::test]
async fn ac4_runtime_refuses_fifth_identical_success_in_the_same_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "present\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::clone(&requests),
            tool: "read_file",
            arguments: r#"{"path":"note.txt"}"#,
            calls_per_turn: 5,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
            .count(),
        1
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let results = requests[1]
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content, is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 5);
    assert!(
        results[..4]
            .iter()
            .all(|(content, error)| !**error && content.ends_with("present\n"))
    );
    assert!(*results[4].1 && results[4].0.contains("not executed"));
}

#[tokio::test]
async fn ac4_runtime_pauses_after_512_quiet_attempts_despite_requested_reports() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::clone(&requests),
            tool: "read_file",
            arguments: r#"{"path":"missing.txt"}"#,
            calls_per_turn: MAX_TOOL_CALLS_PER_TURN,
            ordinary_turns: 40,
            turns: Mutex::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(30),
        runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert!(requests.lock().unwrap().iter().any(|request| request.messages().iter().any(|message| {
        matches!(message.content(), [ContentBlock::Text { text }] if text == STALL_REPORT_NOTICE || text == SLICE_CHECKPOINT_NOTICE || text == ROOT_STALL_REPORT_NOTICE)
    })), "the fixture must actually answer a requested report");
    assert!(
        matches!(
            events.last(),
            Some(RuntimeEvent::Paused { pause })
                if matches!(pause.as_ref(), qq_protocol::RunPause::NoProgress { calls: 513, .. })
        ),
        "expected exact512quiet attempts after one novel warm-up, got {:?}",
        events.last()
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        2
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Completed { .. }))
    );
}

struct SequenceCalls {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    turns: Vec<Vec<(&'static str, String)>>,
    current: Mutex<usize>,
}

impl Provider for SequenceCalls {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let report = request.messages().last().is_some_and(|message| matches!(message.content(), [ContentBlock::Text { text }] if text == STALL_REPORT_NOTICE || text == SLICE_CHECKPOINT_NOTICE || text == ROOT_STALL_REPORT_NOTICE));
        self.requests.lock().unwrap().push(request);
        if report {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "report: still inspecting".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let mut current = self.current.lock().unwrap();
        let turn = *current;
        *current += 1;
        drop(current);
        let Some(calls) = self.turns.get(turn) else {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        };
        let mut events = Vec::new();
        for (index, (tool, arguments)) in calls.iter().enumerate() {
            let id = format!("sequence-{turn}-{index}");
            events.extend([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: (*tool).to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: arguments.clone(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
            ]);
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ac4_verified_process_reads_keep_repeated_observations() {
    use sha2::Digest as _;
    use std::os::unix::fs::PermissionsExt as _;
    if std::env::var_os("QQ_CONTEXT_GUARD_WORKER").is_none() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::write(
            root.join("grep"),
            crate::tools::process_context::test_read_image(0),
        )
        .unwrap();
        std::fs::set_permissions(root.join("grep"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::loop_guard::ac4_verified_process_reads_keep_repeated_observations",
                "--nocapture",
            ])
            .env("QQ_CONTEXT_GUARD_WORKER", "1")
            .env("QQ_CONTEXT_GUARD_ROOT", &root)
            .env("PATH", &root)
            .env("LANG", "C")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let root = PathBuf::from(std::env::var_os("QQ_CONTEXT_GUARD_ROOT").unwrap());
    let images = crate::ApprovedReadImages::new(vec![crate::ApprovedReadImage {
        family: crate::ReadImageFamily::Grep,
        sha256: qq_protocol::ContentHash::from_bytes(
            sha2::Sha256::digest(crate::tools::process_context::test_read_image(0)).into(),
        ),
        contract: crate::ReadImageContract::SelfContainedReadV1,
    }])
    .unwrap();
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::new(Mutex::new(Vec::new())),
            tool: "exec",
            arguments: r#"{"program":"grep","args":["x","f"]}"#,
            calls_per_turn: 5,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test",
        256,
    )
    .unwrap()
    .with_approved_read_images(images);
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let events = tokio.block_on(async {
        runtime
            .run_loop(
                vec![Message::user("inspect")],
                root,
                RunCancellation::new(),
                Arc::new(ApprovedFixtureGate),
                Arc::default(),
            )
            .collect::<Vec<_>>()
            .await
    });
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| matches!(event, RuntimeEvent::ToolCallFinished { execution_context: Some(receipt), .. } if receipt.valid() && !receipt.is_work())).count(), 4);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[tokio::test]
async fn ac4_runtime_mutation_allows_the_same_read_after_four_results() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "old\n").unwrap();
    let mut calls = vec![("read_file", r#"{"path":"note.txt"}"#.to_owned()); 4];
    calls.push((
        "write_file",
        r#"{"path":"note.txt","content":"new\n"}"#.to_owned(),
    ));
    calls.push(("read_file", r#"{"path":"note.txt"}"#.to_owned()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        SequenceCalls {
            requests: Arc::clone(&requests),
            turns: vec![calls],
            current: Mutex::new(0),
        },
        "test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("inspect")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(ApprovedFixtureGate),
            Arc::default(),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        6
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
    );
    let requests = requests.lock().unwrap();
    let results = requests[1]
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content, is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 6);
    assert!(results.iter().all(|(_, error)| !**error));
    assert!(results[5].0.ends_with("new\n"));
}

#[tokio::test]
async fn ac4_runtime_alternating_stable_reads_pause_after_two_novel_results() {
    let directory = tempfile::tempdir().unwrap();
    for path in ["a.txt", "b.txt"] {
        std::fs::write(directory.path().join(path), "same\n").unwrap();
    }
    let calls = (0..640)
        .map(|index| {
            (
                "read_file",
                format!(
                    "{{\"path\":\"{}.txt\"}}",
                    if index % 2 == 0 { "a" } else { "b" }
                ),
            )
        })
        .collect::<Vec<_>>();
    let turns = calls
        .chunks(MAX_TOOL_CALLS_PER_TURN)
        .map(|chunk| chunk.to_vec())
        .collect();
    let runtime = Runtime::new(
        SequenceCalls {
            requests: Arc::new(Mutex::new(Vec::new())),
            turns,
            current: Mutex::new(0),
        },
        "test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Paused {pause}) if matches!(pause.as_ref(), qq_protocol::RunPause::NoProgress {calls:514,..}))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        514
    );
}

#[tokio::test]
async fn ac4_runtime_1500_novel_reads_complete_and_keep_reporting() {
    let directory = tempfile::tempdir().unwrap();
    let calls = (0..1500)
        .map(|index| {
            let path = format!("note-{index}.txt");
            std::fs::write(
                directory.path().join(&path),
                format!("observation {index}\n"),
            )
            .unwrap();
            ("read_file", serde_json::json!({"path":path}).to_string())
        })
        .collect::<Vec<_>>();
    let turns = calls
        .chunks(MAX_TOOL_CALLS_PER_TURN)
        .map(|chunk| chunk.to_vec())
        .collect();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        SequenceCalls {
            requests: Arc::clone(&requests),
            turns,
            current: Mutex::new(0),
        },
        "test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count(),
        1500
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallUnexecuted { .. } | RuntimeEvent::Paused { .. }
    )));
    let requests = requests.lock().unwrap();
    let reports = requests.iter().filter(|request| request.messages().last().is_some_and(|message| matches!(message.content(), [ContentBlock::Text {text}] if text == STALL_REPORT_NOTICE || text == SLICE_CHECKPOINT_NOTICE || text == ROOT_STALL_REPORT_NOTICE))).count();
    assert!(
        reports >= usize::try_from(1500 / STALL_REPORT_CALLS).unwrap(),
        "novel observations must preserve report cadence: {reports}"
    );
}

struct ValidationOnlySpawner;

impl SubagentSpawner for ValidationOnlySpawner {
    fn acknowledge(&self, _: ToolCallId) {}
    fn drain(&self) -> runtime::ChildDrainFuture {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
    fn spawn(&self, _: SpawnRequest) -> SpawnAgentFuture {
        panic!("invalid spawn must not reach the child executor")
    }
    fn wait_children(
        &self,
        _: Option<Vec<qq_protocol::SessionId>>,
        _: std::time::Duration,
    ) -> runtime::WaitFuture {
        panic!("invalid wait must not reach the child registry")
    }
    fn cancel_child(&self, _: qq_protocol::SessionId) -> runtime::CancelFuture {
        panic!("invalid cancellation must not reach the child registry")
    }
}

struct RecallFailure {
    foreign: bool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl runtime::HistorySearcher for RecallFailure {
    fn search(
        &self,
        _: String,
        _: usize,
        _: Option<runtime::HistoryCursor>,
    ) -> runtime::HistorySearchFuture {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(Err(
            "history backing store unavailable".to_owned()
        )))
    }
}

impl runtime::SpillReader for RecallFailure {
    fn read(&self, _: runtime::SpillHandle) -> runtime::SpillReadFuture {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(if self.foreign {
            Ok(runtime::SpillRead::ForeignSession)
        } else {
            Err("spill backing store unavailable".to_owned())
        }))
    }
}

#[tokio::test]
async fn ac4_runtime_host_refusals_do_not_establish_executed_error_streaks() {
    for (tool, arguments, expected, backing_calls) in [
        ("spawn_agent", "{}", "invalid arguments", 0),
        (
            "wait_agents",
            r#"{"timeout_seconds":0}"#,
            "timeout_seconds must",
            0,
        ),
        (
            "wait_agents",
            r#"{"ids":["invalid"],"timeout_seconds":1}"#,
            "ids must be",
            0,
        ),
        ("cancel_agent", r#"{"id":"invalid"}"#, "id must be", 0),
        ("search_history", r#"{"query":""}"#, "query must not", 0),
        (
            "read_tool_result",
            r#"{"handle":"invalid"}"#,
            "handle_invalid",
            0,
        ),
        (
            "read_tool_result",
            r#"{"handle":"t:read_file:00000000:00000000"}"#,
            "handle_placeholder",
            0,
        ),
        (
            "read_tool_result",
            r#"{"handle":"t:read_file:12345678:abcdef12"}"#,
            "handle_foreign_session",
            3,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recall = Arc::new(RecallFailure {
            foreign: true,
            calls: Arc::clone(&calls),
        });
        let runtime = Runtime::new(
            RepeatingCalls {
                requests: Arc::default(),
                tool,
                arguments,
                calls_per_turn: 3,
                ordinary_turns: 1,
                turns: Mutex::new(0),
            },
            "test",
            256,
        )
        .unwrap();
        let capabilities = RunCapabilities::user(Some(Arc::new(ValidationOnlySpawner)))
            .with_history(recall.clone())
            .with_spills(recall);
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("inspect")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(ApprovedFixtureGate),
                Arc::default(),
                capabilities,
            )
            .collect::<Vec<_>>()
            .await;
        let results = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished { result, .. } => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results.len(),
            3,
            "{tool} {arguments}: refusals must not manufacture an executed error streak"
        );
        assert!(
            results.iter().all(|result| result.contains(expected)),
            "{tool} {arguments}: {results:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            backing_calls
        );
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
    }
}

#[tokio::test]
async fn ac4_runtime_unavailable_recall_does_not_prove_an_executed_error_streak() {
    for (tool, arguments, expected) in [
        (
            "search_history",
            r#"{"query":"needle"}"#,
            "history backing store unavailable",
        ),
        (
            "read_tool_result",
            r#"{"handle":"t:read_file:12345678:abcdef12"}"#,
            "spill backing store unavailable",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recall = Arc::new(RecallFailure {
            foreign: false,
            calls: Arc::clone(&calls),
        });
        let runtime = Runtime::new(
            RepeatingCalls {
                requests: Arc::default(),
                tool,
                arguments,
                calls_per_turn: 3,
                ordinary_turns: 1,
                turns: Mutex::new(0),
            },
            "test",
            256,
        )
        .unwrap();
        let capabilities = RunCapabilities::user(None)
            .with_history(recall.clone())
            .with_spills(recall);
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("inspect")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(ApprovedFixtureGate),
                Arc::default(),
                capabilities,
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "{tool}: uncertain completion must not prove the repeat limit"
        );
        assert_eq!(events.iter().filter(|event| matches!(event, RuntimeEvent::ToolCallFinished { result, .. } if result == expected)).count(), 3);
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
    }
}

struct IdentifiedFixtureHost(MockMcpRegistry);
impl ExternalToolHost for IdentifiedFixtureHost {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn catalog_blocking(&self) -> HostCatalog {
        let mut catalog = self.0.catalog_blocking();
        for tool in &mut catalog.tools {
            tool.hints.read_only = true;
        }
        catalog
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
        let ordinal = self.0.calls.lock().unwrap().len();
        crate::HostCallObservation::Observed(Box::pin(async move {
            future.await.map(|result| crate::ObservedHostToolResult {
                result,
                result_digest: Some({
                    use sha2::Digest;
                    sha2::Sha256::digest(ordinal.to_le_bytes()).into()
                }),
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

#[tokio::test]
async fn ac4_runtime_external_producer_identity_survives_equal_previews() {
    let directory = tempfile::tempdir().unwrap();
    let registry = MockMcpRegistry::returning(HostToolResult {
        content: "[image content omitted]".to_owned(),
        is_error: false,
    });
    let calls = Arc::clone(&registry.calls);
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::default(),
            tool: "mcp__srv__ping",
            arguments: "{}",
            calls_per_turn: 6,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(IdentifiedFixtureHost(registry)));
    let events = runtime
        .run_loop(
            vec![Message::user("inspect")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(ApprovedFixtureGate),
            Arc::default(),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        calls.lock().unwrap().len(),
        6,
        "changed producer identity must remain novel despite identical previews"
    );
    assert_eq!(events.iter().filter(|event| matches!(event, RuntimeEvent::ToolCallFinished { result, .. } if result == "[image content omitted]")).count(), 6);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
}

#[tokio::test]
async fn ac4_runtime_1500_changed_results_for_one_call_remain_novel() {
    let directory = tempfile::tempdir().unwrap();
    let registry = MockMcpRegistry::returning(HostToolResult {
        content: "[image content omitted]".to_owned(),
        is_error: false,
    });
    let calls = Arc::clone(&registry.calls);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let turns = vec![("mcp__srv__ping", "{}".to_owned()); 1500]
        .chunks(MAX_TOOL_CALLS_PER_TURN)
        .map(|chunk| chunk.to_vec())
        .collect();
    let runtime = Runtime::new(
        SequenceCalls {
            requests: Arc::clone(&requests),
            turns,
            current: Mutex::new(0),
        },
        "test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(IdentifiedFixtureHost(registry)));
    let events = runtime
        .run_loop(
            vec![Message::user("inspect")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(ApprovedFixtureGate),
            Arc::default(),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(calls.lock().unwrap().len(), 1500);
    assert_eq!(events.iter().filter(|event| matches!(event, RuntimeEvent::ToolCallFinished { result, .. } if result == "[image content omitted]")).count(), 1500);
    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallUnexecuted { .. } | RuntimeEvent::Paused { .. }
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let reports = requests.lock().unwrap().iter().filter(|request|
        request.messages().last().is_some_and(|message|
            matches!(message.content(), [ContentBlock::Text {text}] if text == STALL_REPORT_NOTICE || text == SLICE_CHECKPOINT_NOTICE || text == ROOT_STALL_REPORT_NOTICE)
        )
    ).count();
    assert!(
        reports >= usize::try_from(1500 / STALL_REPORT_CALLS).unwrap(),
        "report cadence must survive changed observations: {reports}"
    );
}

/// Plan D: the second identical argument error carries a minimal valid call;
/// the first does not.
#[tokio::test]
async fn a_repeated_argument_error_shows_a_minimal_valid_call() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::clone(&requests),
            tool: "read_file",
            arguments: r#"{"file":"note.txt"}"#,
            calls_per_turn: 3,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let results = requests[1]
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 3);
    let hint = "a minimal valid call: read_file {\"path\":\"README.md\"}";
    assert!(
        results[0].starts_with("invalid arguments"),
        "{}",
        results[0]
    );
    assert!(!results[0].contains(hint), "{}", results[0]);
    assert!(results[1].starts_with(results[0]), "{}", results[1]);
    assert!(results[1].ends_with(hint), "{}", results[1]);
    // Refusals never enter the loop guard's executed streak, so the third
    // is answered the same way rather than refused.
    assert_eq!(results[2], results[1]);
}

/// An `edits` string or lone object is stored as the list it decodes to, so
/// approval previews, clients and compaction read the usual array shape.
#[tokio::test]
async fn edit_file_arguments_are_stored_with_edits_as_a_list() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
    let runtime = Runtime::new(
        RepeatingCalls {
            requests: Arc::new(Mutex::new(Vec::new())),
            tool: "edit_file",
            arguments: r#"{"edits":{"path":"a.txt","old":"alpha","new":"beta"}}"#,
            calls_per_turn: 1,
            ordinary_turns: 1,
            turns: Mutex::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("edit")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(ApprovedFixtureGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    let stored = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::AssistantTurnCompleted { calls, .. } => calls.first(),
            _ => None,
        })
        .unwrap();
    let stored_arguments = serde_json::from_str::<serde_json::Value>(&stored.arguments).unwrap();
    assert_eq!(
        stored_arguments,
        serde_json::json!({"edits":[{"path":"a.txt","old":"alpha","new":"beta"}]})
    );
    // The approval preview reads the stored arguments.
    let preview = crate::approval::edit_preview("edit_file", &stored.arguments).unwrap();
    assert_eq!(preview.path, "a.txt");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "beta\n"
    );
}
