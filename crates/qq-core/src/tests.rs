#[cfg(unix)]
mod audit_regression;
mod checkpoints;
mod delegation;
mod external_tools;
mod instructions;
mod loop_guard;
mod mcp;
mod progress;
mod retries;
mod summaries;
mod t20;
mod tool_loop;
mod truncation;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{StreamExt, stream};
use qq_protocol::ReasoningKind;
use qq_provider::{ProviderError, ProviderStream};

use sha2::{Digest, Sha256};

use super::*;

#[tokio::test]
async fn public_terminal_paths_release_the_internal_stream_before_yielding() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let cases = [
        RuntimeEvent::Completed { final_output: None },
        RuntimeEvent::Failed {
            kind: RunFailureKind::Policy,
            message: "failed".into(),
        },
        RuntimeEvent::Paused {
            pause: Box::new(qq_protocol::RunPause::NoProgress {
                turn_ordinal: 1,
                slices: 1,
                calls: 1,
            }),
        },
        RuntimeEvent::BudgetExhausted {
            exhaustion: qq_protocol::BudgetExhaustion {
                limit: qq_protocol::BudgetLimitKind::Duration,
                final_response: false,
                message: "budget".into(),
            },
        },
        RuntimeEvent::Prepared {
            turn_ordinal: 1,
            identity: None,
            static_prefix: runtime::PreparedStaticPrefix::new(
                qq_protocol::ContentHash::from_bytes([0; 32]),
                None,
            ),
            weight: runtime::PreparedRequestWeight {
                max_output_tokens: 128,
                system_bytes: 1_000_000,
                tool_schema_bytes: 0,
                reducible_message_bytes: 0,
                irreducible_message_bytes: 0,
                compatible_input_tokens: None,
            },
        },
    ];
    for event in cases {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let events: RuntimeStream = Box::pin(stream! {
            let _guard = guard;
            yield event;
            std::future::pending::<()>().await;
        });
        let mut public = public_run_stream(events, Some(256));
        assert!(matches!(
            public.next().await,
            Some(RunEvent::Completed | RunEvent::Failed { .. })
        ));
        assert!(
            dropped.load(Ordering::SeqCst),
            "terminal public stream retained internal execution"
        );
        assert!(public.next().await.is_none());
    }
}

struct ScriptedProvider {
    request: Arc<Mutex<Option<ModelRequest>>>,
    fails: bool,
}

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        *self.request.lock().unwrap() = Some(request);

        if self.fails {
            return Box::pin(stream::once(async {
                Err(ProviderError::Transport("offline".to_owned()))
            }));
        }

        Box::pin(stream::iter([
            Ok(ProviderEvent::OutputTextDelta {
                text: "hel".to_owned(),
            }),
            Ok(ProviderEvent::OutputTextDelta {
                text: "lo".to_owned(),
            }),
            Ok(ProviderEvent::RefusalDelta {
                text: " cannot continue".to_owned(),
            }),
            Ok(ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }),
            }),
        ]))
    }
}

