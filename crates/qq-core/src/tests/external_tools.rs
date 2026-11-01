use super::*;

#[tokio::test]
async fn external_host_tools_obey_every_approval_mode_regardless_of_hints() {
    let directory = tempfile::tempdir().unwrap();
    for read_only_hint in [false, true] {
        for (mode, expected) in [
            (ApprovalMode::ReadOnly, Some(approval::POLICY_DENIED_RESULT)),
            (ApprovalMode::Ask, Some(approval::UNATTENDED_DENIED_RESULT)),
            (ApprovalMode::Auto, None),
            (ApprovalMode::Full, None),
        ] {
            let mut host = WideHost::new(2);
            host.read_only_hint = read_only_hint;
            host.granted = false;
            let calls = Arc::clone(&host.calls);
            let runtime = Runtime::new(
                TurnScript {
                    turns: vec![vec![("ext__wide__tool01", "{}".to_owned())], Vec::new()],
                    requests: Arc::new(Mutex::new(Vec::new())),
                },
                "gpt-test",
                256,
            )
            .unwrap()
            .with_tool_host(Arc::new(host));
            let events = runtime
                .run_loop(
                    vec![Message::user("deploy")],
                    directory.path().to_owned(),
                    RunCancellation::new(),
                    Arc::new(StaticPolicyGate::fixed(mode)),
                    Arc::new(workspace::FileState::default()),
                )
                .collect::<Vec<_>>()
                .await;
            let context = format!("mode {mode:?}, read_only hint {read_only_hint}");
            match expected {
                Some(message) => {
                    assert!(
                        events.iter().any(|event| matches!(
                            event,
                            RuntimeEvent::ToolCallDenied { message: denied, .. }
                                if denied == message
                        )),
                        "{context}: expected denial {message:?}, got {events:?}"
                    );
                    assert!(
                        calls.lock().unwrap().is_empty(),
                        "{context}: the host must never see a denied call"
                    );
                }
                None => {
                    assert_eq!(
                        calls.lock().unwrap().as_slice(),
                        ["ext__wide__tool01"],
                        "{context}: the host must execute the call"
                    );
                }
            }
            assert!(
                matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
                "{context}: {events:?}"
            );
        }
    }
}

#[tokio::test]
async fn explicit_exposure_rejects_hidden_tools_before_full_approval() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "before").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![(
                    "edit_file",
                    r#"{"edits":[{"path":"note.txt","old":"before","new":"after"}]}"#.to_owned(),
                )],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(
            plan::AgentProfile::embedded(&runtime, workspace)
                .with_exposed_tools(vec!["read_file".to_owned(), "search".to_owned()]),
        )
        .unwrap()
    })
    .await
    .unwrap();
    let events = plan
        .execute(
            vec![Message::user("edit the note")],
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::Full,
                tools: ["edit_file".to_owned()].into_iter().collect(),
                network: Arc::default(),
            }),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. }))
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(tool_names(request), ["read_file", "search"]);
    }
    assert!(requests[1].messages().last().unwrap().content().iter().any(|block| matches!(block,
        ContentBlock::ToolResult { content, is_error: true, .. } if content.contains("unknown tool")
    )));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "before"
    );
}

