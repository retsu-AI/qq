use super::*;

#[tokio::test]
async fn root_agents_instructions_and_completion_contract_reach_provider() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "Run the repository's focused checks before reporting success.\n",
    )
    .unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(
            RunCommand::new("finish the task"),
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    let captured = captured.lock().unwrap();
    let system = captured
        .as_ref()
        .and_then(ModelRequest::system)
        .expect("the provider request must carry a system prompt");
    assert!(system.contains("Workspace instructions from AGENTS.md"));
    assert!(system.contains("Run the repository's focused checks"));
    assert!(system.contains("This unattended run"));
    assert!(!system.contains("Do the work. Decide what done looks like"));
    assert!(system.contains("Preserve unrelated work"));
    assert!(system.contains("analysis-only"));
    assert!(system.contains("failed tools and tests as evidence"));
    assert!(system.contains("continue when a safe path remains"));
    assert!(system.contains("narrowest relevant verification before broader checks"));
    assert!(system.contains("Do not claim success without evidence"));
    assert!(system.contains("remaining failures and uncertainty honestly"));
    assert!(system.contains("time, token, cost, and safety budgets"));
    assert!(system.contains("more-specific scopes take precedence"));
}

#[tokio::test]
async fn agents_instructions_win_and_claude_is_an_absence_only_fallback() {
    for (agents, claude, expected_source, expected_text, rejected_text) in [
        (
            Some("Follow AGENTS policy.\n"),
            Some("Follow CLAUDE policy.\n"),
            "AGENTS.md",
            "Follow AGENTS policy.",
            "Follow CLAUDE policy.",
        ),
        (
            None,
            Some("Use the CLAUDE fallback.\n"),
            "CLAUDE.md",
            "Use the CLAUDE fallback.",
            "Follow AGENTS policy.",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        if let Some(content) = agents {
            std::fs::write(directory.path().join("AGENTS.md"), content).unwrap();
        }
        if let Some(content) = claude {
            std::fs::write(directory.path().join("CLAUDE.md"), content).unwrap();
        }
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_in_workspace(RunCommand::new("work"), directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RunEvent::Completed)));
        let captured = captured.lock().unwrap();
        let system = captured.as_ref().unwrap().system().unwrap();
        assert!(system.contains(&format!("Workspace instructions from {expected_source}")));
        assert!(system.contains(expected_text));
        assert!(!system.contains(rejected_text));
    }
}

#[tokio::test]
async fn no_instruction_file_and_analysis_only_request_complete_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let note = directory.path().join("note.txt");
    std::fs::write(&note, "unchanged\n").unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(
            RunCommand::new("Analyze the design only; do not edit files."),
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    assert_eq!(std::fs::read_to_string(note).unwrap(), "unchanged\n");
    let captured = captured.lock().unwrap();
    let request = captured
        .as_ref()
        .expect("missing instruction files must not prevent provider work");
    assert!(
        !request
            .system()
            .unwrap()
            .contains("BEGIN WORKSPACE INSTRUCTIONS")
    );
    assert_eq!(
        request.messages(),
        [Message::user("Analyze the design only; do not edit files.")]
    );
}