#[tokio::test]
async fn compiled_effort_reaches_provider_and_distinguishes_omission() {
    struct EffortProvider(Arc<Mutex<Vec<Option<qq_provider::ReasoningEffort>>>>);
    impl Provider for EffortProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut seen = self.0.lock().unwrap();
            seen.push(request.reasoning_effort());
            if seen.len() == 1 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "read".into(),
                        name: "tree".into(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "read".into(),
                        json: r#"{"path":".","depth":1}"#.into(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id: "read".into() }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".into(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }
    for effort in [
        None,
        Some(qq_provider::ReasoningEffort::None),
        Some(qq_provider::ReasoningEffort::High),
    ] {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = Runtime::new(EffortProvider(Arc::clone(&captured)), "test", 128).unwrap();
        if let Some(effort) = effort {
            runtime = runtime.with_reasoning_effort(effort);
        }
        let directory = tempfile::tempdir().unwrap();
        let plan = crate::plan::CompiledAgentPlan::compile_blocking(
            crate::plan::AgentProfile::embedded(&runtime, directory.path().to_owned()),
        )
        .unwrap();
        assert_eq!(plan.descriptor().reasoning_effort, effort);
        let _events = plan
            .run(RunCommand::new("answer"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(*captured.lock().unwrap(), vec![effort, effort]);
    }
}

#[tokio::test]
async fn the_transcript_is_shared_with_each_request_and_grown_in_place() {
    struct TwoTurnProvider {
        seen: Arc<Mutex<Vec<(usize, usize, usize)>>>,
    }

    impl Provider for TwoTurnProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut seen = self.seen.lock().unwrap();
            let turn = seen.len();
            let shared = request.shared_messages();
            seen.push((
                Arc::as_ptr(shared).addr(),
                shared.len(),
                Arc::strong_count(shared),
            ));
            drop(request);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "c1".to_owned(),
                        name: "tree".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "c1".to_owned(),
                        json: r#"{"path":".","depth":1}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "c1".to_owned(),
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

    let directory = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(
        TwoTurnProvider {
            seen: Arc::clone(&seen),
        },
        "gpt-test",
        256,
    )
    .unwrap();

    let events = runtime
        .run_in_workspace(
            RunCommand::new("List the workspace."),
            directory.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(RunEvent::Completed)));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let (first, first_len, first_holders) = &seen[0];
    let (second, second_len, second_holders) = &seen[1];
    assert_eq!(*first_len, 1, "turn one sees the prompt alone");
    assert_eq!(
        *second_len, 3,
        "turn two sees prompt, assistant tool call, tool results"
    );
    assert_eq!((*first_holders, *second_holders), (2, 2));
    assert_eq!(
        first, second,
        "the transcript must be grown in place between turns"
    );
}

#[tokio::test]
async fn prefix_plus_suffix_digest_equals_the_full_prompt_digest() {
    struct SystemCapture(Arc<Mutex<Vec<Arc<str>>>>);

    impl Provider for SystemCapture {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.0.lock().unwrap().push(Arc::from(
                request.system().expect("every run has a system prompt"),
            ));
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "{\"ok\":true}".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "Keep every change small.\n",
    )
    .unwrap();
    let skill = directory.path().join(".qq/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "Review for regressions.\n").unwrap();
    let systems = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new(SystemCapture(Arc::clone(&systems)), "test-model", 256).unwrap();
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let plan = tokio::task::spawn_blocking(move || {
        plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(&runtime, workspace))
            .unwrap()
    })
    .await
    .unwrap();
    let contract = Arc::new(
        output::CompiledOutputSchema::compile(&qq_protocol::OutputContract {
            schema: serde_json::json!({"type": "object"}),
            repair_turns: 0,
        })
        .unwrap(),
    );

    let cases: Vec<(RunCapabilities, &str)> = vec![
        (
            RunCapabilities::user(None).with_output(Some(Arc::clone(&contract))),
            "/review the change",
        ),
        (
            RunCapabilities {
                allow_guidance: false,
                ..RunCapabilities::user(None)
            }
            .read_only(),
            "summarize",
        ),
        (
            RunCapabilities {
                allow_guidance: false,
                ..RunCapabilities::user(None)
            }
            .without_tools(),
            "compact",
        ),
        (RunCapabilities::user(None), "hello"),
    ];
    let mut identities = Vec::new();
    for (capabilities, prompt) in cases {
        let events = plan
            .execute(
                vec![Message::user(prompt)],
                RunCancellation::new(),
                Arc::new(StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
                Arc::new(workspace::FileState::default()),
                capabilities,
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let identity = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::Prepared {
                    identity: Some(identity),
                    ..
                } => Some(Arc::clone(identity)),
                _ => None,
            })
            .expect("the first turn publishes the prompt identity");
        identities.push(identity);
    }

    let systems = systems.lock().unwrap();
    assert_eq!(systems.len(), 4);
    for (system, identity) in systems.iter().zip(&identities) {
        let full = ContentHash::from_bytes(Sha256::digest(system.as_bytes()).into());
        assert_eq!(
            identity.system_prompt_hash,
            Some(full),
            "the continued prefix digest must equal the whole-prompt digest"
        );
    }
    assert!(systems[0].contains("Selected skill `review`"));
    assert!(
        systems[0].ends_with("\n```\n"),
        "output contract closes the prompt"
    );
    assert!(systems[0].contains("## Output contract"));
    assert!(!systems[1].contains("Selected skill"));
    assert!(
        !systems[1].contains("edit_file, "),
        "read-only runs are not offered edits"
    );
    assert!(!systems[2].contains("Available tools: read_file"));
    assert!(systems[3].contains("--- BEGIN WORKSPACE INSTRUCTIONS ---"));
    let expected = runtime::agent_system_prompt(
        &plan.catalog.base_specs(&catalog::StaticFilter {
            spawn_agent: false,
            search_history: false,
            update_goal: false,
            read_tool_result: false,
            load_skill: true,
            read_only: false,
            unattended: false,
        }),
        runtime::PromptSections {
            tool_index: plan.catalog.index_text().map(Arc::as_ref),
            roster: plan.roster_text.as_deref(),
            skill_index: plan.skills.disclosure_text(),
            subagent: None,
            ..runtime::PromptSections::default()
        },
        &plan.instructions,
        plan.persona.as_deref(),
        None,
    );
    assert_eq!(systems[3].as_ref(), expected.as_str());
    assert_ne!(systems[1], systems[2]);
    assert_ne!(systems[1], systems[3]);
}

#[tokio::test]
async fn direct_run_rejects_a_known_context_overflow_before_provider_work() {
    struct CountingProvider(Arc<AtomicUsize>);

    impl Provider for CountingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::AcqRel);
            Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
        }
    }

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new(CountingProvider(Arc::clone(&provider_calls)), "test", 1)
        .unwrap()
        .with_context_window(Some(1));

    let events = runtime
        .run(RunCommand::new("work"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(provider_calls.load(Ordering::Acquire), 0);
    assert!(matches!(
        events.last(),
        Some(RunEvent::Failed {
            kind: RunFailureKind::Policy,
            message,
        }) if message.contains("1-token window")
    ));
}

#[tokio::test]
async fn maps_provider_events_to_protocol_events() {
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
        .run(RunCommand::new("say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![
            RunEvent::Started,
            RunEvent::ActivityChanged {
                activity: RunActivity::WaitingForProvider,
            },
            RunEvent::ActivityChanged {
                activity: RunActivity::GeneratingResponse,
            },
            RunEvent::OutputTextDelta {
                text: "hel".to_owned()
            },
            RunEvent::OutputTextDelta {
                text: "lo".to_owned()
            },
            RunEvent::RefusalDelta {
                text: " cannot continue".to_owned()
            },
            RunEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }
            },
            RunEvent::Completed,
        ]
    );

    let request = captured.lock().unwrap().clone().unwrap();
    assert_eq!(request.model(), "gpt-test");
    assert_eq!(request.max_output_tokens(), 256);
    assert_eq!(request.messages(), [Message::user("say hello")]);
}