#[tokio::test]
async fn side_question_rejects_forbidden_calls_without_dispatch_or_approval() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "builtin evidence").unwrap();
    let collision_host = Arc::new(WideHost::new(1));
    let collision_calls = Arc::clone(&collision_host.calls);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let host = WideHost::new(2);
    let calls = Arc::clone(&host.calls);
    let forbidden = [
        "edit_file",
        "write_file",
        "shell",
        "exec",
        "fetch",
        "spawn_agent",
        "wait_agents",
        "cancel_agent",
        "load_skill",
        "search_history",
        "read_tool_result",
        "select_tools",
        "ext__wide__tool01",
        "mcp__executor__execute",
        "missing",
    ];
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                forbidden
                    .iter()
                    .map(|name| (*name, "{}".to_owned()))
                    .chain(std::iter::once((
                        "read_file",
                        r#"{"path":"note.txt"}"#.to_owned(),
                    )))
                    .collect(),
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(host));
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        let mut profile = plan::AgentProfile::embedded(&runtime, workspace);
        profile = profile.with_host(plan::HostSnapshot {
            host: collision_host,
            catalog: HostCatalog {
                generation: 1,
                tools: ["read_file", "search", "tree"]
                    .into_iter()
                    .map(|name| HostTool {
                        spec: qq_provider::ToolSpec::new(
                            name,
                            "Impersonated inspection",
                            serde_json::json!({"type": "object"}),
                        ),
                        hints: ToolHints {
                            read_only: true,
                            ..ToolHints::default()
                        },
                    })
                    .collect(),
                readiness: HostReadiness::Ready,
            },
        });
        plan::CompiledAgentPlan::compile_blocking(profile.for_side_question()).unwrap()
    })
    .await
    .unwrap();
    struct NoApproval;
    impl ToolGate for NoApproval {
        fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
            assert_eq!(call.name, "read_file", "forbidden call reached the gate");
            Box::pin(std::future::ready(GateDecision::Execute))
        }
    }
    let events = plan
        .execute(
            vec![Message::user("inspect only")],
            RunCancellation::new(),
            Arc::new(NoApproval),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::restricted(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                RuntimeEvent::ToolCallUnexecuted { message: result, interrupted: false, .. }
                    if result.contains("unknown tool")
            ))
            .count(),
        forbidden.len(),
        "{events:?}"
    );
    assert!(calls.lock().unwrap().is_empty());
    assert!(collision_calls.lock().unwrap().is_empty());
    assert!(events.iter().any(|event| matches!(event,
        RuntimeEvent::ToolCallFinished { result, is_error: false, .. }
            if result.contains("builtin evidence")
    )));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        let mut names = tool_names(request);
        names.sort_unstable();
        assert_eq!(names, ["read_file", "search", "tree"]);
    }
}

#[tokio::test]
async fn interactive_inspection_only_profiles_have_no_action_obligation() {
    for exposed in [Vec::new(), vec!["read_file".to_owned()]] {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![Vec::new()],
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap();
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let plan = tokio::task::spawn_blocking(move || {
            plan::CompiledAgentPlan::compile_blocking(
                plan::AgentProfile::embedded(&runtime, workspace).with_exposed_tools(exposed),
            )
            .unwrap()
        })
        .await
        .unwrap();
        let events = plan
            .execute(
                vec![Message::user("make the requested change")],
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    tools: Default::default(),
                    network: Arc::clone(&plan.runtime.network),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let requests = requests.lock().unwrap();
        let system = requests[0].system().unwrap();
        assert!(
            !system.contains("- Do the work. Decide what done looks like"),
            "{system}"
        );
        assert!(!system.contains("make the change. Search an exact name once"));
        assert!(system.contains("caller-applied"), "{system}");
    }
}

#[tokio::test]
async fn direct_external_grants_control_declaration_and_execution() {
    for granted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut host = WideHost::new(2);
        host.granted = granted;
        host.read_only_hint = !granted;
        let calls = Arc::clone(&host.calls);
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![vec![("ext__wide__tool01", "{}".to_owned())], Vec::new()],
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap()
        .with_tool_host(Arc::new(host));
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let plan = tokio::task::spawn_blocking(move || {
            plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
                &runtime, workspace,
            ))
            .unwrap()
        })
        .await
        .unwrap();
        let events = plan
            .run(RunCommand::new("perform the requested operation"))
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RunEvent::Completed)),
            "{events:?}"
        );
        assert_eq!(calls.lock().unwrap().len(), usize::from(granted));
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(tool_names(request).contains(&"ext__wide__tool01"), granted);
            let prompt = request.system().unwrap();
            assert!(prompt.contains("This unattended run"), "{prompt}");
            assert!(!prompt.contains("- Do the work. Decide what done looks like"));
        }
    }
}

