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
    assert!(system.contains("let the more specific one win"));
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

#[tokio::test]
async fn mutation_flow_discovers_mixed_scopes_and_verifies_before_completion() {
    struct AllowAllGate;

    impl ToolGate for AllowAllGate {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }

    struct VerificationProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for VerificationProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let system = request
                .system()
                .expect("every turn must retain the root prefix");
            assert!(system.contains("Follow root policy."));
            assert!(!system.contains("Follow src fallback."));
            assert!(!system.contains("Follow feature policy."));
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request.clone());
            drop(requests);

            let tool_turn = |id: &str, name: &str, arguments: &str| {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.to_owned(),
                        name: name.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.to_owned(),
                        json: arguments.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id: id.to_owned() }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ])) as ProviderStream
            };

            match turn {
                0 => tool_turn("list-src", "tree", r#"{"path":"src","depth":1}"#),
                1 => {
                    assert!(matches!(
                        request.messages().last().map(Message::content),
                        Some([ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error: false,
                        }]) if call_id == "list-src"
                            && content.contains("CLAUDE.md")
                            && content.contains("feature/")
                    ));
                    tool_turn(
                        "read-src-policy",
                        "read_file",
                        r#"{"path":"src/CLAUDE.md"}"#,
                    )
                }
                2 => {
                    assert!(matches!(
                        request.messages().last().map(Message::content),
                        Some([ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error: false,
                        }]) if call_id == "read-src-policy"
                            && content.ends_with("\n1\tFollow src fallback.\n")
                    ));
                    tool_turn(
                        "list-feature",
                        "tree",
                        r#"{"path":"src/feature","depth":1}"#,
                    )
                }
                3 => {
                    assert!(matches!(
                        request.messages().last().map(Message::content),
                        Some([ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error: false,
                        }]) if call_id == "list-feature"
                            && content.contains("AGENTS.md")
                            && content.contains("CLAUDE.md")
                    ));
                    tool_turn(
                        "read-feature-policy",
                        "read_file",
                        r#"{"path":"src/feature/AGENTS.md"}"#,
                    )
                }
                4 => {
                    assert!(matches!(
                        request.messages().last().map(Message::content),
                        Some([ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error: false,
                        }]) if call_id == "read-feature-policy"
                            && content.ends_with("\n1\tFollow feature policy.\n")
                    ));
                    tool_turn(
                        "read-before",
                        "read_file",
                        r#"{"path":"src/feature/note.txt"}"#,
                    )
                }
                5 => {
                    assert!(matches!(
                        request.messages().last().map(Message::content),
                        Some([ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error: false,
                        }]) if call_id == "read-before" && content.ends_with("\n1\tbefore\n")
                    ));
                    tool_turn(
                        "edit",
                        "edit_file",
                        r#"{"edits":[{"path":"src/feature/note.txt","old":"before\n","new":"after\n"}]}"#,
                    )
                }
                6 => tool_turn(
                    "read-after",
                    "read_file",
                    r#"{"path":"src/feature/note.txt"}"#,
                ),
                7 => {
                    assert!(matches!(
                        request.messages().last().map(Message::content),
                        Some([ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error: false,
                        }]) if call_id == "read-after" && content.ends_with("\n1\tafter\n")
                    ));
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "verified".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                }
                _ => panic!("provider was polled after its verified completion"),
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("src/feature")).unwrap();
    std::fs::write(directory.path().join("AGENTS.md"), "Follow root policy.\n").unwrap();
    std::fs::write(
        directory.path().join("src/CLAUDE.md"),
        "Follow src fallback.\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("src/feature/AGENTS.md"),
        "Follow feature policy.\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("src/feature/CLAUDE.md"),
        "This same-scope fallback must not be loaded.\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("src/feature/note.txt"), "before\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        VerificationProvider {
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_loop(
            vec![Message::user(
                "Update src/feature/note.txt and verify the result.",
            )],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(AllowAllGate),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("src/feature/note.txt")).unwrap(),
        "after\n"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 8);
    assert!(
        !requests
            .iter()
            .flat_map(ModelRequest::messages)
            .any(|message| {
                message.content().iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, .. }
                if content.contains("same-scope fallback"))
                })
            })
    );
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