#[tokio::test]
async fn maps_reasoning_lifecycle_without_joining_answer_text() {
    struct ReasoningProvider;

    impl Provider for ReasoningProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ReasoningStarted {
                    kind: qq_provider::ReasoningKind::Summary,
                }),
                Ok(ProviderEvent::ReasoningDelta {
                    kind: qq_provider::ReasoningKind::Summary,
                    text: "checking constraints".to_owned(),
                }),
                Ok(ProviderEvent::ReasoningCompleted {
                    kind: qq_provider::ReasoningKind::Summary,
                }),
                Ok(ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    let events = Runtime::new(ReasoningProvider, "gpt-test", 256)
        .unwrap()
        .run(RunCommand::new("solve it"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![
            RunEvent::Started,
            RunEvent::ActivityChanged {
                activity: RunActivity::WaitingForProvider,
            },
            RunEvent::ActivityChanged {
                activity: RunActivity::Reasoning,
            },
            RunEvent::ReasoningStarted {
                kind: ReasoningKind::Summary,
            },
            RunEvent::ReasoningDelta {
                kind: ReasoningKind::Summary,
                text: "checking constraints".to_owned(),
            },
            RunEvent::ReasoningCompleted {
                kind: ReasoningKind::Summary,
            },
            RunEvent::ActivityChanged {
                activity: RunActivity::GeneratingResponse,
            },
            RunEvent::OutputTextDelta {
                text: "answer".to_owned(),
            },
            RunEvent::Completed,
        ]
    );
}

#[tokio::test]
async fn passes_multi_turn_context_to_the_provider() {
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

    runtime
        .run_messages(vec![
            Message::user("hey"),
            Message::assistant("Hello!"),
            Message::user("what was my first message?"),
        ])
        .collect::<Vec<_>>()
        .await;

    let request = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        request.messages(),
        [
            Message::user("hey"),
            Message::assistant("Hello!"),
            Message::user("what was my first message?"),
        ]
    );
}