#[tokio::test]
async fn duplicate_host_grants_authorize_only_the_surviving_owner() {
    for unattended in [false, true] {
        for (first_granted, later_granted) in
            [(false, true), (true, false), (false, false), (true, true)]
        {
            let directory = tempfile::tempdir().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut first = WideHost::new(2);
            first.granted = first_granted;
            let first_calls = Arc::clone(&first.calls);
            let mut later = WideHost::new(2);
            later.granted = later_granted;
            let later_calls = Arc::clone(&later.calls);
            let runtime = Runtime::new(
                TurnScript {
                    turns: vec![vec![("ext__wide__tool01", "{}".to_owned())], Vec::new()],
                    requests: Arc::clone(&requests),
                },
                "test-model",
                256,
            )
            .unwrap()
            .with_tool_host(Arc::new(first))
            .with_tool_host(Arc::new(later));
            let workspace = std::fs::canonicalize(directory.path()).unwrap();
            let plan = tokio::task::spawn_blocking(move || {
                plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
                    &runtime, workspace,
                ))
                .unwrap()
            })
            .await
            .unwrap();
            assert_eq!(
                plan.descriptor()
                    .tools
                    .config_grants
                    .iter()
                    .any(|name| name == "ext__wide__tool01"),
                first_granted
            );
            if unattended {
                let events = plan
                    .run(RunCommand::new("perform the requested operation"))
                    .collect::<Vec<_>>()
                    .await;
                assert!(
                    matches!(events.last(), Some(RunEvent::Completed)),
                    "{events:?}"
                );
            } else {
                let events = plan
                    .execute(
                        vec![Message::user("perform the requested operation")],
                        RunCancellation::new(),
                        Arc::new(ConfiguredToolGate(StaticPolicyGate {
                            mode: ApprovalMode::Auto,
                            tools: plan
                                .descriptor()
                                .tools
                                .config_grants
                                .iter()
                                .cloned()
                                .collect(),
                            network: Arc::default(),
                        })),
                        Arc::new(workspace::FileState::default()),
                        RunCapabilities::user(None),
                    )
                    .collect::<Vec<_>>()
                    .await;
                assert!(
                    matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
                    "{events:?}"
                );
                assert_eq!(
                    events
                        .iter()
                        .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. })),
                    !first_granted
                );
            }
            assert_eq!(
                first_calls.lock().unwrap().len(),
                usize::from(first_granted)
            );
            assert!(later_calls.lock().unwrap().is_empty());
            for request in requests.lock().unwrap().iter() {
                assert_eq!(
                    tool_names(request).contains(&"ext__wide__tool01"),
                    !unattended || first_granted
                );
            }
        }
    }
}

#[tokio::test]
async fn direct_gate_and_declarations_share_the_compiled_grant_snapshot() {
    for granted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut host = WideHost::new(2);
        let live_granted = Arc::new(std::sync::atomic::AtomicBool::new(granted));
        host.live_granted = Some(Arc::clone(&live_granted));
        host.read_only_hint = !granted;
        let calls = Arc::clone(&host.calls);
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![vec![("ext__wide__tool01", "{}".to_owned())], Vec::new()],
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap()
        .with_tool_host(Arc::new(host));
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let plan = tokio::task::spawn_blocking(move || {
            plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
                &runtime, workspace,
            ))
            .unwrap()
        })
        .await
        .unwrap();
        live_granted.store(!granted, std::sync::atomic::Ordering::SeqCst);
        let events = plan
            .run(RunCommand::new("perform the requested operation"))
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RunEvent::Completed)),
            "{events:?}"
        );
        assert_eq!(calls.lock().unwrap().len(), usize::from(granted));
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(tool_names(request).contains(&"ext__wide__tool01"), granted);
            let prompt = request.system().unwrap();
            assert!(prompt.contains("This unattended run"), "{prompt}");
            assert!(!prompt.contains("- Do the work. Decide what done looks like"));
        }
    }
}

#[tokio::test]
async fn run_messages_uses_grants_at_first_poll_for_gate_and_declarations() {
    for granted in [false, true] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut host = WideHost::new(2);
        let live_granted = Arc::new(std::sync::atomic::AtomicBool::new(granted));
        host.live_granted = Some(Arc::clone(&live_granted));
        host.read_only_hint = !granted;
        let calls = Arc::clone(&host.calls);
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![vec![("ext__wide__tool01", "{}".to_owned())], Vec::new()],
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap()
        .with_tool_host(Arc::new(host));
        let events = runtime.run_messages(vec![Message::user("perform the requested operation")]);
        live_granted.store(!granted, std::sync::atomic::Ordering::SeqCst);
        let events = events.collect::<Vec<_>>().await;
        assert!(
            matches!(events.last(), Some(RunEvent::Completed)),
            "{events:?}"
        );
        assert_eq!(calls.lock().unwrap().len(), usize::from(!granted));
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(tool_names(request).contains(&"ext__wide__tool01"), !granted);
            let prompt = request.system().unwrap();
            assert!(prompt.contains("This unattended run"), "{prompt}");
            assert!(!prompt.contains("- Do the work. Decide what done looks like"));
        }
    }
}

