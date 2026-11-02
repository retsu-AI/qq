use std::{
    path::Path,
    sync::{Arc, Mutex},
};

mod budgets;
mod cancellation;
mod delivery_history;
mod foreground_goal;
mod output;
mod resume;

use futures_util::stream;
use qq_core::{
    LoadedRuntime, Runtime, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadProgress,
    RuntimeLoadRequest, RuntimeLoader, SessionRuntimeOptions,
};
use qq_protocol::{AccountingTotal, ModelPricing, RunStatus, SessionStatus, WorkspaceSnapshot};
use qq_provider::{Message, ModelRequest, Provider, ProviderEvent, ProviderStream, ProviderUsage};

use super::*;

fn loaded_runtime(
    runtime: Runtime,
    workspace: &str,
    pricing: Option<ModelPricing>,
) -> LoadedRuntime {
    LoadedRuntime::compile_blocking(
        &runtime,
        qq_protocol::ResolvedModel {
            version: qq_protocol::ResolvedModelVersion::new(1).unwrap(),
            request_shape: None,
            route: "test/model".to_owned(),
            provider_model: "test-model".to_owned(),
            organization: None,
            credential_profile: None,
            max_output_tokens: 256,
            context_window: None,
            pricing,
            output_token_control: qq_protocol::CapabilitySupport::Native,
            generation: qq_protocol::GenerationCapabilities {
                reasoning_effort: qq_protocol::CapabilitySupport::Unsupported,
            },
            prompt_cache: qq_protocol::PromptCacheCapabilities {
                control: qq_protocol::CapabilitySupport::Unsupported,
                cache_read_usage: false,
                cache_write_usage: false,
            },
        },
        PathBuf::from(workspace),
    )
    .expect("test plan compiles")
}

struct ProviderLoader<F>(F);

impl<P, F> RuntimeLoader for ProviderLoader<F>
where
    P: Provider + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = (self.0)();
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime,
                        &request.workspace,
                        Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: None,
                            cache_write_usd_nanos_per_token: None,
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: qq_protocol::RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct HeldChildPreparation {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<RuntimeLoadProgress>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    exited: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

struct ParentChildLoader {
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    held_child: Option<Arc<HeldChildPreparation>>,
}

impl RuntimeLoader for ParentChildLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Self {
            parent: Arc::clone(&self.parent),
            child: Arc::clone(&self.child),
            held_child: None,
        }
        .load_with_progress(request, RuntimeLoadProgress::default())
    }

    fn load_with_progress(
        &self,
        request: RuntimeLoadRequest,
        progress: RuntimeLoadProgress,
    ) -> RuntimeLoadFuture {
        let is_child = request.model.model.as_deref() == Some("test/child");
        let held = is_child.then(|| self.held_child.clone()).flatten();
        let provider: Arc<dyn Provider> = if is_child {
            Arc::clone(&self.child)
        } else if request.model.model.as_deref() == Some("test/foreign") {
            Arc::new(HangingProvider)
        } else {
            Arc::clone(&self.parent)
        };
        Box::pin(async move {
            if let Some(held) = held {
                let release = held.release.lock().unwrap().take().unwrap();
                held.entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(progress)
                    .unwrap();
                release.await.unwrap();
                held.exited
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
            }
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(vec!["test/child".to_owned()]),
                        &request.workspace,
                        Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: None,
                            cache_write_usd_nanos_per_token: None,
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: qq_protocol::RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct TextProvider;

impl Provider for TextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(ProviderEvent::OutputTextDelta {
                text: "hel".to_owned(),
            }),
            Ok(ProviderEvent::OutputTextDelta {
                text: "lo".to_owned(),
            }),
            Ok(ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }),
            }),
        ]))
    }
}

struct UnmeteredTextProvider;

impl Provider for UnmeteredTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(ProviderEvent::OutputTextDelta {
                text: "done".to_owned(),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected output failure",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected output failure",
        ))
    }
}

struct BreaksAfterFlush {
    broken: bool,
}

impl Write for BreaksAfterFlush {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.broken {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected event output failure",
            ));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.broken = true;
        Ok(())
    }
}