pub(crate) struct MockMcpRegistry {
    specs: Vec<qq_provider::ToolSpec>,
    grants: Vec<String>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    result: Result<HostToolResult, HostCallError>,
}

impl MockMcpRegistry {
    fn returning(result: HostToolResult) -> Self {
        Self {
            specs: vec![qq_provider::ToolSpec::new(
                "mcp__srv__ping",
                "Ping the fixture server.",
                serde_json::json!({"type": "object"}),
            )],
            grants: Vec::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
            result: Ok(result),
        }
    }
}

impl ExternalToolHost for MockMcpRegistry {
    fn name(&self) -> &str {
        "mcp"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: 1,
            tools: self
                .specs
                .iter()
                .cloned()
                .map(|spec| HostTool {
                    spec,
                    hints: ToolHints::default(),
                })
                .collect(),
            readiness: HostReadiness::Ready,
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        generation == 1
    }

    fn config_grants(&self) -> Vec<String> {
        self.grants.clone()
    }

    fn call(&self, name: String, arguments: String, _cancelled: RunCancellation) -> HostCallFuture {
        self.calls.lock().unwrap().push((name, arguments.clone()));
        let result = self.result.clone();
        Box::pin(async move { result })
    }

    fn readiness(&self) -> HostReadiness {
        HostReadiness::Ready
    }

    fn shutdown(&self) -> HostShutdownFuture {
        Box::pin(std::future::ready(()))
    }
}

#[cfg(unix)]
#[test]
fn ab1_inherited_path_shadowed_read_counts_as_work_and_audit() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("QQ_CONTEXT_SHADOW_WORKER").is_none() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let program = bin.join("grep");
        std::fs::write(
            &program,
            "#!/bin/sh\nprintf changed > written.txt\nprintf 'match\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::test_support::run_worker(
            "tests::ab1_inherited_path_shadowed_read_counts_as_work_and_audit",
            "QQ_CONTEXT_SHADOW_WORKER",
            |command| {
                command
                    .env("QQ_CONTEXT_SHADOW_ROOT", &workspace)
                    .env("PATH", &bin);
            },
        );
        return;
    }
    let path = std::path::PathBuf::from(std::env::var_os("QQ_CONTEXT_SHADOW_ROOT").unwrap());
    let arguments = r#"{"program":"grep","args":["-n","x","f"]}"#.to_owned();
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(tools::execute(
            workspace::Workspace::open(&path).unwrap(),
            Arc::new(workspace::FileState::default()),
            "exec".to_owned(),
            arguments.clone(),
            RunCancellation::new(),
            None,
            tools::ToolTasks::default(),
            Arc::new(runtime::ShellPolicy::default()),
            Arc::new(tools::network::NetworkPolicy::default()),
        ));
    assert!(!output.is_error, "{}", output.model_text);
    assert_eq!(std::fs::read(path.join("written.txt")).unwrap(), b"changed");
    let call = RuntimeToolCall {
        id: ToolCallId::from_bytes([42; 16]),
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "shadow".to_owned(),
        name: "exec".to_owned(),
        arguments,
        effect: catalog::EffectClass::Shell,
        rejection: None,
    };
    assert!(
        runtime::is_progress(&call, None, &output),
        "unverified executable must count as work"
    );
    let mut triggers = runtime::AuditTriggers::default();
    note_audited_action(&mut triggers, &mut Vec::new(), false, &call, &output);
    assert!(
        triggers.non_read_shell,
        "unverified mutation must not suppress audit"
    );
}