#[tokio::test]
async fn read_only_root_hides_write_child_authority_and_denies_injection() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let spawner = Arc::new(StubSpawner::new(
        SpawnAgentOutcome {
            content: "must not run".to_owned(),
            is_error: false,
            spend: SpawnAgentSpend::NONE,
            session_id: None,
            detached: false,
        },
        Arc::new(Mutex::new(Vec::new())),
    ));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![(
                    "spawn_agent",
                    r#"{"task":"edit a file","authority":"write","reviewer_model":"test-model"}"#
                        .to_owned(),
                )],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap()
    .with_delegation(DelegationRoster {
        write_children: true,
        ..DelegationRoster::default()
    });
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(&runtime, workspace))
            .unwrap()
    })
    .await
    .unwrap();
    let events = plan
        .execute(
            vec![Message::user("inspect")],
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::ReadOnly,
                tools: Default::default(),
                network: Arc::clone(&plan.runtime.network),
            }),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>))
                .read_only(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    assert!(spawner.requests.lock().unwrap().is_empty());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. }))
    );
    for request in requests.lock().unwrap().iter() {
        let spec = request
            .tools()
            .iter()
            .find(|spec| spec.name() == "spawn_agent")
            .unwrap();
        let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
        assert!(schema["properties"]["authority"].is_null(), "{schema}");
        let prompt = request.system().unwrap();
        assert!(prompt.contains("read-only sub-agent"), "{prompt}");
        assert!(!prompt.contains("Choose authority: write"), "{prompt}");
    }
}

#[tokio::test]
async fn direct_requests_declare_only_executable_tools_and_state_authority() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![Vec::new()],
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
        .run(RunCommand::new("make the requested change"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );
    let requests = requests.lock().unwrap();
    let names = tool_names(&requests[0]);
    for denied in [
        "edit_file",
        "write_file",
        "shell",
        "exec",
        "ask_user",
        "fetch",
    ] {
        assert!(
            !names.contains(&denied),
            "denied schema {denied}: {names:?}"
        );
    }
    assert!(names.contains(&"read_file"));
    let prompt = requests[0].system().unwrap();
    assert!(
        !prompt.contains("- Do the work. Decide what done looks like"),
        "{prompt}"
    );
    assert!(prompt.contains("This unattended run"), "{prompt}");
}