struct MutatingProvider {
    turn: Mutex<usize>,
}

impl MutatingProvider {
    fn new() -> Self {
        Self {
            turn: Mutex::new(0),
        }
    }
}

impl Provider for MutatingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_write".to_owned(),
                    name: "write_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_write".to_owned(),
                    json: r#"{"path":"note.txt","content":"hello from qq\n"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "call_write".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_shell".to_owned(),
                    name: "shell".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_shell".to_owned(),
                    json: r#"{"command":"cp note.txt shelled.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "call_shell".to_owned(),
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

struct DangerousShellProvider {
    turn: Mutex<usize>,
    stopped: bool,
}

impl Provider for DangerousShellProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current < 2 {
            let id = format!("call_rm_{current}");
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "shell".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: format!(
                        r#"{{"command":"{}rm -r scratch{current}"}}"#,
                        if self.stopped { "sudo " } else { "" }
                    ),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
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

struct AskingProvider {
    turn: Mutex<usize>,
}

impl Provider for AskingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_ask".to_owned(),
                    name: "ask_user".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_ask".to_owned(),
                    json: r#"{"questions":[{"prompt":"Which crate?","options":["qq-core","qq-tui"]}]}"#
                        .to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "call_ask".to_owned(),
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

struct FetchingProvider {
    turn: Mutex<usize>,
    url: String,
}

struct FetchingLoader {
    url: String,
}

impl RuntimeLoader for FetchingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let url = self.url.clone();
        Box::pin(async move {
            let runtime = Runtime::new(
                FetchingProvider {
                    turn: Mutex::new(0),
                    url,
                },
                "test-model",
                256,
            )
            .unwrap()
            .with_network_policy(qq_core::NetworkPolicy {
                allow_private_for_tests: true,
                ..Default::default()
            });
            Ok(loaded_runtime(runtime, &request.workspace, None))
        })
    }
}

impl Provider for FetchingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call_fetch".to_owned(),
                    name: "fetch".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_fetch".to_owned(),
                    json: serde_json::json!({"url": self.url}).to_string(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "call_fetch".to_owned(),
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

struct SteerableProvider {
    turn: Mutex<usize>,
    release: Arc<tokio::sync::Notify>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for SteerableProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            let release = Arc::clone(&self.release);
            Box::pin(
                stream::once(async move {
                    release.notified().await;
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_read".to_owned(),
                        name: "read_file".to_owned(),
                    })
                })
                .chain(stream::iter([
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_read".to_owned(),
                        json: r#"{"path":"note.txt"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_read".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ])),
            )
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

struct HangingProvider;

impl Provider for HangingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::pending())
    }
}

struct ReadLoopProvider {
    turn: Mutex<usize>,
    report_usage: bool,
}

impl ReadLoopProvider {
    fn new() -> Self {
        Self {
            turn: Mutex::new(0),
            report_usage: true,
        }
    }

    fn unmetered() -> Self {
        Self {
            turn: Mutex::new(0),
            report_usage: false,
        }
    }
}

impl Provider for ReadLoopProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        Box::pin(stream::iter([
            Ok(ProviderEvent::ToolCallStarted {
                id: format!("call_{current}"),
                name: "read_file".to_owned(),
            }),
            Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: format!("call_{current}"),
                json: r#"{"path":"note.txt"}"#.to_owned(),
            }),
            Ok(ProviderEvent::ToolCallCompleted {
                id: format!("call_{current}"),
            }),
            Ok(ProviderEvent::Completed {
                usage: self.report_usage.then_some(ProviderUsage {
                    input_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 1,
                    reasoning_tokens: None,
                }),
            }),
        ]))
    }
}

struct SpawnsLoopingChildProvider {
    turn: Mutex<usize>,
}

impl SpawnsLoopingChildProvider {
    fn new() -> Self {
        Self {
            turn: Mutex::new(0),
        }
    }
}

