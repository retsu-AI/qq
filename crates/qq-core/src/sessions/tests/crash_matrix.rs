use super::probe::EffectStage;
use super::*;
use crate::{
    ExternalToolHost, HostCallFuture, HostCatalog, HostReadiness, HostShutdownFuture, HostTool,
    HostToolResult, ToolHints,
};

const MCP_TOOL: &str = "mcp__fake__append";
const SHELL_EFFECT: &str = r#"{"command":"printf x >> effects"}"#;
const NOT_STARTED: &str = "Tool execution did not start before the run was interrupted.";

#[derive(Clone, Copy, Debug)]
enum Effect {
    Shell,
    Mcp,
}

struct CountingHost {
    effects: Arc<AtomicUsize>,
}

impl ExternalToolHost for CountingHost {
    fn name(&self) -> &str {
        "mcp"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        HostCatalog {
            generation: 1,
            tools: vec![HostTool {
                spec: qq_provider::ToolSpec::new(
                    MCP_TOOL,
                    "Append one record on the fake server.",
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

    fn call(
        &self,
        _name: String,
        _arguments: String,
        _cancelled: RunCancellation,
    ) -> HostCallFuture {
        let count = self.effects.fetch_add(1, Ordering::AcqRel) + 1;
        Box::pin(async move {
            Ok(HostToolResult {
                content: format!("appended record {count}"),
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

struct EffectProvider {
    effect: Effect,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for EffectProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let acting = request_texts(&request)
            .last()
            .is_some_and(|text| text == "act");
        self.requests.lock().unwrap().push(request);
        if !acting {
            return text_reply("done");
        }
        let (name, arguments) = match self.effect {
            Effect::Shell => ("shell", SHELL_EFFECT),
            Effect::Mcp => (MCP_TOOL, "{}"),
        };
        provider_script(
            tool_call("call_effect", name, arguments)
                .into_iter()
                .chain([completed(None)]),
        )
    }
}

struct EffectLoader {
    effect: Effect,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    mcp_effects: Arc<AtomicUsize>,
}

impl RuntimeLoader for EffectLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = Arc::new(EffectProvider {
            effect: self.effect,
            requests: Arc::clone(&self.requests),
        });
        let runtime = Runtime::with_provider(provider, "test-model", 256)
            .unwrap()
            .with_tool_host(Arc::new(CountingHost {
                effects: Arc::clone(&self.mcp_effects),
            }));
        let loaded = loaded_runtime(runtime, &request.workspace, None);
        Box::pin(async move { Ok(loaded) })
    }
}

#[derive(Debug)]
struct Recovered {
    effects: usize,
    requests_before_follow_up: usize,
    recovery_events: Vec<String>,
    run: RunOutcome,
    tool: ToolCallState,
    execution: Option<ToolExecution>,
    result: String,
    follow_up_saw: String,
    effects_after_follow_up: usize,
    second_recovery_events: usize,
}

fn event_types(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT envelope_json FROM events ORDER BY sequence")?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|json| {
            let envelope: SessionEventEnvelope = serde_json::from_str(&json).unwrap();
            serde_json::to_value(&envelope.event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect())
}

fn stored_event_types(database: &std::path::Path) -> Vec<String> {
    event_types(&Connection::open(database).unwrap()).unwrap()
}

fn effect_count(effect: Effect, workspace: &std::path::Path, mcp: &AtomicUsize) -> usize {
    match effect {
        Effect::Shell => std::fs::read_to_string(workspace.join("effects")).map_or(0, |e| e.len()),
        Effect::Mcp => mcp.load(Ordering::Acquire),
    }
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn crash_at(stage: EffectStage, effect: Effect) -> Recovered {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mcp_effects = Arc::new(AtomicUsize::new(0));
    let loader = || {
        Arc::new(EffectLoader {
            effect,
            requests: Arc::clone(&requests),
            mcp_effects: Arc::clone(&mcp_effects),
        })
    };

    let process = current_thread();
    let (directory, workspace_id, session_id, held) = process.block_on(async {
        let SessionFixture {
            directory,
            runtime,
            workspace_id,
            session_id,
            ..
        } = SessionFixture::open(loader(), ApprovalMode::Full).await;
        let (entered, held) = PROBE.hold(Checkpoint::Effect(session_id, stage));
        submit(&runtime, session_id, "act").await;
        within(entered).await.unwrap().unwrap();
        runtime.abandon_for_test().await.unwrap();
        drop(runtime);
        (directory, workspace_id, session_id, held)
    });
    process.shutdown_background();
    drop(held);

    let database = directory.path().join("sessions.sqlite3");
    let workspace = directory.path();
    let effects = effect_count(effect, workspace, &mcp_effects);
    let crashed_events = stored_event_types(&database).len();

    let restarted = current_thread();
    let recovered = restarted.block_on(async {
        let runtime = SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader())
            .await
            .unwrap();
        let requests_before_follow_up = requests.lock().unwrap().len();
        let recovery_events = runtime
            .store()
            .call(Priority::Control, |connection| Ok(event_types(connection)?))
            .await
            .unwrap()[crashed_events..]
            .to_vec();
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 4, 4))
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        let call = focused.tool_calls[0].clone();
        let run = focused.runs[0].outcome.clone().unwrap();

        let mut events = subscribe_after(&runtime, workspace_id, snapshot.cursor);
        let follow_up = submit(&runtime, session_id, "continue").await;
        collect_until(&mut events, finished_for(follow_up)).await;
        let follow_up_saw = {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                requests_before_follow_up + 1,
                "recovery must not send a provider request of its own"
            );
            requests[requests_before_follow_up]
                .messages()
                .iter()
                .flat_map(Message::content)
                .find_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .unwrap()
        };
        runtime.close().await.unwrap();
        (
            requests_before_follow_up,
            recovery_events,
            run,
            call,
            follow_up_saw,
        )
    });
    drop(restarted);
    let (requests_before_follow_up, recovery_events, run, call, follow_up_saw) = recovered;
    let effects_after_follow_up = effect_count(effect, workspace, &mcp_effects);

    let settled_events = stored_event_types(&database).len();
    current_thread().block_on(async {
        SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader())
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    });
    let second_recovery_events = stored_event_types(&database).len() - settled_events;

    Recovered {
        effects,
        requests_before_follow_up,
        recovery_events,
        run,
        tool: call.state,
        execution: call.execution,
        result: call.result.unwrap(),
        follow_up_saw,
        effects_after_follow_up,
        second_recovery_events,
    }
}

struct Expected {
    stage: EffectStage,
    effects: usize,
    requests: usize,
    recovery_events: &'static [&'static str],
    run: RunOutcome,
    tool: ToolCallState,
    execution: Option<ToolExecution>,
    interrupted_result: Option<&'static str>,
}

#[test]
fn a_crash_at_each_effect_boundary_recovers_without_repeating_the_effect() {
    let cases = [
        Expected {
            stage: EffectStage::IntentCommitted,
            effects: 0,
            requests: 1,
            recovery_events: &["tool_call_finished", "run_finished"],
            run: RunOutcome::Interrupted,
            tool: ToolCallState::Interrupted,
            execution: Some(ToolExecution::NotStarted),
            interrupted_result: Some(NOT_STARTED),
        },
        Expected {
            stage: EffectStage::DispatchCommitted,
            effects: 0,
            requests: 1,
            recovery_events: &["tool_call_finished", "run_finished"],
            run: RunOutcome::Interrupted,
            tool: ToolCallState::Interrupted,
            execution: Some(ToolExecution::MayHaveRun),
            interrupted_result: Some(INTERRUPTED_TOOL_RESULT),
        },
        Expected {
            stage: EffectStage::ResultReceived,
            effects: 1,
            requests: 1,
            recovery_events: &["tool_call_finished", "run_finished"],
            run: RunOutcome::Interrupted,
            tool: ToolCallState::Interrupted,
            execution: Some(ToolExecution::MayHaveRun),
            interrupted_result: Some(INTERRUPTED_TOOL_RESULT),
        },
        Expected {
            stage: EffectStage::ResultCommitted,
            effects: 1,
            requests: 1,
            recovery_events: &["run_finished"],
            run: RunOutcome::Interrupted,
            tool: ToolCallState::Completed,
            execution: None,
            interrupted_result: None,
        },
        Expected {
            stage: EffectStage::RunSettled,
            effects: 1,
            requests: 2,
            recovery_events: &[],
            run: RunOutcome::Completed,
            tool: ToolCallState::Completed,
            execution: None,
            interrupted_result: None,
        },
    ];
    let effects = if cfg!(unix) {
        vec![Effect::Shell, Effect::Mcp]
    } else {
        vec![Effect::Mcp]
    };
    for effect in effects {
        for expected in &cases {
            let recovered = crash_at(expected.stage, effect);
            let context = format!("{effect:?} at {:?}: {recovered:#?}", expected.stage);
            assert_eq!(recovered.effects, expected.effects, "{context}");
            assert_eq!(
                recovered.effects_after_follow_up, expected.effects,
                "{context}"
            );
            assert_eq!(
                recovered.requests_before_follow_up, expected.requests,
                "{context}"
            );
            assert_eq!(
                recovered.recovery_events, expected.recovery_events,
                "{context}"
            );
            assert_eq!(recovered.second_recovery_events, 0, "{context}");
            assert_eq!(recovered.run, expected.run, "{context}");
            assert_eq!(recovered.tool, expected.tool, "{context}");
            assert_eq!(recovered.execution, expected.execution, "{context}");
            assert_eq!(recovered.follow_up_saw, recovered.result, "{context}");
            match (expected.interrupted_result, effect) {
                (Some(result), _) => assert_eq!(recovered.result, result, "{context}"),
                (None, Effect::Shell) => {
                    assert!(recovered.result.starts_with("shell exit=0 "), "{context}");
                }
                (None, Effect::Mcp) => {
                    assert_eq!(recovered.result, "appended record 1", "{context}");
                }
            }
        }
    }
}