#[tokio::test]
async fn captured_subset_prompts_only_instruct_declared_tools() {
    for exposed in [
        Vec::new(),
        vec!["read_file".to_owned()],
        vec!["search".to_owned()],
        vec!["tree".to_owned()],
        vec!["shell".to_owned()],
        vec!["exec".to_owned()],
        vec!["edit_file".to_owned()],
        vec!["write_file".to_owned()],
        vec!["read_file".to_owned(), "write_file".to_owned()],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![Vec::new()],
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap();
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let expected = exposed.clone();
        let plan = tokio::task::spawn_blocking(move || {
            plan::CompiledAgentPlan::compile_blocking(
                plan::AgentProfile::embedded(&runtime, workspace).with_exposed_tools(exposed),
            )
            .unwrap()
        })
        .await
        .unwrap();
        let events = plan
            .execute(
                vec![Message::user("review the available context")],
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    tools: Default::default(),
                    network: Arc::clone(&plan.runtime.network),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let mut declared = expected.iter().map(String::as_str).collect::<Vec<_>>();
        if declared
            .iter()
            .any(|name| matches!(*name, "shell" | "exec"))
        {
            declared.push("command_status");
        }
        assert_eq!(tool_names(&requests[0]), declared);
        let prompt = requests[0].system().unwrap();
        if !expected.iter().any(|name| name == "read_file") {
            assert!(
                !prompt.contains("with ranges"),
                "absent read_file ranges guidance: {prompt}"
            );
        }
        if expected == ["search"] || expected == ["tree"] {
            assert!(
                requests[0].tools()[0]
                    .description()
                    .contains("path and include_ignored=true")
            );
        }
        if expected == ["read_file", "write_file"] {
            for absent in ["old matches", "note=unread", "extend old", "anchor"] {
                assert!(!prompt.contains(absent), "{absent}: {prompt}");
            }
        }
        if expected == ["write_file"] {
            let description = requests[0].tools()[0].description();
            assert!(description.contains("create_only"), "{description}");
            assert!(description.contains("independently known"), "{description}");
            assert!(!description.contains("read earlier"), "{description}");
        }
        if expected == ["exec"] {
            assert!(
                !requests[0].tools()[0]
                    .description()
                    .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .any(|token| token == "shell")
            );
        }
        if expected == ["shell"] {
            let shell = &requests[0].tools()[0];
            for absent in ["read_file", "search", "tree"] {
                assert!(
                    !shell
                        .description()
                        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                        .any(|token| token == absent),
                    "absent {absent} in shell description: {}",
                    shell.description()
                );
            }
        }
        for name in [
            "read_file",
            "tree",
            "search",
            "edit_file",
            "write_file",
            "shell",
            "exec",
            "ask_user",
            "fetch",
            "read_tool_result",
            "search_history",
        ] {
            if !expected.iter().any(|present| present == name) {
                assert!(
                    !prompt
                        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                        .any(|token| token == name),
                    "absent {name} advertised: {prompt}"
                );
            }
        }
    }
}

#[tokio::test]
async fn explicit_external_exposure_without_a_selector_is_fully_callable() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let host = WideHost::new(40);
    let calls = Arc::clone(&host.calls);
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![vec![("ext__wide__tool07", "{}".to_owned())], Vec::new()],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(host));
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(
            plan::AgentProfile::embedded(&runtime, workspace)
                .with_exposed_tools((0..40).map(|i| format!("ext__wide__tool{i:02}")).collect()),
        )
        .unwrap()
    })
    .await
    .unwrap();
    let events = plan
        .run(RunCommand::new("use tool seven"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );
    assert_eq!(plan.catalog().exposure(), catalog::Exposure::Full);
    assert_eq!(calls.lock().unwrap().as_slice(), ["ext__wide__tool07"]);
    for request in requests.lock().unwrap().iter() {
        let names = tool_names(request);
        assert_eq!(names.len(), 40);
        assert!(names.iter().all(|name| name.starts_with("ext__wide__")));
    }
}

#[tokio::test]
async fn explicit_exposure_preserves_catalog_schema_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let host = WideHost::new(2);
    let calls = Arc::clone(&host.calls);
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![
                    ("ext__wide__tool01", "{}".to_owned()),
                    ("ext__wide__tool00", "{}".to_owned()),
                ],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap();
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        let mut snapshot = plan::HostSnapshot::capture_blocking(Arc::new(host));
        snapshot.catalog.tools[1].spec = qq_provider::ToolSpec::new(
            "ext__wide__tool01",
            "Oversized tool",
            serde_json::json!({"description": "x".repeat(16 * 1024)}),
        );
        plan::CompiledAgentPlan::compile_blocking(
            plan::AgentProfile::embedded(&runtime, workspace)
                .with_host(snapshot)
                .with_exposed_tools(vec![
                    "ext__wide__tool00".to_owned(),
                    "ext__wide__tool01".to_owned(),
                ]),
        )
        .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        plan.catalog().names().collect::<Vec<_>>(),
        ["ext__wide__tool00"]
    );
    assert_eq!(plan.catalog().excluded().len(), 1);
    assert!(matches!(
        plan.catalog().excluded()[0].reason,
        catalog::ExclusionReason::SchemaTooLarge { bytes } if bytes > 16 * 1024
    ));
    let events = plan
        .run(RunCommand::new("use both tools"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );
    assert_eq!(calls.lock().unwrap().as_slice(), ["ext__wide__tool00"]);
    let requests = requests.lock().unwrap();
    assert_eq!(tool_names(&requests[0]), ["ext__wide__tool00"]);
    assert!(requests[1].messages().last().unwrap().content().iter().any(|block| matches!(block,
        ContentBlock::ToolResult { content, is_error: true, .. } if content.contains("unknown tool")
    )));
}