#[tokio::test]
async fn explicit_workspace_skill_reaches_the_shared_provider_request() {
    let directory = tempfile::tempdir().unwrap();
    let skill = directory.path().join(".qq/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "Review the requested change for durable-state regressions.\n",
    )
    .unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(
            RunCommand::new("/review focus on cancellation"),
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    let captured = captured.lock().unwrap();
    let request = captured.as_ref().expect("the provider must be called");
    let system = request.system().expect("the selected skill needs a prompt");
    assert!(system.contains("Selected skill `review`"));
    assert!(system.contains(".qq/skills/review/SKILL.md"));
    assert!(system.contains("Review the requested change for durable-state regressions."));
    assert_eq!(
        request.messages(),
        [Message::user("/review focus on cancellation")]
    );
}

#[tokio::test]
async fn native_guidance_shadows_compatibility_without_loading_ambient_bodies() {
    let directory = tempfile::tempdir().unwrap();
    for (path, content) in [
        (".qq/commands/check.md", "Use the native command.\n"),
        (
            ".agents/skills/check/SKILL.md",
            "Do not load the compatibility skill.\n",
        ),
        (
            ".qq/skills/ambient/SKILL.md",
            "Do not load an unselected skill.\n",
        ),
    ] {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(RunCommand::new("/check"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    let captured = captured.lock().unwrap();
    let system = captured.as_ref().unwrap().system().unwrap();
    assert!(system.contains("Selected command `check`"));
    assert!(system.contains("Use the native command."));
    assert!(!system.contains("Do not load the compatibility skill."));
    assert!(!system.contains("Do not load an unselected skill."));
}

#[tokio::test]
async fn ambiguous_and_unknown_guidance_fail_before_provider_work() {
    for (name, setup, expected) in [
        (
            "duplicate",
            vec![
                (".qq/commands/duplicate.md", "command\n"),
                (".qq/skills/duplicate/SKILL.md", "skill\n"),
            ],
            "ambiguous command or skill /duplicate",
        ),
        ("missing", Vec::new(), "unknown command or skill /missing"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        for (path, content) in setup {
            let path = directory.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_in_workspace(
                RunCommand::new(format!("/{name}")),
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::InvalidCommand,
                message,
            }) if message.contains(expected)
        ));
        assert!(captured.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn guidance_bounds_and_reserved_names_fail_before_provider_work() {
    for (prompt, write_oversized, expected) in [
        ("/large", true, "exceeds the 65536-byte file limit"),
        ("/quit", false, "reserved client command"),
        ("/Upper", false, "slash invocation names must start"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        if write_oversized {
            let skill = directory.path().join(".qq/skills/large");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), vec![b'x'; 64 * 1024 + 1]).unwrap();
        }
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_in_workspace(RunCommand::new(prompt), directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::InvalidCommand,
                message,
            }) if message.contains(expected)
        ));
        assert!(captured.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn double_slash_escapes_runtime_guidance_selection() {
    let directory = tempfile::tempdir().unwrap();
    let skill = directory.path().join(".qq/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "Must not be loaded.\n").unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(
            RunCommand::new("//review literally"),
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    let captured = captured.lock().unwrap();
    let request = captured.as_ref().unwrap();
    assert_eq!(request.messages(), [Message::user("/review literally")]);
    assert!(!request.system().unwrap().contains("Must not be loaded."));
}

#[cfg(unix)]
#[tokio::test]
async fn guidance_symlink_escape_fails_before_provider_work() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("SKILL.md");
    std::fs::write(&target, "outside authority\n").unwrap();
    let skill = directory.path().join(".qq/skills/escape");
    std::fs::create_dir_all(&skill).unwrap();
    symlink(target, skill.join("SKILL.md")).unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(RunCommand::new("/escape"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::InvalidCommand,
            ..
        })
    ));
    assert!(captured.lock().unwrap().is_none());
}

#[derive(Clone)]
struct ScopedCall {
    name: &'static str,
    arguments: String,
}

impl ScopedCall {
    fn new(name: &'static str, arguments: impl Into<String>) -> Self {
        Self {
            name,
            arguments: arguments.into(),
        }
    }
}

struct ScopedScriptProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    turns: Vec<Vec<ScopedCall>>,
    turn: AtomicUsize,
    gate_calls: Option<Arc<AtomicUsize>>,
    expected_gate_calls: Vec<usize>,
}

impl ScopedScriptProvider {
    fn new(requests: Arc<Mutex<Vec<ModelRequest>>>, turns: Vec<Vec<ScopedCall>>) -> Self {
        Self {
            requests,
            turns,
            turn: AtomicUsize::new(0),
            gate_calls: None,
            expected_gate_calls: Vec::new(),
        }
    }

    fn checking_gate(mut self, gate_calls: Arc<AtomicUsize>, expected: Vec<usize>) -> Self {
        self.gate_calls = Some(gate_calls);
        self.expected_gate_calls = expected;
        self
    }
}

impl Provider for ScopedScriptProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let turn = self.turn.fetch_add(1, Ordering::SeqCst);
        if let (Some(gate_calls), Some(expected)) =
            (&self.gate_calls, self.expected_gate_calls.get(turn))
        {
            assert_eq!(gate_calls.load(Ordering::SeqCst), *expected);
        }
        self.requests.lock().unwrap().push(request);
        let Some(calls) = self.turns.get(turn) else {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        };
        let mut events = Vec::with_capacity(calls.len() * 3 + 1);
        for (index, call) in calls.iter().enumerate() {
            let id = format!("scoped-{turn}-{index}");
            events.extend([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: call.name.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: call.arguments.clone(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
            ]);
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

struct CountingGate {
    calls: Arc<AtomicUsize>,
    denial: Option<&'static str>,
}

impl CountingGate {
    fn allowing(calls: Arc<AtomicUsize>) -> Self {
        Self {
            calls,
            denial: None,
        }
    }

    fn denying(calls: Arc<AtomicUsize>, message: &'static str) -> Self {
        Self {
            calls,
            denial: Some(message),
        }
    }
}

impl ToolGate for CountingGate {
    fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let decision = self
            .denial
            .map_or(GateDecision::Execute, |message| GateDecision::Deny {
                message: message.to_owned(),
            });
        Box::pin(std::future::ready(decision))
    }
}

fn request_result_text(request: &ModelRequest) -> String {
    request
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn latest_result_text(request: &ModelRequest) -> String {
    request
        .messages()
        .last()
        .into_iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn request_tool_results(request: &ModelRequest) -> Vec<(&str, bool)> {
    request
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[tokio::test]
async fn nested_read_is_deferred_for_automatic_parent_and_child_instruction_disclosure() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("parent/child")).unwrap();
    std::fs::create_dir_all(directory.path().join("parent/sibling")).unwrap();
    std::fs::write(
        directory.path().join("parent/CLAUDE.md"),
        "Follow parent fallback.\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("parent/child/AGENTS.md"),
        "Follow child policy.\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("parent/sibling/AGENTS.md"),
        "Sibling secret must not leak.\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("parent/child/note.txt"), "nested\n").unwrap();
    let read = ScopedCall::new("read_file", r#"{"path":"parent/child/note.txt"}"#);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let provider = ScopedScriptProvider::new(
        Arc::clone(&requests),
        vec![vec![read.clone()], vec![read.clone()], vec![read]],
    )
    .checking_gate(Arc::clone(&gate_calls), vec![0, 0, 1, 2]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("inspect the nested file")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(gate_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
            .count(),
        1
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let notice = latest_result_text(&requests[1]);
    assert!(notice.contains("Instruction source: parent/CLAUDE.md"));
    assert!(notice.contains("Scope: parent"));
    assert!(notice.contains("Follow parent fallback."));
    assert!(notice.contains("Instruction source: parent/child/AGENTS.md"));
    assert!(notice.contains("Scope: parent/child"));
    assert!(notice.contains("Content hash:"));
    assert!(notice.contains("Follow child policy."));
    assert!(!notice.contains("Sibling secret must not leak."));
    assert!(
        request_tool_results(&requests[1])
            .iter()
            .all(|(_, error)| *error)
    );
    assert!(!latest_result_text(&requests[2]).contains("Follow parent fallback."));
    assert!(!latest_result_text(&requests[2]).contains("Follow child policy."));
    assert!(!latest_result_text(&requests[3]).contains("Follow parent fallback."));
    assert!(!latest_result_text(&requests[3]).contains("Follow child policy."));
    assert!(latest_result_text(&requests[2]).contains("1\tnested"));
    assert!(latest_result_text(&requests[3]).contains("1\tnested"));
    assert!(requests.iter().all(|request| {
        request
            .system()
            .is_some_and(|system| !system.contains("Follow child policy."))
    }));
}

#[tokio::test]
async fn parallel_same_turn_reads_are_all_deferred_with_one_instruction_body() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("scope")).unwrap();
    std::fs::write(
        directory.path().join("scope/AGENTS.md"),
        "One disclosed body for this scope.\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("scope/one.txt"), "one\n").unwrap();
    std::fs::write(directory.path().join("scope/two.txt"), "two\n").unwrap();
    let calls = vec![
        ScopedCall::new("read_file", r#"{"path":"scope/one.txt"}"#),
        ScopedCall::new("search", r#"{"path":"scope","query":"two"}"#),
        ScopedCall::new("tree", r#"{"path":"scope","depth":1}"#),
    ];
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let provider = ScopedScriptProvider::new(Arc::clone(&requests), vec![calls.clone(), calls])
        .checking_gate(Arc::clone(&gate_calls), vec![0, 0, 3]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("read both")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(gate_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
            .count(),
        3
    );
    let requests = requests.lock().unwrap();
    let results = request_tool_results(&requests[1]);
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|(_, error)| *error));
    assert_eq!(
        occurrences(
            &results.iter().map(|(text, _)| *text).collect::<String>(),
            "One disclosed body for this scope."
        ),
        1
    );
    assert!(results[0].0.contains("Instruction source: scope/AGENTS.md"));
    assert!(
        results[1]
            .0
            .contains("scoped instruction preflight deferred this tool turn")
    );
    assert!(!results[1].0.contains("One disclosed body for this scope."));

    assert!(!results[2].0.contains("One disclosed body for this scope."));
}

#[tokio::test]
async fn full_scoped_instruction_notice_is_not_truncated_by_tool_output_bounds() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("scope")).unwrap();
    let body = format!("BEGIN-FULL-BODY\n{}END-FULL-BODY\n", "z".repeat(50 * 1024));
    std::fs::write(directory.path().join("scope/AGENTS.md"), &body).unwrap();
    std::fs::write(directory.path().join("scope/note.txt"), "value\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ScopedScriptProvider::new(
        Arc::clone(&requests),
        vec![vec![ScopedCall::new(
            "read_file",
            r#"{"path":"scope/note.txt"}"#,
        )]],
    );
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("read the file")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let notice = latest_result_text(&requests[1]);
    assert!(notice.contains("BEGIN-FULL-BODY"));
    assert!(notice.contains("END-FULL-BODY"));
    assert!(notice.contains(&"z".repeat(50 * 1024)));
    assert!(!notice.contains("omitted"));
}
#[tokio::test]
async fn multi_scope_edit_waits_for_complete_disclosure_before_gate_and_mutation() {
    let directory = tempfile::tempdir().unwrap();
    for (scope, policy, value) in [
        ("alpha", "Alpha edit policy.\n", "alpha before\n"),
        ("beta", "Beta edit policy.\n", "beta before\n"),
    ] {
        std::fs::create_dir(directory.path().join(scope)).unwrap();
        std::fs::write(directory.path().join(scope).join("AGENTS.md"), policy).unwrap();
        std::fs::write(directory.path().join(scope).join("note.txt"), value).unwrap();
    }
    let edit = ScopedCall::new(
        "edit_file",
        r#"{"edits":[{"path":"alpha/note.txt","old":"alpha before\n","new":"alpha after\n"},{"path":"beta/note.txt","old":"beta before\n","new":"beta after\n"}]}"#,
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let provider =
        ScopedScriptProvider::new(Arc::clone(&requests), vec![vec![edit.clone()], vec![edit]])
            .checking_gate(Arc::clone(&gate_calls), vec![0, 0, 1]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("edit both files")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(gate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("alpha/note.txt")).unwrap(),
        "alpha after\n"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("beta/note.txt")).unwrap(),
        "beta after\n"
    );
    let requests = requests.lock().unwrap();
    let notice = latest_result_text(&requests[1]);
    assert!(notice.contains("Alpha edit policy."));
    assert!(notice.contains("Beta edit policy."));
}

#[tokio::test]
async fn invalid_later_edit_scope_keeps_every_target_unchanged_and_skips_the_gate() {
    let directory = tempfile::tempdir().unwrap();
    for scope in ["alpha", "beta"] {
        std::fs::create_dir(directory.path().join(scope)).unwrap();
        std::fs::write(
            directory.path().join(scope).join("note.txt"),
            format!("{scope} before\n"),
        )
        .unwrap();
    }
    std::fs::write(
        directory.path().join("alpha/AGENTS.md"),
        "Valid earlier scope.\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("beta/AGENTS.md"), [0xff]).unwrap();
    let edit = ScopedCall::new(
        "edit_file",
        r#"{"edits":[{"path":"alpha/note.txt","old":"alpha before\n","new":"alpha after\n"},{"path":"beta/note.txt","old":"beta before\n","new":"beta after\n"}]}"#,
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let provider = ScopedScriptProvider::new(Arc::clone(&requests), vec![vec![edit]])
        .checking_gate(Arc::clone(&gate_calls), vec![0, 0]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("edit both files")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("alpha/note.txt")).unwrap(),
        "alpha before\n"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("beta/note.txt")).unwrap(),
        "beta before\n"
    );
    let requests = requests.lock().unwrap();
    let result = request_result_text(&requests[1]);
    assert!(result.contains("beta/AGENTS.md"));
    assert!(result.contains("not valid UTF-8"));
    assert!(!result.contains("Valid earlier scope."));
}

#[tokio::test]
async fn invalid_scoped_instruction_inputs_fail_explicitly_without_mutation() {
    for case in ["oversized", "invalid_utf8", "aggregate", "disclosure"] {
        let directory = tempfile::tempdir().unwrap();
        let target = match case {
            "aggregate" | "disclosure" => {
                let mut scope = std::path::PathBuf::new();
                let segments = if case == "aggregate" {
                    vec!["a", "b", "c", "d", "e"]
                } else {
                    vec!["a", "b"]
                };
                for segment in segments {
                    scope.push(segment);
                    std::fs::create_dir_all(directory.path().join(&scope)).unwrap();
                    std::fs::write(
                        directory.path().join(&scope).join("AGENTS.md"),
                        vec![b'x'; 60 * 1024],
                    )
                    .unwrap();
                }
                scope.join("note.txt")
            }
            "oversized" | "invalid_utf8" => {
                std::fs::create_dir(directory.path().join("scope")).unwrap();
                std::fs::write(
                    directory.path().join("scope/AGENTS.md"),
                    if case == "oversized" {
                        vec![b'x'; 64 * 1024 + 1]
                    } else {
                        vec![0xff]
                    },
                )
                .unwrap();
                std::path::PathBuf::from("scope/note.txt")
            }
            _ => unreachable!(),
        };
        std::fs::write(directory.path().join(&target), "before\n").unwrap();
        let arguments = serde_json::json!({
            "path": target.to_string_lossy(),
            "content": "after\n"
        })
        .to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate_calls = Arc::new(AtomicUsize::new(0));
        let provider = ScopedScriptProvider::new(
            Arc::clone(&requests),
            vec![vec![ScopedCall::new("write_file", arguments)]],
        )
        .checking_gate(Arc::clone(&gate_calls), vec![0, 0]);
        let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

        let events = runtime
            .run_loop(
                vec![Message::user("write")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join(&target)).unwrap(),
            "before\n",
            "{case}"
        );
        let requests = requests.lock().unwrap();
        let result = request_result_text(&requests[1]);
        let expected = match case {
            "oversized" => "exceeds the 65536-byte file limit",
            "invalid_utf8" => "not valid UTF-8",
            "aggregate" => "exceed the 262144-byte aggregate limit",
            "disclosure" => "exceeds the 81920-byte disclosure limit",
            _ => unreachable!(),
        };
        assert!(result.contains(expected), "{case}: {result}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn escaped_scoped_instruction_symlink_fails_explicitly_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("scope")).unwrap();
    std::fs::write(outside.path().join("policy.md"), "outside\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("policy.md"),
        directory.path().join("scope/AGENTS.md"),
    )
    .unwrap();
    std::fs::write(directory.path().join("scope/note.txt"), "before\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let provider = ScopedScriptProvider::new(
        Arc::clone(&requests),
        vec![vec![ScopedCall::new(
            "write_file",
            r#"{"path":"scope/note.txt","content":"after\n"}"#,
        )]],
    )
    .checking_gate(Arc::clone(&gate_calls), vec![0, 0]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("write")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("scope/note.txt")).unwrap(),
        "before\n"
    );
    let requests = requests.lock().unwrap();
    let result = request_result_text(&requests[1]);
    assert!(result.contains("scope/AGENTS.md"));
    assert!(result.contains("workspace"));
}

#[tokio::test]
async fn externally_added_and_removed_instructions_are_disclosed_on_the_next_call() {
    struct MutatingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
        turn: AtomicUsize,
        scope: std::path::PathBuf,
    }

    impl Provider for MutatingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            match turn {
                1 => std::fs::write(self.scope.join("AGENTS.md"), "Externally added policy.\n")
                    .unwrap(),
                3 => std::fs::remove_file(self.scope.join("AGENTS.md")).unwrap(),
                _ => {}
            }
            if turn >= 5 {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let id = format!("external-{turn}");
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: r#"{"path":"scope/note.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let scope = directory.path().join("scope");
    std::fs::create_dir(&scope).unwrap();
    std::fs::write(scope.join("note.txt"), "value\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        MutatingProvider {
            requests: Arc::clone(&requests),
            turn: AtomicUsize::new(0),
            scope,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_messages_in_workspace(
            vec![Message::user("keep reading")],
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { .. }))
            .count(),
        2
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    let added = latest_result_text(&requests[2]);
    assert!(added.contains("Instruction source: scope/AGENTS.md"));
    assert!(added.contains("Externally added policy."));
    let removed = latest_result_text(&requests[4]);
    assert!(removed.contains("Instruction source: scope/AGENTS.md"));
    assert!(removed.contains("Removed: this instruction scope no longer applies."));
    assert!(!latest_result_text(&requests[3]).contains("Externally added policy."));
    assert!(!latest_result_text(&requests[4]).contains("Externally added policy."));
}

#[tokio::test]
async fn same_turn_instruction_mutation_defers_later_target_until_review() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("scope")).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let target_args = r#"{"path":"scope/note.txt","content":"after review","create_only":true}"#;
    let provider = ScopedScriptProvider::new(Arc::clone(&requests), vec![
        vec![
            ScopedCall::new("write_file", r#"{"path":"scope/AGENTS.md","content":"New same-turn policy","create_only":true}"#),
            ScopedCall::new("write_file", target_args),
        ],
        vec![ScopedCall::new("write_file", target_args)],
    ]).checking_gate(Arc::clone(&gate_calls), vec![0, 1, 2]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("write policy then target")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::allowing(Arc::clone(&gate_calls))),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("scope/note.txt")).unwrap(),
        "after review"
    );
    let requests = requests.lock().unwrap();
    assert!(latest_result_text(&requests[1]).contains("New same-turn policy"));
    assert!(latest_result_text(&requests[1]).contains("Instruction source: scope/AGENTS.md"));
    assert!(events.iter().any(|event| matches!(event, RuntimeEvent::ToolCallUnexecuted { message, .. } if message.contains("New same-turn policy"))));
}

#[tokio::test]
async fn scoped_instruction_retry_does_not_bypass_an_authority_gate_denial() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("scope")).unwrap();
    std::fs::write(
        directory.path().join("scope/AGENTS.md"),
        "This guidance cannot authorize writes.\n",
    )
    .unwrap();
    let write = ScopedCall::new(
        "write_file",
        r#"{"path":"scope/new.txt","content":"forbidden\n"}"#,
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let provider = ScopedScriptProvider::new(
        Arc::clone(&requests),
        vec![vec![write.clone()], vec![write]],
    )
    .checking_gate(Arc::clone(&gate_calls), vec![0, 0, 1]);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user("write the file")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(CountingGate::denying(
                Arc::clone(&gate_calls),
                "authority denied",
            )),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(gate_calls.load(Ordering::SeqCst), 1);
    assert!(!directory.path().join("scope/new.txt").exists());
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallDenied { message, .. } if message == "authority denied"
    )));
    let requests = requests.lock().unwrap();
    assert!(request_result_text(&requests[1]).contains("This guidance cannot authorize writes."));
    assert!(request_result_text(&requests[2]).contains("authority denied"));
}

