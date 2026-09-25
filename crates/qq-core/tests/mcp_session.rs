//! End-to-end MCP dispatch through the durable session runtime: a scripted
//! model requests an `mcp__` tool, the session's approval flow gates it in
//! `ask` mode, and an exact-name session grant auto-approves the next call.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use qq_core::{
    ExternalToolHost, HostCallFuture, HostCatalog, HostReadiness, HostShutdownFuture, HostTool,
    HostToolResult, LoadedRuntime, RunCancellation, Runtime, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeOptions,
    ToolHints,
};
use qq_protocol::{
    ApprovalDecision, ApprovalGrant, ApprovalMode, CapabilitySupport, CommandId, CommandOutcome,
    GenerationCapabilities, ModelSelection, PromptCacheCapabilities, ResolvedModel,
    ResolvedModelVersion, RunFailureKind, RunId, RunOutcome, SessionCommand, SessionEvent,
    SubscribeRequest, ToolCallSnapshot, ToolCallState,
};
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream, ToolSpec};

const MCP_TOOL: &str = "mcp__srv__ping";

struct PingRegistry {
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl ExternalToolHost for PingRegistry {
    fn name(&self) -> &str {
        "mcp"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: 1,
            tools: vec![HostTool {
                spec: ToolSpec::new(
                    MCP_TOOL,
                    "Ping the fixture MCP server.",
                    serde_json::json!({"type": "object"}),
                ),
                hints: ToolHints::default(),
            }],
            readiness: HostReadiness::Ready,
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        generation == 1
    }

    fn config_grants(&self) -> Vec<String> {
        Vec::new()
    }

    fn call(&self, name: String, arguments: String, _cancelled: RunCancellation) -> HostCallFuture {
        self.calls.lock().unwrap().push((name, arguments));
        Box::pin(async {
            Ok(HostToolResult {
                content: "pong".to_owned(),
                is_error: false,
            })
        })
    }

    fn readiness(&self) -> HostReadiness {
        HostReadiness::Ready
    }

    fn shutdown(&self) -> HostShutdownFuture {
        Box::pin(std::future::ready(()))
    }
}

/// Each run's first model turn requests the MCP tool; the second completes.
struct McpTurnProvider {
    turn: Mutex<usize>,
}

impl Provider for McpTurnProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: MCP_TOOL.to_owned(),
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
            Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}

struct McpLoader {
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl RuntimeLoader for McpLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let registry = Arc::new(PingRegistry {
            calls: Arc::clone(&self.calls),
        });
        Box::pin(async move {
            let runtime = Runtime::new(
                McpTurnProvider {
                    turn: Mutex::new(0),
                },
                "test-model",
                256,
            )
            .map(|runtime| runtime.with_tool_host(registry))
            .map_err(|error| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: error.to_string(),
            })?;
            LoadedRuntime::compile_blocking(
                &runtime,
                ResolvedModel {
                    version: ResolvedModelVersion::new(1).unwrap(),
                    request_shape: None,
                    route: "test/model".to_owned(),
                    provider_model: "test-model".to_owned(),
                    organization: None,
                    credential_profile: None,
                    max_output_tokens: 256,
                    context_window: None,
                    pricing: None,
                    output_token_control: CapabilitySupport::Native,
                    generation: GenerationCapabilities {
                        reasoning_effort: CapabilitySupport::Unsupported,
                    },
                    prompt_cache: PromptCacheCapabilities {
                        control: CapabilitySupport::Unsupported,
                        cache_read_usage: false,
                        cache_write_usage: false,
                    },
                },
                PathBuf::from(request.workspace),
            )
            .map_err(|error| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: error.to_string(),
            })
        })
    }
}

async fn command(runtime: &SessionRuntime, command: SessionCommand) -> CommandOutcome {
    runtime
        .command(CommandId::generate().unwrap(), command)
        .await
        .unwrap()
        .outcome
}