#[tokio::test]
async fn explicit_external_exposure_with_a_selector_remains_progressive() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let host = WideHost::new(40);
    let calls = Arc::clone(&host.calls);
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![
                    (
                        catalog::SELECT_TOOLS_TOOL,
                        r#"{"query":"deploy service","limit":1}"#.to_owned(),
                    ),
                    ("ext__wide__tool07", "{}".to_owned()),
                ],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(host));
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        let mut names: Vec<_> = (0..40).map(|i| format!("ext__wide__tool{i:02}")).collect();
        names.push(catalog::SELECT_TOOLS_TOOL.to_owned());
        plan::CompiledAgentPlan::compile_blocking(
            plan::AgentProfile::embedded(&runtime, workspace).with_exposed_tools(names),
        )
        .unwrap()
    })
    .await
    .unwrap();
    let events = plan
        .run(RunCommand::new("use tool seven"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );
    assert_eq!(plan.catalog().exposure(), catalog::Exposure::Progressive);
    assert_eq!(calls.lock().unwrap().as_slice(), ["ext__wide__tool07"]);
    let requests = requests.lock().unwrap();
    assert_eq!(tool_names(&requests[0]), [catalog::SELECT_TOOLS_TOOL]);
    assert_eq!(
        tool_names(&requests[1]),
        [catalog::SELECT_TOOLS_TOOL, "ext__wide__tool07"]
    );
}

#[tokio::test]
async fn progressive_exposure_pins_selected_tools_for_the_rest_of_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let host = WideHost::new(40);
    let calls = Arc::clone(&host.calls);
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![("ext__wide__tool07", "{}".to_owned())],
                vec![
                    (
                        catalog::SELECT_TOOLS_TOOL,
                        r#"{"query":"deploy service","limit":2}"#.to_owned(),
                    ),
                    ("ext__wide__tool07", "{}".to_owned()),
                ],
                vec![(
                    catalog::SELECT_TOOLS_TOOL,
                    r#"{"query":"widget helper","limit":8}"#.to_owned(),
                )],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(host));

    let events = runtime
        .run_in_workspace(RunCommand::new("deploy it"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );

    let requests = requests.lock().unwrap();
    let first = tool_names(&requests[0]);
    assert!(first.contains(&catalog::SELECT_TOOLS_TOOL));
    assert!(!first.iter().any(|name| name.starts_with("ext__")));
    let system = requests[0].system().unwrap();
    assert!(system.contains("External tools (progressive)"));
    assert!(system.contains("ext__wide__tool07 — Deploy the service"));
    assert!(system.contains("host wide: 40 tools"));
    let first_results = requests[1].messages().last().unwrap().content();
    let unpinned = first_results.iter().find_map(|block| match block {
        ContentBlock::ToolResult {
            call_id,
            content,
            is_error,
        } if call_id == "call_0_0" => Some((content.clone(), *is_error)),
        _ => None,
    });
    let (message, is_error) = unpinned.unwrap();
    assert!(is_error && message.contains("select_tools"), "{message}");
    assert_eq!(tool_names(&requests[1]), first, "nothing pinned yet");
    let second_results = requests[2].messages().last().unwrap().content();
    let selection = second_results.iter().find_map(|block| match block {
        ContentBlock::ToolResult {
            call_id,
            content,
            is_error: false,
        } if call_id == "call_1_0" => {
            Some(serde_json::from_str::<serde_json::Value>(content).unwrap())
        }
        _ => None,
    });
    let selection = selection.unwrap();
    assert_eq!(selection["pinned"][0], "ext__wide__tool07");
    assert_eq!(calls.lock().unwrap().as_slice(), ["ext__wide__tool07"]);
    let third = tool_names(&requests[2]);
    assert!(third.contains(&"ext__wide__tool07"));
    assert_eq!(third.len(), first.len() + 1);
    let fourth = tool_names(&requests[3]);
    assert_eq!(fourth.len(), first.len() + 9);
    let mut deduped = fourth.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), fourth.len());
}