type SpawnedTasks = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct StubSpawner {
    outcome: SpawnAgentOutcome,
    tasks: SpawnedTasks,
    requests: Arc<Mutex<Vec<SpawnRequest>>>,
}

impl StubSpawner {
    fn new(outcome: SpawnAgentOutcome, tasks: SpawnedTasks) -> Self {
        Self {
            outcome,
            tasks,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl SubagentSpawner for StubSpawner {
    fn acknowledge(&self, _: ToolCallId) {}
    fn drain(&self) -> runtime::ChildDrainFuture {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn spawn(&self, request: SpawnRequest) -> SpawnAgentFuture {
        self.tasks
            .lock()
            .unwrap()
            .push((request.task.clone(), request.model.clone()));
        self.requests.lock().unwrap().push(request);
        let outcome = self.outcome.clone();
        Box::pin(std::future::ready(outcome))
    }
}

struct WideHost {
    count: usize,
    generation: Arc<std::sync::atomic::AtomicU64>,
    calls: Arc<Mutex<Vec<String>>>,
    failure: Option<HostCallError>,
    read_only_hint: bool,
    granted: bool,
    live_granted: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl WideHost {
    fn new(count: usize) -> Self {
        Self {
            count,
            generation: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            calls: Arc::new(Mutex::new(Vec::new())),
            failure: None,
            read_only_hint: false,
            granted: true,
            live_granted: None,
        }
    }
}

impl ExternalToolHost for WideHost {
    fn name(&self) -> &str {
        "wide"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: self.generation.load(std::sync::atomic::Ordering::SeqCst),
            tools: (0..self.count)
                .map(|i| HostTool {
                    spec: qq_provider::ToolSpec::new(
                        format!("ext__wide__tool{i:02}"),
                        if i == 7 {
                            "Deploy the service to production".to_owned()
                        } else {
                            format!("Widget helper number {i}")
                        },
                        serde_json::json!({"type": "object"}),
                    ),
                    hints: ToolHints {
                        read_only: self.read_only_hint,
                        ..ToolHints::default()
                    },
                })
                .collect(),
            readiness: HostReadiness::Ready,
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        self.generation.load(std::sync::atomic::Ordering::SeqCst) == generation
    }

    fn config_grants(&self) -> Vec<String> {
        if !self.live_granted.as_ref().map_or(self.granted, |granted| {
            granted.load(std::sync::atomic::Ordering::SeqCst)
        }) {
            return Vec::new();
        }
        (0..self.count)
            .map(|i| format!("ext__wide__tool{i:02}"))
            .collect()
    }

    fn call(
        &self,
        name: String,
        _arguments: String,
        _cancelled: RunCancellation,
    ) -> HostCallFuture {
        self.calls.lock().unwrap().push(name.clone());
        let failure = self.failure.clone();
        Box::pin(async move {
            match failure {
                Some(error) => Err(error),
                None => Ok(HostToolResult {
                    content: format!("{name} ran"),
                    is_error: false,
                }),
            }
        })
    }

    fn readiness(&self) -> HostReadiness {
        HostReadiness::Ready
    }

    fn shutdown(&self) -> HostShutdownFuture {
        Box::pin(std::future::ready(()))
    }
}

struct TurnScript {
    turns: Vec<Vec<(&'static str, String)>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for TurnScript {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        let Some(calls) = self.turns.get(turn).filter(|calls| !calls.is_empty()) else {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        };
        let mut events = Vec::new();
        for (index, (name, arguments)) in calls.iter().enumerate() {
            let id = format!("call_{turn}_{index}");
            events.push(Ok(ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: (*name).to_owned(),
            }));
            events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: arguments.clone(),
            }));
            events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request
        .tools()
        .iter()
        .map(qq_provider::ToolSpec::name)
        .collect()
}
#[tokio::test]
async fn server_cooldown_survives_an_exhausted_http_retry_budget() {
    let server = qq_provider::test_support::LoopbackServer::respond_with_headers(
        429,
        "application/json",
        &[("Retry-After", "60")],
        r#"{"error":{"message":"wait for capacity"}}"#,
    );
    let provider = qq_provider::ProviderCompiler::new()
        .unwrap()
        .with_attempt_policy(qq_provider::AttemptPolicy::disabled())
        .compile(qq_provider::ProviderRecipe::http(
            qq_provider::HttpProviderRecipe::new(
                qq_provider::EndpointSpec::base(&server.base_url, true),
                qq_provider::HttpProtocol::OpenAiResponses,
                qq_provider::HttpAuth::NoAuth,
            ),
        ))
        .unwrap();
    let runtime = Runtime::with_provider(provider, "test-model", 256).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut events = runtime.run_messages_in_workspace(
        vec![Message::user("finish the task")],
        directory.path().to_owned(),
    );
    let mut rejected_before_work = false;
    let delay = loop {
        let event = events
            .next()
            .await
            .expect("the runtime must report the retry");
        if let RuntimeEvent::AssistantTurnCompleted { model_work, .. } = &event {
            assert_eq!(
                *model_work,
                crate::runtime::ModelWorkDisposition::RejectedBeforeWork
            );
            rejected_before_work = true;
        }
        if let RuntimeEvent::TurnRetrying { delay, kind, .. } = event {
            assert_eq!(kind, RunFailureKind::ProviderRateLimited);
            break delay;
        }
    };
    assert!(
        rejected_before_work,
        "the rejected turn must commit its no-work disposition before retrying"
    );
    assert!(
        delay >= Duration::from_secs(60),
        "server requested 60 seconds; runtime scheduled {delay:?}"
    );
    assert!(delay <= Duration::from_secs(62));
    drop(events);
    assert!(
        server
            .capture()
            .request_line()
            .unwrap()
            .starts_with("POST ")
    );
}
#[tokio::test]
async fn summarizer_errors_keep_server_cooldown_before_and_after_partial_output() {
    struct ThrottledSummary(bool);
    impl Provider for ThrottledSummary {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let mut events = Vec::new();
            if self.0 {
                events.push(Ok(ProviderEvent::OutputTextDelta {
                    text: "partial summary".to_owned(),
                }));
            }
            events.push(Err(ProviderError::RetryAfter {
                status: 429,
                message: "capacity".to_owned(),
                retry_after: Duration::from_secs(60),
            }));
            Box::pin(stream::iter(events))
        }
    }
    for partial in [false, true] {
        let runtime = Runtime::new(ThrottledSummary(partial), "test-model", 256).unwrap();
        let mut summary = Box::pin(runtime.summarize(
            vec![Message::user("summarize")],
            Arc::from("system"),
            Arc::from([]),
            None,
            256,
            None,
        ));
        let error = loop {
            if let Err(error) = summary.next().await.unwrap() {
                break error;
            }
        };
        let (kind, retry_after) = match error {
            runtime::SummarizeError::RejectedBeforeWork {
                kind, retry_after, ..
            } if !partial => (kind, retry_after),
            runtime::SummarizeError::Transient {
                kind, retry_after, ..
            } if partial => (kind, retry_after),
            other => panic!("wrong summary fault: {other:?}"),
        };
        assert_eq!(kind, RunFailureKind::ProviderRateLimited);
        assert_eq!(retry_after, Some(Duration::from_secs(60)));
    }
}