async fn submit_prompt(runtime: &SessionRuntime, session_id: qq_protocol::SessionId) -> RunId {
    let outcome = command(
        runtime,
        SessionCommand::SubmitPrompt {
            session_id,
            input: vec![qq_protocol::InputPart::text("ping the server".to_owned())],
            limits: qq_protocol::RunLimits::default(),
            correlation: qq_protocol::Correlation::default(),
            output: None,
        },
    )
    .await;
    let CommandOutcome::PromptQueued { run_id, .. } = outcome else {
        panic!("unexpected receipt: {outcome:?}");
    };
    run_id
}

/// Collects this run's events through `RunFinished`, returning them.
async fn collect_run(events: &mut SessionEventStream, run_id: RunId) -> Vec<SessionEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut observed = Vec::new();
        loop {
            let envelope = events.next().await.unwrap().unwrap();
            if envelope.run_id != Some(run_id) {
                continue;
            }
            let finished = matches!(envelope.event, SessionEvent::RunFinished { .. });
            observed.push(envelope.event);
            if finished {
                return observed;
            }
        }
    })
    .await
    .expect("the run must finish within five seconds")
}

fn approval_request(events: &[SessionEvent]) -> Option<&ToolCallSnapshot> {
    events.iter().find_map(|event| match event {
        SessionEvent::ToolApprovalRequested { tool_call, .. } => Some(tool_call),
        _ => None,
    })
}

#[tokio::test]
async fn ask_mode_gates_mcp_calls_and_an_exact_name_grant_auto_approves() {
    let directory = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(McpLoader {
            calls: Arc::clone(&calls),
        }),
    )
    .await
    .unwrap();

    let outcome = command(
        &runtime,
        SessionCommand::ResolveWorkspace {
            path: directory.path().to_str().unwrap().to_owned(),
        },
    )
    .await;
    let CommandOutcome::WorkspaceResolved { workspace_id } = outcome else {
        panic!("unexpected receipt: {outcome:?}");
    };
    let created = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::Ask,
                profile: qq_protocol::AgentProfileId::default(),
                reasoning_effort: None,
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt: {:?}", created.outcome);
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();

    // First run: ask mode must hold the MCP call for approval, and
    // approve-for-session must record the exact-name grant before executing.
    let first_run = submit_prompt(&runtime, session_id).await;
    let requested = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let envelope = events.next().await.unwrap().unwrap();
            if let SessionEvent::ToolApprovalRequested { tool_call, .. } = envelope.event {
                break tool_call;
            }
            assert!(
                !matches!(envelope.event, SessionEvent::RunFinished { .. }),
                "the run must wait for approval before finishing"
            );
        }
    })
    .await
    .expect("ask mode must request approval for the MCP call");
    assert_eq!(requested.name, MCP_TOOL);
    assert_eq!(requested.arguments, r#"{"value":1}"#);
    assert!(
        calls.lock().unwrap().is_empty(),
        "the call must not execute before approval"
    );

    let outcome = command(
        &runtime,
        SessionCommand::RespondToolApproval {
            run_id: first_run,
            tool_call_id: requested.id,
            decision: ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::Tool {
                    name: MCP_TOOL.to_owned(),
                },
            },
        },
    )
    .await;
    assert!(matches!(
        outcome,
        CommandOutcome::ToolApprovalResolved { .. }
    ));

    let observed = collect_run(&mut events, first_run).await;
    assert!(observed.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
                && tool_call.result.as_deref() == Some("pong")
                && !tool_call.is_error
    )));
    assert!(matches!(
        observed.last(),
        Some(SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        })
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);

    // Second run: the recorded exact-name grant auto-approves without a
    // prompt, and the MCP result is indistinguishable from a built-in's.
    let second_run = submit_prompt(&runtime, session_id).await;
    let observed = collect_run(&mut events, second_run).await;
    assert!(
        approval_request(&observed).is_none(),
        "the session grant must cover the second call"
    );
    assert!(observed.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallFinished { tool_call }
            if tool_call.state == ToolCallState::Completed
                && tool_call.result.as_deref() == Some("pong")
    )));
    assert!(matches!(
        observed.last(),
        Some(SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        })
    ));
    assert_eq!(calls.lock().unwrap().len(), 2);
}