impl Provider for SpawnsLoopingChildProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "spawn_child".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "spawn_child".to_owned(),
                    json: r#"{"task":"keep reading","model":"test/child"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "spawn_child".to_owned(),
                }),
                Ok(ProviderEvent::Completed {
                    usage: Some(ProviderUsage {
                        input_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 1,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed {
                    usage: Some(ProviderUsage {
                        input_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 1,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        }
    }
}

struct CompletesAfterInternalSlice {
    state: Mutex<(usize, bool)>,
}

impl CompletesAfterInternalSlice {
    fn new() -> Self {
        Self {
            state: Mutex::new((0, false)),
        }
    }
}

impl Provider for CompletesAfterInternalSlice {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut state = self.state.lock().unwrap();
        let checkpoint = request.messages().last().is_some_and(|message| {
            message.content().iter().any(|block| {
                matches!(block, qq_provider::ContentBlock::Text { text }
                    if text.contains("safe tool-call boundary"))
            })
        });
        if checkpoint {
            assert!(!request.tools().is_empty());
            state.1 = true;
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "slice checkpoint".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let stall_report = request.messages().last().is_some_and(|message| {
            message.content().iter().any(|block| {
                matches!(block, qq_provider::ContentBlock::Text { text }
                    if text.contains("tool calls changed nothing"))
            })
        });
        if stall_report {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "stall report".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        if state.1 {
            return Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "task complete".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]));
        }
        let turn = state.0;
        state.0 += 1;
        drop(state);

        let mut events = Vec::with_capacity(49);
        for index in 0..16 {
            let id = format!("call_{turn}_{index}");
            events.push(Ok(ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: "read_file".to_owned(),
            }));
            events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: r#"{"path":"note.txt"}"#.to_owned(),
            }));
            events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

struct ReadsThenHangsProvider {
    turn: Mutex<usize>,
}

impl ReadsThenHangsProvider {
    fn new() -> Self {
        Self {
            turn: Mutex::new(0),
        }
    }
}

impl Provider for ReadsThenHangsProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current >= 2 {
            return Box::pin(stream::pending());
        }
        Box::pin(stream::iter([
            Ok(ProviderEvent::ToolCallStarted {
                id: format!("call_{current}"),
                name: "read_file".to_owned(),
            }),
            Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: format!("call_{current}"),
                json: r#"{"path":"note.txt"}"#.to_owned(),
            }),
            Ok(ProviderEvent::ToolCallCompleted {
                id: format!("call_{current}"),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct Fixture {
    sessions: SessionRuntime,
    workspace: PathBuf,
    _directory: tempfile::TempDir,
}

async fn fixture<P, F>(provider: F) -> Fixture
where
    P: Provider + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    fixture_with_loader(Arc::new(ProviderLoader(provider))).await
}

async fn fixture_with_loader(loader: Arc<dyn RuntimeLoader>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let workspace = root.join("work");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let sessions = SessionRuntime::open(
        SessionRuntimeOptions::new(root.join("sessions.sqlite3")),
        loader,
    )
    .await
    .unwrap();
    Fixture {
        sessions,
        workspace,
        _directory: directory,
    }
}

async fn retire_delivery_history(fixture: &Fixture) {
    fixture
        .sessions
        .expire_delivery_history_for_test()
        .await
        .unwrap();
}

fn options(workspace: &Path) -> HeadlessOptions {
    HeadlessOptions {
        prompt: "do the task".to_owned(),
        workspace: workspace.to_owned(),
        session: None,
        model: ModelSelection {
            model_is_fallback: false,
            model: Some("test/model".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        },
        profile: qq_protocol::AgentProfileId::default(),
        context_window: Some(128_000),
        pricing_provenance: Some("test fixture".to_owned()),
        approval: HeadlessApproval::ReadOnly,
        approval_explicit: false,
        timeout: None,
        max_turns: None,
        max_cost_usd_nanos: None,
        correlation: qq_protocol::Correlation::default(),
        output: None,
        format: HeadlessFormat::Jsonl,
        trace: None,
        resume_hint: false,
        arm: None,
    }
}

fn goal_request() -> GoalInvocation {
    GoalInvocation {
        objective: Some("finish the task".to_owned()),
        check: None,
        check_approval: None,
        deadline: None,
        max_runs: Some(1),
        replace: false,
        defaults: qq_config::GoalsConfig::default(),
    }
}

async fn run_to_end(
    fixture: &Fixture,
    options: HeadlessOptions,
    interrupt: impl Future<Output = ()>,
) -> (HeadlessStatus, String, String) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = tokio::time::timeout(
        Duration::from_secs(30),
        run(
            &fixture.sessions,
            options,
            interrupt,
            None,
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .expect("a headless run must reach a terminal status");
    (
        status,
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

fn parse_records(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| {
            let record: qq_protocol::HeadlessRecord = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("not a headless record: {error}: {line}"));
            assert_eq!(serde_json::to_string(&record).unwrap(), line);
            serde_json::from_str(line).expect("every stdout line must be JSON")
        })
        .collect()
}

fn event_records(records: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    records
        .iter()
        .filter(|record| record["type"] == "event")
        .collect()
}

fn finished_tool_calls(records: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    event_records(records)
        .into_iter()
        .filter(|record| record["envelope"]["event"]["type"] == "tool_call_finished")
        .map(|record| &record["envelope"]["event"]["tool_call"])
        .collect()
}

async fn workspace_snapshot(fixture: &Fixture) -> WorkspaceSnapshot {
    let resolved = send(
        &fixture.sessions,
        SessionCommand::ResolveWorkspace {
            path: fixture.workspace.display().to_string(),
        },
    )
    .await
    .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        panic!("unexpected receipt: {:?}", resolved.outcome);
    };
    fixture
        .sessions
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            include_sessions: Vec::new(),
            session_limit: 16,
            message_limit: 16,
        })
        .await
        .unwrap()
}

async fn assert_no_active_run(fixture: &Fixture) {
    let snapshot = workspace_snapshot(fixture).await;
    for session in &snapshot.sessions {
        assert_eq!(session.status, SessionStatus::Idle, "session must be idle");
        assert_eq!(session.active_run_id, None, "no run may remain active");
    }
    for session in &snapshot.sessions {
        let focused = fixture
            .sessions
            .snapshot(SnapshotRequest {
                workspace_id: snapshot.workspace.id,
                focused_session_id: Some(session.id),
                include_sessions: Vec::new(),
                session_limit: 1,
                message_limit: 1,
            })
            .await
            .unwrap()
            .focused
            .expect("the session must still exist");
        for run in &focused.runs {
            assert!(
                matches!(
                    run.status,
                    RunStatus::Completed
                        | RunStatus::Cancelled
                        | RunStatus::Failed
                        | RunStatus::BudgetExhausted
                ),
                "run {run:?} must be terminal"
            );
        }
    }
}

#[tokio::test]
async fn the_selected_profile_reaches_the_loader_and_the_trial_record() {
    struct RecordingLoader {
        profiles: Arc<Mutex<Vec<String>>>,
    }
    impl RuntimeLoader for RecordingLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.profiles
                .lock()
                .unwrap()
                .push(request.profile.as_str().to_owned());
            Box::pin(async move {
                Runtime::new(TextProvider, "test-model", 256)
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                    .map_err(|error| RuntimeLoadError {
                        kind: qq_protocol::RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }
    let profiles = Arc::new(Mutex::new(Vec::new()));
    let fixture = fixture_with_loader(Arc::new(RecordingLoader {
        profiles: Arc::clone(&profiles),
    }))
    .await;
    let mut options = options(&fixture.workspace);
    options.profile = qq_protocol::AgentProfileId::new("reviewer").unwrap();

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert_eq!(*profiles.lock().unwrap(), ["reviewer"]);
    let records = parse_records(&stdout);
    assert_eq!(records[0]["profile"], "reviewer");
}

#[tokio::test]
async fn ask_mode_denies_mutations_immediately_without_a_client() {
    let fixture = fixture(MutatingProvider::new).await;
    let mut options = options(&fixture.workspace);
    options.approval = HeadlessApproval::Ask;
    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    assert!(!fixture.workspace.join("note.txt").exists());
    assert!(!fixture.workspace.join("shelled.txt").exists());
    let records = parse_records(&stdout);
    let calls: Vec<_> = event_records(&records)
        .into_iter()
        .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
        .map(|record| &record["envelope"]["event"]["tool_call"])
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call["state"] == "denied"));
    assert!(
        event_records(&records)
            .iter()
            .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_requested")
            .all(|record| record["envelope"]["event"]["delegated"] != true)
    );
}

#[tokio::test]
async fn auto_mode_executes_write_and_shell_calls_through_the_session_runtime() {
    let fixture = fixture(MutatingProvider::new).await;
    let mut options = options(&fixture.workspace);
    options.approval = HeadlessApproval::Auto;

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("note.txt")).unwrap(),
        "hello from qq\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("shelled.txt")).unwrap(),
        "hello from qq\n"
    );

    let records = parse_records(&stdout);
    let calls = finished_tool_calls(&records);
    let call_state = |name: &str| {
        calls
            .iter()
            .find(|call| call["name"] == name)
            .unwrap_or_else(|| panic!("expected a finished {name} call"))["state"]
            .clone()
    };
    assert_eq!(call_state("write_file"), "completed");
    assert_eq!(call_state("shell"), "completed");
    assert!(
        event_records(&records)
            .iter()
            .all(|record| { record["envelope"]["event"]["type"] != "tool_approval_requested" })
    );
}

#[tokio::test]
async fn auto_runs_shell_directly_but_fixed_stops_deny() {
    let fixture = fixture(|| DangerousShellProvider {
        turn: Mutex::new(0),
        stopped: false,
    })
    .await;
    for name in ["scratch0", "scratch1"] {
        std::fs::create_dir_all(fixture.workspace.join(name)).unwrap();
    }
    let mut options = options(&fixture.workspace);
    options.approval = HeadlessApproval::Auto;

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    assert!(!fixture.workspace.join("scratch0").exists());
    assert!(!fixture.workspace.join("scratch1").exists());
    let records = parse_records(&stdout);
    let approvals: Vec<_> = event_records(&records)
        .into_iter()
        .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_requested")
        .collect();
    assert!(approvals.is_empty(), "Auto must not open an approval hold");
    let resolved = event_records(&records)
        .into_iter()
        .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
        .count();
    assert_eq!(resolved, 0);
    assert!(
        finished_tool_calls(&records)
            .iter()
            .all(|call| call["state"] == "completed")
    );

    let unmatched = self::fixture(|| DangerousShellProvider {
        turn: Mutex::new(0),
        stopped: true,
    })
    .await;
    let mut strict = self::options(&unmatched.workspace);
    strict.approval = HeadlessApproval::Auto;
    let (status, stdout, _) = run_to_end(&unmatched, strict, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed);
    assert!(
        finished_tool_calls(&parse_records(&stdout))
            .iter()
            .all(|call| call["state"] == "denied")
    );
}

#[tokio::test]
async fn stdin_steering_lines_reach_the_next_model_turn() {
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let fixture = {
        let release = Arc::clone(&release);
        let requests = Arc::clone(&requests);
        fixture(move || SteerableProvider {
            turn: Mutex::new(0),
            release: Arc::clone(&release),
            requests: Arc::clone(&requests),
        })
        .await
    };
    std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
    let options = options(&fixture.workspace);
    let (tx, rx) = tokio::sync::mpsc::channel(MAX_PENDING_STEER_LINES);
    let sessions = fixture.sessions.clone();
    let workspace = fixture.workspace.display().to_string();
    let session_requests = Arc::clone(&requests);
    let driver = tokio::spawn(async move {
        while session_requests.lock().unwrap().len() != 1 {
            tokio::task::yield_now().await;
        }
        tx.send("   ".to_owned()).await.unwrap();
        tx.send("also check the tests".to_owned()).await.unwrap();
        let resolved = send(
            &sessions,
            SessionCommand::ResolveWorkspace { path: workspace },
        )
        .await
        .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
            panic!("unexpected receipt")
        };
        loop {
            tokio::task::yield_now().await;
            let sessions_snapshot = sessions
                .snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: None,
                    include_sessions: Vec::new(),
                    session_limit: 1,
                    message_limit: 8,
                })
                .await
                .unwrap();
            let Some(session_id) = sessions_snapshot.sessions.first().map(|s| s.id) else {
                continue;
            };
            let snapshot = sessions
                .snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: Some(session_id),
                    include_sessions: Vec::new(),
                    session_limit: 1,
                    message_limit: 8,
                })
                .await
                .unwrap();
            let queued = snapshot
                .focused
                .as_ref()
                .is_some_and(|body| body.messages.iter().any(|message| message.steering));
            if queued {
                break;
            }
        }
        drop(tx);
        release.notify_one();
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = tokio::time::timeout(
        Duration::from_secs(30),
        run(
            &fixture.sessions,
            options,
            std::future::pending(),
            Some(rx),
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .expect("terminal status");
    driver.await.unwrap();
    assert_eq!(status, HeadlessStatus::Completed);
    let stdout = String::from_utf8(stdout).unwrap();
    let records = parse_records(&stdout);
    let steering_events: Vec<&str> = event_records(&records)
        .into_iter()
        .filter_map(|record| record["envelope"]["event"]["type"].as_str())
        .filter(|kind| kind.starts_with("steering_"))
        .collect();
    assert_eq!(steering_events, ["steering_queued", "steering_applied"]);
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(
        captured[1].messages().iter().any(|message| {
            message.content().iter().any(|block| {
                matches!(block, qq_provider::ContentBlock::Text { text } if text.contains("also check the tests"))
            })
        }),
        "turn two carries the steering text"
    );
}

#[tokio::test]
async fn auto_fetch_dispatches_public_hosts_without_a_hold() {
    {
        let server = qq_provider::test_support::LoopbackServer::respond(
            200,
            "text/plain",
            "fixture-public-docs",
        );
        let fixture = fixture_with_loader(Arc::new(FetchingLoader {
            url: format!("{}/docs", server.base_url),
        }))
        .await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        let records = parse_records(&stdout);
        assert!(event_records(&records).iter().all(|record| {
            !record["envelope"]["event"]["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("tool_approval_"))
        }));
        let finished = finished_tool_calls(&records);
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0]["state"], "completed");
        assert!(
            finished[0]["result"]
                .as_str()
                .is_some_and(|result| result.contains("fixture-public-docs")),
            "{stdout}"
        );
        assert!(
            event_records(&records)
                .iter()
                .any(
                    |record| record["envelope"]["event"]["type"] == "tool_call_started"
                        && record["envelope"]["event"]["tool_call"]["name"] == "fetch"
                )
        );
        let request = tokio::task::spawn_blocking(move || server.capture())
            .await
            .unwrap();
        assert_eq!(request.request_line(), Some("GET /docs HTTP/1.1"));
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_question_with_no_client_ends_the_run_as_needs_input() {
    let fixture = fixture(|| AskingProvider {
        turn: Mutex::new(0),
    })
    .await;
    let mut options = options(&fixture.workspace);
    options.approval = HeadlessApproval::Full;
    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::NeedsInput);
    let records = parse_records(&stdout);
    let outcome = records.last().unwrap();
    assert_eq!(outcome["type"], "outcome");
    assert_eq!(outcome["status"], "needs_input");
    assert_eq!(outcome["exit_code"], 5);
    assert_eq!(
        outcome["message"],
        "the model asked the user a question and no client could answer: Which crate?"
    );
    let question = event_records(&records)
        .into_iter()
        .find(|record| record["envelope"]["event"]["type"] == "tool_approval_requested")
        .expect("the question is on the stream");
    assert_eq!(
        question["envelope"]["event"]["question"]["questions"][0]["prompt"],
        "Which crate?"
    );
    let finished = finished_tool_calls(&records);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["state"], "interrupted");
    assert!(
        stderr.contains("no client could answer: Which crate?"),
        "{stderr}"
    );
}

