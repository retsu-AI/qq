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
    ApprovalDecision, ApprovalMode, CommandId, CommandOutcome, ModelSelection, RunFailureKind,
    RunId, RunOutcome, SessionCommand, SessionEvent, SubscribeRequest, ToolCallState,
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
            LoadedRuntime::from_runtime(
                runtime,
                qq_protocol::AgentProfileId::default(),
                PathBuf::from(request.workspace),
            )
            .await
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

#[tokio::test]
async fn ask_mode_requires_fresh_human_approval_for_each_mcp_call() {
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
            decision: ApprovalDecision::ApproveOnce,
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

    let second_run = submit_prompt(&runtime, session_id).await;
    let requested = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let envelope = events.next().await.unwrap().unwrap();
            if let SessionEvent::ToolApprovalRequested { tool_call, .. } = envelope.event {
                break tool_call;
            }
            assert!(!matches!(envelope.event, SessionEvent::RunFinished { .. }));
        }
    })
    .await
    .expect("the next MCP call still needs the human");
    assert_eq!(requested.name, MCP_TOOL);
    assert_eq!(calls.lock().unwrap().len(), 1);
    command(
        &runtime,
        SessionCommand::RespondToolApproval {
            run_id: second_run,
            tool_call_id: requested.id,
            decision: ApprovalDecision::Deny,
        },
    )
    .await;
    let observed = collect_run(&mut events, second_run).await;
    assert!(observed.iter().any(|event| matches!(
        event,
        SessionEvent::ToolApprovalResolved {
            resolution: qq_protocol::ApprovalResolution::Denied,
            ..
        }
    )));
    assert_eq!(calls.lock().unwrap().len(), 1);
}