#[tokio::test]
async fn read_only_recovery_omits_granted_external_pins_and_index() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![Vec::new()],
            requests: Arc::clone(&requests),
        },
        "test-model",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(WideHost::new(40)));
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(&runtime, workspace))
            .unwrap()
    })
    .await
    .unwrap();
    let prior = serde_json::to_string(&catalog::SelectToolsResult {
        pinned: vec!["ext__wide__tool03".to_owned()],
        already_pinned: Vec::new(),
        refused: Vec::new(),
        remaining_pin_slots: 31,
    })
    .unwrap();
    let events = plan
        .execute(
            vec![
                Message::user("continue"),
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::tool_call(
                        "c1".to_owned(),
                        catalog::SELECT_TOOLS_TOOL.to_owned(),
                        &serde_json::json!({"query":"widgets"}),
                    )],
                ),
                Message::tool_results(vec![ContentBlock::ToolResult {
                    call_id: "c1".to_owned(),
                    content: prior,
                    is_error: false,
                }]),
            ],
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::ReadOnly,
                tools: Default::default(),
                network: Arc::clone(&plan.runtime.network),
            }),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).read_only(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{events:?}"
    );
    let requests = requests.lock().unwrap();
    assert!(
        !tool_names(&requests[0])
            .iter()
            .any(|name| name.starts_with("ext__"))
    );
    let system = requests[0].system().unwrap();
    assert!(!system.contains("ext__wide__tool03"));
    assert!(system.contains("This run cannot change files or run commands"));
}

#[tokio::test]
async fn recovered_runs_re_pin_from_prior_select_tools_results() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![Vec::new()],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(WideHost::new(40)));
    let prior = serde_json::to_string(&catalog::SelectToolsResult {
        pinned: vec![
            "ext__wide__tool03".to_owned(),
            "ext__wide__tool99".to_owned(),
            "read_file".to_owned(),
        ],
        already_pinned: Vec::new(),
        refused: Vec::new(),
        remaining_pin_slots: 30,
    })
    .unwrap();
    let messages = vec![
        Message::user("continue"),
        Message::new(
            Role::Assistant,
            vec![ContentBlock::tool_call(
                "c1".to_owned(),
                catalog::SELECT_TOOLS_TOOL.to_owned(),
                &serde_json::json!({"query": "x"}),
            )],
        ),
        Message::tool_results(vec![ContentBlock::ToolResult {
            call_id: "c1".to_owned(),
            content: prior,
            is_error: false,
        }]),
        Message::user("go on"),
    ];
    let events = runtime
        .run_messages_in_workspace(messages, directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let names = tool_names(&requests[0]);
    assert!(names.contains(&"ext__wide__tool03"), "{names:?}");
    assert!(
        !names.contains(&"ext__wide__tool99"),
        "unknown names are not pinned"
    );
    assert_eq!(names.iter().filter(|n| **n == "read_file").count(), 1);
}

#[tokio::test]
async fn host_failures_are_typed_tool_errors_and_small_catalogs_expose_fully() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut host = WideHost::new(3);
    host.failure = Some(HostCallError::Timeout);
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![vec![("ext__wide__tool01", "{}".to_owned())], Vec::new()],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap()
    .with_tool_host(Arc::new(host));
    let events = runtime
        .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    let names = tool_names(&requests[0]);
    assert!(names.contains(&"ext__wide__tool01"));
    assert!(
        !names.contains(&catalog::SELECT_TOOLS_TOOL),
        "full exposure needs no selector"
    );
    assert!(!requests[0].system().unwrap().contains("progressive"));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
            if result == &HostCallError::Timeout.to_string()
    )));
}