#[tokio::test]
async fn read_only_mode_denies_mutations_without_stalling() {
    let fixture = fixture(MutatingProvider::new).await;
    let options = options(&fixture.workspace);

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert!(!fixture.workspace.join("note.txt").exists());
    assert!(!fixture.workspace.join("shelled.txt").exists());
    let records = parse_records(&stdout);
    assert!(
        event_records(&records)
            .iter()
            .all(|record| { record["envelope"]["event"]["type"] != "tool_approval_requested" })
    );
    assert!(
        finished_tool_calls(&records)
            .iter()
            .all(|call| call["state"] == "denied")
    );
}

#[tokio::test]
async fn correlation_is_stamped_on_the_trial_record_and_every_session_snapshot() {
    let fixture = fixture(|| TextProvider).await;
    let correlation = qq_protocol::Correlation::new(
        [("job", "j-1"), ("attempt", "2")]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    )
    .unwrap();
    let options = HeadlessOptions {
        correlation: correlation.clone(),
        ..options(&fixture.workspace)
    };

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    assert_eq!(records[0]["type"], "trial");
    assert_eq!(records[0]["correlation"]["job"], "j-1");
    assert_eq!(records[0]["correlation"]["attempt"], "2");
    let snapshots: Vec<&serde_json::Value> = event_records(&records)
        .iter()
        .filter_map(|record| {
            let session = &record["envelope"]["event"]["session"];
            session.is_object().then_some(session)
        })
        .collect();
    assert!(
        !snapshots.is_empty(),
        "lifecycle events must carry a session snapshot"
    );
    for session in snapshots {
        assert_eq!(session["correlation"]["job"], "j-1");
        assert_eq!(session["correlation"]["attempt"], "2");
    }
    let workspace = workspace_snapshot(&fixture).await;
    assert!(
        workspace
            .sessions
            .iter()
            .all(|session| session.correlation == correlation)
    );
}