#[tokio::test]
async fn invalid_primary_instructions_fail_before_provider_work() {
    for case in ["oversized", "invalid_utf8", "directory"] {
        let directory = tempfile::tempdir().unwrap();
        match case {
            "oversized" => {
                std::fs::write(
                    directory.path().join("AGENTS.md"),
                    vec![b'x'; 64 * 1024 + 1],
                )
                .unwrap();
            }
            "invalid_utf8" => {
                std::fs::write(directory.path().join("AGENTS.md"), [0xff]).unwrap();
            }
            "directory" => {
                std::fs::create_dir(directory.path().join("AGENTS.md")).unwrap();
            }
            _ => unreachable!(),
        }
        std::fs::write(
            directory.path().join("CLAUDE.md"),
            "This fallback must not mask an invalid AGENTS.md.\n",
        )
        .unwrap();
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_in_workspace(RunCommand::new("work"), directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.as_slice(),
            [
                RunEvent::Started,
                RunEvent::Failed {
                    kind: RunFailureKind::Configuration,
                    message,
                }
            ] if message.contains("AGENTS.md")
        ));
        assert!(captured.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn instruction_file_at_the_byte_limit_reaches_provider() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("AGENTS.md"), vec![b'x'; 64 * 1024]).unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(RunCommand::new("work"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(RunEvent::Completed)));
    let captured = captured.lock().unwrap();
    let system = captured.as_ref().unwrap().system().unwrap();
    assert!(system.contains(&"x".repeat(64 * 1024)));
}