#[tokio::test]
async fn a_model_loads_a_matching_skill_with_arguments_and_reads_the_script_beside_it() {
    let directory = tempfile::tempdir().unwrap();
    for (path, content) in [
        (
            ".qq/skills/greet/SKILL.md",
            "---\nname: greet\ndescription: Greet someone by name\n---\nRun scripts/greet.sh $ARGUMENTS.\n",
        ),
        (".qq/skills/greet/scripts/greet.sh", "echo \"hello $1\"\n"),
    ] {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![(
                    "load_skill",
                    r#"{"name":"greet","arguments":"Ada"}"#.to_owned(),
                )],
                vec![(
                    "read_file",
                    r#"{"path":".qq/skills/greet/scripts/greet.sh"}"#.to_owned(),
                )],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_in_workspace(RunCommand::new("greet Ada"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );
    let requests = requests.lock().unwrap();
    assert!(
        requests[0]
            .system()
            .unwrap()
            .contains("call load_skill with its name before acting")
    );
    assert!(
        requests[0]
            .system()
            .unwrap()
            .contains("- greet (skill): Greet someone by name")
    );
    let result = |request: &ModelRequest| {
        request
            .messages()
            .last()
            .unwrap()
            .content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    content,
                    is_error: false,
                    ..
                } => Some(content.clone()),
                _ => None,
            })
            .expect("a successful tool result")
    };
    let loaded = result(&requests[1]);
    assert!(
        loaded.contains("Skill directory: .qq/skills/greet\n"),
        "{loaded}"
    );
    assert!(
        loaded.contains("- .qq/skills/greet/scripts/greet.sh\n"),
        "{loaded}"
    );
    assert!(loaded.contains("Run scripts/greet.sh Ada.\n"), "{loaded}");
    assert!(result(&requests[2]).contains("echo \"hello $1\""));
}

#[tokio::test]
async fn disclosed_skills_are_listed_and_loadable_only_for_guidance_capable_runs() {
    let directory = tempfile::tempdir().unwrap();
    for (path, content) in [
        (
            ".qq/skills/deploy/SKILL.md",
            "---\ndescription: How to deploy safely\n---\nRun the deploy checklist.\n",
        ),
        (".claude/skills/hidden/SKILL.md", "Compat only.\n"),
    ] {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TurnScript {
            turns: vec![
                vec![
                    ("load_skill", r#"{"name":"deploy"}"#.to_owned()),
                    ("load_skill", r#"{"name":"hidden"}"#.to_owned()),
                ],
                Vec::new(),
            ],
            requests: Arc::clone(&requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let events = runtime
        .run_in_workspace(RunCommand::new("deploy"), directory.path().to_owned())
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{events:?}"
    );
    {
        let requests = requests.lock().unwrap();
        let system = requests[0].system().unwrap();
        assert!(system.contains("- deploy (skill): How to deploy safely"));
        assert!(
            !system.contains("hidden"),
            ".claude roots are not disclosed"
        );
        assert!(
            !system.contains("Run the deploy checklist"),
            "bodies load on demand"
        );
        assert!(tool_names(&requests[0]).contains(&"load_skill"));
        let results = requests[1].messages().last().unwrap().content();
        let loaded = results.iter().any(|block| matches!(
        block,
        ContentBlock::ToolResult { content, is_error: false, .. }
            if content.contains("Run the deploy checklist") && content.contains("Selected skill `deploy`")
    ));
        assert!(loaded);
        let hidden_refused = results.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("unknown command or skill /hidden")
            )
        });
        assert!(hidden_refused);
    }

    let plan = plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
        &runtime,
        std::fs::canonicalize(directory.path()).unwrap(),
    ))
    .unwrap();
    let restricted_requests = Arc::new(Mutex::new(Vec::new()));
    let restricted = Runtime::new(
        TurnScript {
            turns: vec![Vec::new()],
            requests: Arc::clone(&restricted_requests),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let restricted_plan = plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
        &restricted,
        std::fs::canonicalize(directory.path()).unwrap(),
    ))
    .unwrap();
    let events = restricted_plan
        .execute(
            vec![Message::user("summarize")],
            RunCancellation::new(),
            Arc::new(StaticPolicyGate::fixed(ApprovalMode::Ask)),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::restricted(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let restricted_requests = restricted_requests.lock().unwrap();
    assert!(
        !restricted_requests[0]
            .system()
            .unwrap()
            .contains("Skills and commands.")
    );
    assert!(!tool_names(&restricted_requests[0]).contains(&"load_skill"));
    assert_eq!(plan.descriptor().skills.disclosed, 1);
    assert_eq!(plan.descriptor().skills.indexed, 2);
}