struct RecordingTextProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Provider for RecordingTextProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([
            Ok(ProviderEvent::OutputTextDelta {
                text: "ok".to_owned(),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]))
    }
}

#[tokio::test]
async fn at_mentions_in_the_prompt_attach_workspace_files_client_side() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let fixture = {
        let requests = Arc::clone(&requests);
        fixture(move || RecordingTextProvider {
            requests: Arc::clone(&requests),
        })
        .await
    };
    std::fs::write(fixture.workspace.join("notes.md"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(fixture.workspace.join("other.rs"), "fn x() {}\n").unwrap();
    let options = HeadlessOptions {
        prompt: "compare @notes.md:2-3 with @other.rs; ignore me@example.com and @missing.txt"
            .to_owned(),
        ..options(&fixture.workspace)
    };

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    assert!(stderr.contains("note: @missing.txt"), "{stderr}");
    let requests = requests.lock().unwrap();
    let prompt = requests[0]
        .messages()
        .iter()
        .flat_map(|message| message.content())
        .find_map(|block| match block {
            qq_provider::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        prompt.starts_with(
            "compare @notes.md:2-3 with @other.rs; ignore me@example.com and @missing.txt"
        ),
        "{prompt}"
    );
    assert!(
        prompt
            .contains("<attached-file path=\"notes.md\" lines=\"2-3/3\">\n````\nbeta\ngamma\n````"),
        "{prompt}"
    );
    assert!(
        prompt.contains("<attached-file path=\"other.rs\">"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("alpha"),
        "the range excludes line 1: {prompt}"
    );
    let records = parse_records(&stdout);
    let queued = event_records(&records)
        .into_iter()
        .find(|record| record["envelope"]["event"]["type"] == "prompt_queued")
        .unwrap();
    let output = queued["envelope"]["event"]["message"]["output"]
        .as_str()
        .unwrap();
    assert!(output.contains("@notes.md"), "{output}");
}

struct ScriptedTextProvider {
    answers: Mutex<std::collections::VecDeque<&'static str>>,
}

impl ScriptedTextProvider {
    fn new(answers: &[&'static str]) -> Self {
        Self {
            answers: Mutex::new(answers.iter().copied().collect()),
        }
    }
}

impl Provider for ScriptedTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let text = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or("out of script");
        Box::pin(stream::iter([
            Ok(ProviderEvent::OutputTextDelta {
                text: text.to_owned(),
            }),
            Ok(ProviderEvent::Completed { usage: None }),
        ]))
    }
}

const DENIAL_HINT: &str = "held calls were denied under --approval read-only; rerun with --approval auto to allow workspace edits";