#[tokio::test]
async fn cancellation_after_workspace_open_starts_no_provider_work() {
    struct ExecuteGate;

    impl ToolGate for ExecuteGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let cancelled = RunCancellation::new();
    let run_cancelled = cancelled.clone();
    let pause = workspace::test_pause_after_workspace_open(&cancelled);
    let task = tokio::spawn(async move {
        runtime
            .run_loop(
                vec![Message::user("work")],
                directory.path().to_owned(),
                run_cancelled,
                Arc::new(ExecuteGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await
    });
    let pause = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            pause.wait_until_opened().unwrap();
            pause
        }),
    )
    .await
    .unwrap()
    .unwrap();
    cancelled.cancel();
    pause.resume().unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(
        events.as_slice(),
        [
            RuntimeEvent::Started,
            RuntimeEvent::Failed {
                kind: RunFailureKind::Configuration,
                message,
            }
        ] if message.contains("cancelled")
    ));
    assert!(captured.lock().unwrap().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn instruction_symlink_escape_fails_before_provider_work() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("policy.md"), "outside policy\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("policy.md"),
        directory.path().join("AGENTS.md"),
    )
    .unwrap();
    let captured = Arc::new(Mutex::new(None));
    let runtime = Runtime::new(
        ScriptedProvider {
            request: Arc::clone(&captured),
            fails: false,
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(RunCommand::new("work"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.as_slice(),
        [
            RunEvent::Started,
            RunEvent::Failed {
                kind: RunFailureKind::Configuration,
                message,
            }
        ] if message.contains("AGENTS.md") && message.contains("workspace")
    ));
    assert!(captured.lock().unwrap().is_none());
}
