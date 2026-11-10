use crate::test_support::within;
use std::sync::atomic::AtomicBool;

use super::*;

mod accounting;
mod approvals;
mod budgets;
mod commands;
mod compaction;
mod context_capacity;
mod contract;
mod crash_matrix;
mod deadlines;
mod delegation;
mod execution_context;
mod feeds;
mod fixtures;
mod loop_guard;
mod migrations;
mod nonblocking;
mod progress;
mod projection;
mod replay_identity;
mod runs;
mod settlement;
mod side_questions;
mod streaming;
mod undo;

mod checkpoint_review;
mod checkpoints;
use std::sync::Mutex as StdMutex;

use async_stream::stream as async_stream;

use futures_util::{StreamExt, stream};

use qq_protocol::{BudgetExhaustion, BudgetLimitKind};

use qq_provider::{ModelRequest, Provider, ProviderStream};

use tempfile::TempDir;

use super::probe::{Checkpoint, FAULTS, PROBE, StoreFault};
use fixtures::*;

fn hold_outcome_read(
    run_id: RunId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (entered, release) = PROBE.hold(Checkpoint::OutcomeRead(run_id));
    (
        entered,
        release,
        PROBE.observe(Checkpoint::OutcomeReadPolled(run_id)),
    )
}

fn test_directory() -> TempDir {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

fn usage(input_tokens: u64, output_tokens: u64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_tokens: None,
    }
}

fn loaded_runtime(
    runtime: Runtime,
    workspace: &str,
    pricing: Option<ModelPricing>,
) -> LoadedRuntime {
    loaded_runtime_for_route(runtime, workspace, "test/model", pricing)
}

fn loaded_runtime_for_route(
    runtime: Runtime,
    workspace: &str,
    route: impl Into<String>,
    pricing: Option<ModelPricing>,
) -> LoadedRuntime {
    let provider_model = runtime.model.to_string();
    let max_output_tokens = runtime.max_output_tokens;
    let mut resolved = test_resolved_model(route, provider_model, max_output_tokens, pricing);
    resolved.context_window = runtime.context_window;
    loaded_runtime_with_model(runtime, workspace, resolved)
}

fn loaded_runtime_with_model(
    runtime: Runtime,
    workspace: &str,
    resolved: ResolvedModel,
) -> LoadedRuntime {
    let runtime = runtime.with_turn_recovery(crate::TurnRecoveryPolicy::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
    ));
    LoadedRuntime::compile_blocking(&runtime, resolved, PathBuf::from(workspace))
        .expect("test plan compiles")
}

fn test_resolved_model(
    route: impl Into<String>,
    provider_model: impl Into<String>,
    max_output_tokens: u32,
    pricing: Option<ModelPricing>,
) -> ResolvedModel {
    ResolvedModel {
        version: qq_protocol::ResolvedModelVersion::new(2).unwrap(),
        request_shape: Some(qq_protocol::ProviderRequestShapeIdentity {
            version: qq_protocol::ProviderRequestShapeVersion::new(1).unwrap(),
            digest: ContentHash::from_bytes([1; 32]),
        }),
        route: route.into(),
        provider_model: provider_model.into(),
        organization: None,
        credential_profile: None,
        max_output_tokens,
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
    }
}

fn test_static_prefix(system: u8, tools: Option<u8>) -> PreparedStaticPrefix {
    PreparedStaticPrefix::new(
        ContentHash::from_bytes([system; 32]),
        tools.map(|tools| ContentHash::from_bytes([tools; 32])),
    )
}

fn insert_accounting_session(
    connection: &Connection,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    parent_id: Option<SessionId>,
) {
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, parent_id, title, status,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, 'accounting test', 'idle', 1, 1)",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                parent_id.map(|id| id.to_string()),
            ],
        )
        .unwrap();
}

fn insert_accounting_run(
    connection: &Connection,
    session_id: SessionId,
    status: &str,
    usage: Option<TokenUsage>,
    cost: Option<u64>,
) -> RunId {
    let run_id = RunId::generate().unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id,
                 assistant_message_id, status, outcome_json, usage_json,
                 estimated_cost_usd_nanos, created_at_ms, started_at_ms, finished_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, 1, 1, 2)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::generate().unwrap().to_string(),
                MessageId::generate().unwrap().to_string(),
                MessageId::generate().unwrap().to_string(),
                status,
                usage.map(|usage| serde_json::to_string(&usage).unwrap()),
                cost,
            ],
        )
        .unwrap();
    run_id
}

struct ScriptedLoader;

impl RuntimeLoader for ScriptedLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let loaded = ProviderLoader::new(ScriptedProvider)
            .pricing(test_pricing())
            .loaded(&request.workspace);
        Box::pin(async move { Ok(loaded) })
    }
}

struct ScriptedProvider;

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let summarizing = request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"));
        if summarizing {
            return provider_script([
                text_delta(valid_summary("hello")),
                completed(Some(qq_provider::ProviderUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 5,
                    reasoning_tokens: None,
                })),
            ]);
        }
        provider_script([
            text_delta("hel"),
            text_delta("l"),
            text_delta("o"),
            completed(Some(qq_provider::ProviderUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
                reasoning_tokens: None,
            })),
        ])
    }
}

struct CountingTextLoader {
    provider_calls: Arc<AtomicUsize>,
}

impl RuntimeLoader for CountingTextLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider_calls = Arc::clone(&self.provider_calls);
        Box::pin(async move {
            struct CountingTextProvider(Arc<AtomicUsize>);

            impl Provider for CountingTextProvider {
                fn stream(&self, _request: ModelRequest) -> ProviderStream {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    text_reply("done")
                }
            }

            Ok(
                ProviderLoader::new(CountingTextProvider(provider_calls))
                    .loaded(&request.workspace),
            )
        })
    }
}

struct PricedHangingLoader;

impl RuntimeLoader for PricedHangingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let loaded = ProviderLoader::new(HangingProvider)
            .pricing(test_pricing())
            .loaded(&request.workspace);
        Box::pin(async move { Ok(loaded) })
    }
}

struct CapturingLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for CapturingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        load_provider(DelayedProvider { requests }, &request)
    }
}

struct DelayedProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

struct RenewableSliceProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    checkpoint_wait: Option<Arc<tokio::sync::Notify>>,
    metered_empty_checkpoint: bool,
    slice_calls: usize,
}

impl Provider for RenewableSliceProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let current = requests.len();
        requests.push(request.clone());
        drop(requests);

        let tool_turns = self.slice_calls / crate::MAX_TOOL_CALLS_PER_TURN;
        let checkpoint_request =
            request.messages().last() == Some(&Message::user(crate::SLICE_CHECKPOINT_NOTICE));
        if checkpoint_request {
            assert!(
                !request.tools().is_empty(),
                "tools stay declared on the checkpoint turn"
            );
            if let Some(checkpoint_wait) = &self.checkpoint_wait {
                checkpoint_wait.notify_one();
                return Box::pin(stream::pending());
            }
            if self.metered_empty_checkpoint {
                return provider_script([completed(Some(qq_provider::ProviderUsage {
                    input_tokens: 3,
                    cache_read_input_tokens: 1,
                    cache_write_input_tokens: 2,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }))]);
            }
            return text_reply("slice checkpoint");
        }
        if current > tool_turns {
            return provider_script([
                text_delta("task complete"),
                completed(
                    self.metered_empty_checkpoint
                        .then_some(provider_usage(7, 3)),
                ),
            ]);
        }

        let mut events = Vec::with_capacity(crate::MAX_TOOL_CALLS_PER_TURN * 3 + 1);
        for index in 0..crate::MAX_TOOL_CALLS_PER_TURN {
            let id = format!("call-{current}-{index}");
            let (name, json) = if current == 0 && index == 0 {
                ("read_file", r#"{"path":"slice-effects.txt"}"#)
            } else if current == 0 && index == 1 {
                (
                    "edit_file",
                    r#"{"edits":[{"path":"slice-effects.txt","old":"seed","new":"seedx"}]}"#,
                )
            } else if index == crate::MAX_TOOL_CALLS_PER_TURN - 1 {
                ("write_file", r#"{"path":"progress.txt","content":"x"}"#)
            } else {
                ("read_file", r#"{"path":"note.txt"}"#)
            };
            events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: name.to_owned(),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: if name == "read_file" && json == r#"{"path":"note.txt"}"# {
                    serde_json::json!({"path":"note.txt","limit":current*crate::MAX_TOOL_CALLS_PER_TURN+index+1}).to_string()
                } else {
                    json.to_owned()
                },
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(qq_provider::ProviderEvent::Completed {
            usage: self
                .metered_empty_checkpoint
                .then_some(provider_usage(1, 1)),
        }));
        Box::pin(stream::iter(events))
    }
}

struct TurnTextProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for TurnTextProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        if turn == 0 {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "Let me look. ".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0".to_owned(),
                    json: r#"{"path":"note.txt"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "call_0".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        } else {
            text_reply("done")
        }
    }
}

impl Provider for DelayedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let summarizing = request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"));
        self.requests.lock().unwrap().push(request);
        Box::pin(async_stream! {
            yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: if summarizing { valid_summary("answer") } else { "answer".to_owned() },
            });
            yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
        })
    }
}

struct ApprovalLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    tool: &'static str,
    arguments: &'static str,
    tool_turns: usize,
}

impl RuntimeLoader for ApprovalLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = ApprovalProvider {
            requests: Arc::clone(&self.requests),
            turn: StdMutex::new(0),
            tool: self.tool,
            arguments: self.arguments,
            tool_turns: self.tool_turns,
            usage: None,
        };
        load_provider(provider, &request)
    }
}

struct ApprovalProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    turn: StdMutex<usize>,
    tool: &'static str,
    arguments: &'static str,
    tool_turns: usize,
    usage: Option<qq_provider::ProviderUsage>,
}

impl Provider for ApprovalProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let mut current = self.turn.lock().unwrap();
        let turn = *current;
        *current += 1;
        drop(current);
        if turn < self.tool_turns {
            provider_script(
                tool_call(
                    format!("call_{turn}"),
                    self.tool.to_owned(),
                    self.arguments.to_owned(),
                )
                .into_iter()
                .chain([completed(self.usage)]),
            )
        } else {
            provider_script([text_delta("done"), completed(self.usage)])
        }
    }
}

struct ScriptedRunsLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    runs: Vec<Vec<(&'static str, String)>>,
    loads: StdMutex<usize>,
}

impl RuntimeLoader for ScriptedRunsLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let mut loads = self.loads.lock().unwrap();
        let script = self.runs.get(*loads).cloned().unwrap_or_default();
        *loads += 1;
        drop(loads);
        let provider = ScriptedRunProvider {
            requests: Arc::clone(&self.requests),
            script,
            turn: StdMutex::new(0),
        };
        load_provider(provider, &request)
    }
}

struct ScriptedRunProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: Vec<(&'static str, String)>,
    turn: StdMutex<usize>,
}

impl Provider for ScriptedRunProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let summarizing = request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"));
        self.requests.lock().unwrap().push(request);
        if summarizing {
            return text_reply(valid_summary("done"));
        }
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        match self.script.get(current) {
            Some((name, arguments)) => provider_script(
                tool_call(
                    format!("call_{current}"),
                    (*name).to_owned(),
                    arguments.clone(),
                )
                .into_iter()
                .chain([completed(None)]),
            ),
            None => text_reply("done"),
        }
    }
}

async fn scripted_runs_harness(
    mode: ApprovalMode,
    runs: Vec<Vec<(&'static str, String)>>,
) -> SessionFixture {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    SessionFixture::open(
        Arc::new(ScriptedRunsLoader {
            requests: Arc::clone(&requests),
            runs,
            loads: StdMutex::new(0),
        }),
        mode,
    )
    .await
    .recording(&requests)
}

async fn submit_prompt(harness: &SessionFixture, prompt: &str) -> RunId {
    harness.submit(prompt).await
}

struct ApprovalHarness {
    fixture: SessionFixture,
    run_id: RunId,
}

impl std::ops::Deref for ApprovalHarness {
    type Target = SessionFixture;

    fn deref(&self) -> &SessionFixture {
        &self.fixture
    }
}

impl std::ops::DerefMut for ApprovalHarness {
    fn deref_mut(&mut self) -> &mut SessionFixture {
        &mut self.fixture
    }
}

async fn approval_harness(
    mode: ApprovalMode,
    tool: &'static str,
    arguments: &'static str,
    tool_turns: usize,
    approval_timeout: Option<Duration>,
) -> ApprovalHarness {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let fixture = SessionFixture::open_with(
        Arc::new(ApprovalLoader {
            requests: Arc::clone(&requests),
            tool,
            arguments,
            tool_turns,
        }),
        mode,
        |options| {
            options.max_active_runs = 1;
            options.approval_timeout = approval_timeout;
        },
    )
    .await
    .recording(&requests);
    let run_id = fixture.submit("mutate something").await;
    ApprovalHarness { fixture, run_id }
}

async fn collect_until_approval_requested(
    events: &mut SessionEventStream,
) -> (Vec<SessionEventEnvelope>, ToolCallSnapshot) {
    within(async {
        let mut observed = Vec::new();
        loop {
            let event = events.next().await.unwrap().unwrap();
            let requested = match &event.event {
                SessionEvent::ToolApprovalRequested { tool_call, .. } => Some(tool_call.clone()),
                _ => None,
            };
            observed.push(event);
            if let Some(tool_call) = requested {
                return (observed, tool_call);
            }
        }
    })
    .await
    .unwrap()
}

async fn respond_approval(
    runtime: &SessionRuntime,
    run_id: RunId,
    tool_call_id: ToolCallId,
    decision: ApprovalDecision,
) -> Result<CommandReceipt, SessionRuntimeError> {
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id,
                decision,
            },
        )
        .await
}

async fn steer(
    runtime: &SessionRuntime,
    run_id: RunId,
    text: &str,
    interrupt: bool,
) -> Result<CommandReceipt, SessionRuntimeError> {
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SteerRun {
                run_id,
                input: vec![InputPart::text(text)],
                interrupt,
            },
        )
        .await
}

async fn test_runtime() -> (TempDir, SessionRuntime) {
    let directory = test_directory();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    (directory, runtime)
}

struct RecordingApprovalLoader {
    models: Arc<StdMutex<Vec<Option<String>>>>,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for RecordingApprovalLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let route = request.model.model.clone().unwrap();
        self.models.lock().unwrap().push(request.model.model);
        let provider = ApprovalProvider {
            requests: Arc::clone(&self.requests),
            turn: StdMutex::new(0),
            tool: "__test_mutate",
            arguments: "{}",
            tool_turns: 1,
            usage: Some(provider_usage(7, 1)),
        };
        {
            let loaded = ProviderLoader::new(provider)
                .route(route)
                .loaded(&request.workspace);
            Box::pin(async move { Ok(loaded) })
        }
    }
}

struct SessionManagementHarness {
    fixture: SessionFixture,
    models: Arc<StdMutex<Vec<Option<String>>>>,
}

impl std::ops::Deref for SessionManagementHarness {
    type Target = SessionFixture;

    fn deref(&self) -> &SessionFixture {
        &self.fixture
    }
}

impl std::ops::DerefMut for SessionManagementHarness {
    fn deref_mut(&mut self) -> &mut SessionFixture {
        &mut self.fixture
    }
}

async fn session_management_harness() -> SessionManagementHarness {
    let models = Arc::new(StdMutex::new(Vec::new()));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let fixture = SessionFixture::open(
        Arc::new(RecordingApprovalLoader {
            models: Arc::clone(&models),
            requests: Arc::clone(&requests),
        }),
        ApprovalMode::Ask,
    )
    .await
    .recording(&requests);
    SessionManagementHarness { fixture, models }
}

async fn claimed_store_fixture() -> (TempDir, Store, ClaimedRun) {
    let directory = test_directory();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, session_id) = store_session(&store, directory.path(), ApprovalMode::default()).await;
    store_command(&store, session_id, "x").await.unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    (directory, store, claimed)
}

struct SaturatedControlLane {
    release: std::sync::mpsc::Sender<()>,
    blocked: tokio::task::JoinHandle<Result<(), SessionRuntimeError>>,
    fillers: Vec<tokio::task::JoinHandle<Result<(), SessionRuntimeError>>>,
}

impl SaturatedControlLane {
    async fn release(self) {
        self.release.send(()).unwrap();
        self.blocked.await.unwrap().unwrap();
        for filler in self.fillers {
            filler.await.unwrap().unwrap();
        }
    }
}

async fn saturate_control_lane(runtime: &SessionRuntime) -> SaturatedControlLane {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let blocked_store = runtime.store().clone();
    let blocked = tokio::spawn(async move {
        blocked_store
            .call(Priority::Control, move |_| {
                let _ = entered_tx.send(());
                release_rx
                    .recv()
                    .map_err(|_| SessionRuntimeError::Unavailable)
            })
            .await
    });
    entered_rx.await.unwrap();
    let mut fillers = Vec::new();
    for _ in 0..store::CONTROL_QUEUE_CAPACITY {
        let store = runtime.store().clone();
        let mut call = Box::pin(async move { store.call(Priority::Control, |_| Ok(())).await });
        match futures_util::poll!(call.as_mut()) {
            std::task::Poll::Pending => fillers.push(tokio::spawn(call)),
            std::task::Poll::Ready(Err(SessionRuntimeError::Overloaded)) => break,
            std::task::Poll::Ready(other) => panic!("unexpected filler result: {other:?}"),
        }
    }
    assert!(matches!(
        runtime.store().call(Priority::Control, |_| Ok(())).await,
        Err(SessionRuntimeError::Overloaded)
    ));
    SaturatedControlLane {
        release,
        blocked,
        fillers,
    }
}

async fn resolve_workspace(
    runtime: &SessionRuntime,
    path: &std::path::Path,
) -> (WorkspaceId, EventCursor) {
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: path.to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
        panic!("unexpected receipt")
    };
    (workspace_id, receipt.committed_through)
}

async fn create_session(
    runtime: &SessionRuntime,
    workspace_id: WorkspaceId,
    parent_id: Option<SessionId>,
) -> CommandReceipt {
    create_session_with_mode(runtime, workspace_id, parent_id, ApprovalMode::default()).await
}

async fn create_session_with_mode(
    runtime: &SessionRuntime,
    workspace_id: WorkspaceId,
    parent_id: Option<SessionId>,
    approval_mode: ApprovalMode,
) -> CommandReceipt {
    runtime
        .command(
            CommandId::generate().unwrap(),
            create_session_command(workspace_id, parent_id, approval_mode),
        )
        .await
        .unwrap()
}

fn create_session_command(
    workspace_id: WorkspaceId,
    parent_id: Option<SessionId>,
    approval_mode: ApprovalMode,
) -> SessionCommand {
    SessionCommand::CreateSession {
        workspace_id,
        parent_id,
        model: ModelSelection {
            model_is_fallback: false,
            model: Some("test/model".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        },
        approval_mode,
        profile: AgentProfileId::default(),
        reasoning_effort: None,
        correlation: Correlation::default(),
    }
}

mod reference_assembly {
    use super::*;

    pub(super) fn reference_load_model_context(
        transaction: &Connection,
        session_id: SessionId,
        through_ordinal: u64,
    ) -> Result<(Vec<Message>, bool), SessionRuntimeError> {
        let compaction = latest_compaction(transaction, session_id)?;
        let cutoff_ordinal = compaction
            .as_ref()
            .map_or(0, |compaction| compaction.cutoff_ordinal);
        let through_ordinal = through_ordinal.min(u64::try_from(i64::MAX).unwrap_or(u64::MAX));
        let mut statement = transaction.prepare(
            "SELECT id FROM messages
                 WHERE session_id = ?1 AND ordinal <= ?2 AND ordinal > ?3
                   AND role = 'user' AND steering = 0
                   AND state IN ('complete', 'cancelled', 'failed', 'interrupted')
                 ORDER BY ordinal",
        )?;
        let message_ids = statement
            .query_map(
                params![session_id.to_string(), through_ordinal, cutoff_ordinal],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let watermark: Option<(String, u32)> = transaction.query_row(
            "SELECT (SELECT r.id FROM messages m JOIN runs r ON r.id = m.run_id
                     WHERE m.session_id = s.id AND m.ordinal = s.prune_through_ordinal
                       AND m.role = 'user' AND m.steering = 0),
                    s.prune_through_turn
                 FROM sessions s WHERE s.id = ?1",
            [session_id.to_string()],
            |row| {
                Ok(row
                    .get::<_, Option<String>>(0)?
                    .zip(row.get::<_, Option<u32>>(1)?))
            },
        )?;
        let watermark_ordinal: Option<u64> = transaction.query_row(
            "SELECT prune_through_ordinal FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        let watermark = watermark.filter(|_| watermark_ordinal.is_some());
        let mut prune_limit = 0_usize;
        let mut context = Vec::new();
        if let Some(compaction) = compaction {
            context.push(Message::user(format!(
                "{COMPACTION_SUMMARY_PREAMBLE}\n\n{}",
                with_exact_inputs(
                    transaction,
                    &session_id.to_string(),
                    RecordScope::Session {
                        cutoff_ordinal: compaction.cutoff_ordinal
                    },
                    compaction.summary
                )?
            )));
        }
        for id in message_ids {
            let snapshot = load_message(transaction, parse_id(&id)?)?;
            let snapshot_ordinal: u64 = transaction.query_row(
                "SELECT ordinal FROM messages WHERE id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )?;
            if snapshot.role != MessageRole::User {
                return Err(SessionRuntimeError::CODEC);
            }
            context.push(Message::user(render_message(
                transaction,
                &id,
                snapshot.output,
            )?));
            let status: String = transaction.query_row(
                "SELECT status FROM runs WHERE id = ?1",
                [snapshot.run_id.to_string()],
                |row| row.get(0),
            )?;
            if matches!(
                status.as_str(),
                "completed" | "cancelled" | "failed" | "interrupted" | "running"
            ) {
                let has_turns: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM model_turns WHERE run_id = ?1)",
                    [snapshot.run_id.to_string()],
                    |row| row.get(0),
                )?;
                if has_turns {
                    let through = watermark
                        .as_ref()
                        .filter(|(run, _)| *run == snapshot.run_id.to_string())
                        .map(|(_, turn)| *turn);
                    reference_append_run_turns(
                        transaction,
                        snapshot.run_id,
                        &mut context,
                        through.map(|turn| (turn, &mut prune_limit)),
                    )?;
                } else {
                    reference_append_legacy_run_messages(
                        transaction,
                        snapshot.run_id,
                        &mut context,
                    )?;
                    let mut statement = transaction.prepare(
                        "SELECT text, delivery_ordinal FROM child_deliveries
                             WHERE parent_run_id = ?1 AND delivered_at_ms IS NOT NULL
                         UNION ALL
                         SELECT text, delivery_ordinal FROM child_reports
                             WHERE parent_run_id = ?1
                         ORDER BY 2",
                    )?;
                    for text in statement
                        .query_map([snapshot.run_id.to_string()], |row| row.get::<_, String>(0))?
                    {
                        context.push(Message::user(text?));
                    }
                }
            }
            if matches!(status.as_str(), "cancelled" | "failed" | "interrupted") {
                let outcome_json: String = transaction.query_row(
                    "SELECT outcome_json FROM runs WHERE id = ?1",
                    [snapshot.run_id.to_string()],
                    |row| row.get(0),
                )?;
                let outcome: RunOutcome = serde_json::from_str(&outcome_json)?;
                if let Some(notice) = runtime_notice(&outcome) {
                    context.push(Message::user(notice));
                }
            }
            if watermark_ordinal.is_some_and(|watermark| snapshot_ordinal < watermark) {
                prune_limit = context.len();
            }
        }
        let context_rewritten =
            prune_stale_tool_results(&mut context[..prune_limit], &HashMap::new());
        Ok((context, context_rewritten))
    }
    fn reference_append_legacy_run_messages(
        connection: &Connection,
        run_id: RunId,
        context: &mut Vec<Message>,
    ) -> Result<(), SessionRuntimeError> {
        let mut statement = connection.prepare(
            "SELECT id FROM messages
                 WHERE run_id = ?1 AND role = 'assistant' AND state = 'complete'
                 ORDER BY turn_ordinal, ordinal",
        )?;
        let message_ids = statement
            .query_map([run_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for message_id in message_ids {
            let message = load_message(connection, parse_id(&message_id)?)?;
            let content = if message.output.is_empty() {
                message.refusal
            } else {
                message.output
            };
            if !content.trim().is_empty() {
                context.push(Message::assistant(content));
            }
        }
        Ok(())
    }
    fn render_message(
        transaction: &Connection,
        id: &str,
        output: String,
    ) -> Result<String, SessionRuntimeError> {
        let mut statement = transaction.prepare(
            "SELECT a.path, a.window_start, a.window_end, a.window_total, b.content
                 FROM message_attachments a
                 JOIN attachment_blobs b
                   ON b.session_id = a.session_id AND b.blob_key = a.blob_key
                 WHERE a.message_id = ?1
                 ORDER BY a.ordinal",
        )?;
        let stored = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<u64>>(1)?,
                    row.get::<_, Option<u64>>(2)?,
                    row.get::<_, Option<u64>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if stored.is_empty() {
            return Ok(output);
        }
        let input_json: Option<String> = transaction.query_row(
            "SELECT input_json FROM messages WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        let parts = parse_input_parts(input_json.as_deref())?;
        let stored: Vec<_> = stored
            .into_iter()
            .map(|(path, start, end, total, content)| {
                let window = match (start, end, total) {
                    (Some(start), Some(end), Some(total)) => {
                        Some((start as usize, end as usize, total as usize))
                    }
                    _ => None,
                };
                (
                    path,
                    window,
                    content.map(|bytes| String::from_utf8(bytes).unwrap()),
                )
            })
            .collect();
        Ok(crate::input::render_resolved_prompt(
            &crate::input::render_text_parts(&parts),
            stored
                .iter()
                .map(|(path, window, content)| (path.as_str(), *window, content.as_deref())),
        ))
    }

    fn reference_append_run_turns(
        transaction: &Connection,
        run_id: RunId,
        context: &mut Vec<Message>,
        mut watermark: Option<(u32, &mut usize)>,
    ) -> Result<(), SessionRuntimeError> {
        let mut statement = transaction.prepare(
            "SELECT turn_ordinal, assistant_content_json, truncated, notice FROM model_turns
                 WHERE run_id = ?1 ORDER BY turn_ordinal",
        )?;
        let turns = statement
            .query_map([run_id.to_string()], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut statement = transaction.prepare(
            "SELECT turn_ordinal, output, id FROM messages
                 WHERE run_id = ?1 AND steering = 1 AND state = 'complete'
                 ORDER BY turn_ordinal, ordinal",
        )?;
        let rows = statement
            .query_map([run_id.to_string()], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut steering = rows
            .into_iter()
            .map(|(turn, output, id)| Ok((turn, render_message(transaction, &id, output)?)))
            .collect::<Result<std::collections::VecDeque<_>, SessionRuntimeError>>()?;
        let mut statement = transaction.prepare(
            "SELECT turn_ordinal, text, delivery_ordinal FROM child_deliveries
                 WHERE parent_run_id = ?1 AND delivered_at_ms IS NOT NULL
             UNION ALL
             SELECT turn_ordinal, text, delivery_ordinal FROM child_reports
                 WHERE parent_run_id = ?1
             ORDER BY 3",
        )?;
        let mut delivered = statement
            .query_map([run_id.to_string()], |row| {
                Ok((row.get::<_, Option<u32>>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<std::collections::VecDeque<_>, _>>()?;
        drop(statement);
        let marker: Option<(String, u32)> = transaction
            .query_row(
                "SELECT summary, turn_cutoff FROM session_compactions
                     WHERE scope_run_id = ?1 ORDER BY rowid DESC LIMIT 1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let mut turns = turns;
        if let Some((summary, turn_cutoff)) = marker {
            let replaced = turns
                .iter()
                .take_while(|(ordinal, _, _, _)| *ordinal <= turn_cutoff)
                .count();
            if replaced > 0 {
                turns.drain(..replaced);
                let first_kept = turns.first().map_or(turn_cutoff, |turn| turn.0);
                if let Some(first) = turns.first_mut() {
                    first.3 = None;
                }
                while steering
                    .front()
                    .is_some_and(|(applied_before, _)| *applied_before <= first_kept)
                {
                    steering.pop_front();
                }
                while delivered
                    .front()
                    .is_some_and(|(before, _)| before.is_some_and(|before| before <= first_kept))
                {
                    delivered.pop_front();
                }
                context.push(Message::user(format!(
                    "{IN_RUN_COMPACTION_PREAMBLE}\n\n{summary}"
                )));
                if let Some((_, limit)) = watermark.as_mut() {
                    **limit = context.len();
                }
            }
        }
        for (turn_ordinal, content_json, truncated, notice) in turns {
            while steering
                .front()
                .is_some_and(|(applied_before, _)| *applied_before <= turn_ordinal)
            {
                let (_, text) = steering.pop_front().expect("front was just checked");
                context.push(Message::user(text));
            }
            while delivered
                .front()
                .is_some_and(|(before, _)| before.is_some_and(|before| before <= turn_ordinal))
            {
                let (_, text) = delivered.pop_front().expect("front was just checked");
                context.push(Message::user(text));
            }
            match notice.as_deref() {
                None => {}
                Some("report") => context.push(Message::user(crate::SLICE_CHECKPOINT_NOTICE)),
                Some("continuation") => {
                    context.push(Message::user(crate::SLICE_CONTINUATION_NOTICE));
                }
                Some("stall_report") => context.push(Message::user(crate::STALL_REPORT_NOTICE)),
                Some("root_stall_report") => {
                    context.push(Message::user(crate::ROOT_STALL_REPORT_NOTICE))
                }
                Some("final_answer") => {
                    context.push(Message::user(crate::SUBAGENT_FINAL_ANSWER_NOTICE));
                }
                Some(_) => return Err(SessionRuntimeError::CODEC),
            }
            let content: Vec<ContentBlock> =
                serde_json::from_str::<Vec<PersistedContentBlock>>(&content_json)?
                    .into_iter()
                    .map(ContentBlock::from)
                    .collect();

            let mut statement = transaction.prepare(
                "SELECT c.provider_call_id, c.result, c.is_error, c.id, s.digest
                     FROM tool_calls c LEFT JOIN tool_spills s ON s.tool_call_id = c.id
                     WHERE c.run_id = ?1 AND c.turn_ordinal = ?2 AND c.result IS NOT NULL
                     ORDER BY c.call_ordinal",
            )?;
            let mut recorded = statement
                .query_map(params![run_id.to_string(), turn_ordinal], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        (
                            row.get::<_, String>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ),
                    ))
                })?
                .collect::<Result<HashMap<String, (String, bool, String, Option<String>)>, _>>()?;
            drop(statement);
            let mut turn_output =
                crate::tools::TurnOutputBudget::for_results(content.iter().filter_map(|block| {
                    match block {
                        ContentBlock::ToolCall { id, name, .. } => Some((
                            name.as_str(),
                            recorded
                                .get(id)
                                .map_or(INTERRUPTED_TOOL_RESULT, |row| row.0.as_str()),
                        )),
                        _ => None,
                    }
                }));
            let results = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, name, .. } => Some(match recorded.remove(id) {
                        Some((mut content, is_error, tool_call_id, spill_digest)) => {
                            let tool_call_id = parse_id::<ToolCallId>(&tool_call_id).unwrap();
                            let spill_handle = spill_digest.as_deref().map(|digest| {
                                crate::tools::output::result_handle(name, tool_call_id, digest)
                            });
                            let recall = match &spill_handle {
                                Some(handle) => crate::tools::ResultRecall::Spill(handle),
                                None => crate::tools::ResultRecall::StoredResult {
                                    tool: name,
                                    call: tool_call_id,
                                },
                            };
                            turn_output.admit(name, &mut content, recall);
                            ContentBlock::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error,
                            }
                        }
                        None => {
                            let mut content = INTERRUPTED_TOOL_RESULT.to_owned();
                            turn_output.admit(name, &mut content, crate::tools::ResultRecall::None);
                            ContentBlock::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error: true,
                            }
                        }
                    }),
                    ContentBlock::Text { .. }
                    | ContentBlock::ToolResult { .. }
                    | ContentBlock::Image { .. } => None,
                })
                .collect::<Vec<_>>();
            let message = Message::new(Role::Assistant, content);
            if truncated && !message.has_content() && results.is_empty() {
                continue;
            }
            context.push(message);
            if !results.is_empty() {
                context.push(Message::tool_results(results));
            }
            if truncated {
                context.push(Message::user(crate::OUTPUT_TRUNCATED_CONTINUE_NOTICE));
            }
            if let Some((through, limit)) = watermark.as_mut()
                && turn_ordinal <= *through
            {
                **limit = context.len();
            }
        }
        for (_, text) in steering {
            context.push(Message::user(text));
        }
        for (_, text) in delivered {
            context.push(Message::user(text));
        }
        Ok(())
    }
}

fn mark_prune_seam(database: &std::path::Path, session_id: SessionId) {
    let connection = Connection::open(database).unwrap();
    connection
        .execute(
            "UPDATE sessions SET (prune_through_ordinal, prune_through_turn) = (
                 SELECT m.ordinal, t.turn_ordinal
                 FROM messages m JOIN model_turns t ON t.run_id = m.run_id
                 WHERE m.session_id = ?1 AND m.role = 'user' AND m.steering = 0
                 ORDER BY m.ordinal DESC, t.turn_ordinal DESC LIMIT 1)
             WHERE id = ?1",
            [session_id.to_string()],
        )
        .unwrap();
}

fn assert_assembly_matches_reference(database: &std::path::Path, session_id: SessionId) {
    let mut connection = Connection::open(database).unwrap();
    let transaction = connection.transaction().unwrap();
    let (joined, joined_rewritten) =
        load_model_context_with_rewrite_status(&transaction, session_id, u64::MAX).unwrap();
    let (reference, reference_rewritten) =
        reference_assembly::reference_load_model_context(&transaction, session_id, u64::MAX)
            .unwrap();
    assert_eq!(joined_rewritten, reference_rewritten);
    assert_eq!(joined.len(), reference.len(), "message count");
    for (index, (joined, reference)) in joined.iter().zip(&reference).enumerate() {
        assert_eq!(joined.role(), reference.role(), "role at {index}");
        assert_eq!(joined.content(), reference.content(), "content at {index}");
    }
    let (joined, _) = load_model_context_with_rewrite_status(&transaction, session_id, 1).unwrap();
    let (reference, _) =
        reference_assembly::reference_load_model_context(&transaction, session_id, 1).unwrap();
    assert_eq!(joined.len(), reference.len(), "bounded message count");
}

async fn collect_through_finished(events: &mut SessionEventStream) -> Vec<SessionEventEnvelope> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let finished = matches!(event.event, SessionEvent::RunFinished { .. });
            observed.push(event);
            if finished {
                break;
            }
        }
        observed
    })
    .await
    .unwrap()
}

async fn collect_through_compacted(events: &mut SessionEventStream) -> Vec<SessionEventEnvelope> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let compacted = matches!(event.event, SessionEvent::SessionCompacted { .. });
            observed.push(event);
            if compacted {
                break;
            }
        }
        observed
    })
    .await
    .unwrap()
}

fn summarized_turns(request: &ModelRequest) -> usize {
    request
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => text.lines().find_map(|line| {
                line.strip_prefix("turns_done=")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            }),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn valid_summary(body: &str) -> String {
    format!(
        "1. Intent: {body}\n2. Decisions and constraints: {body}\n3. Work state: {body}\n\
         4. Open problems: {body}\n5. Next step: {body}"
    )
}

async fn compact_session(runtime: &SessionRuntime, session_id: SessionId) -> RunId {
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CompactSession { session_id },
        )
        .await
        .unwrap();
    let CommandOutcome::CompactionQueued { run_id, .. } = receipt.outcome else {
        panic!("unexpected receipt")
    };
    run_id
}

fn delivered_answers(requests: &[ModelRequest]) -> Vec<(String, bool)> {
    let mut seen = Vec::new();
    for request in requests {
        for text in request_texts(request) {
            let Some(rest) = text.strip_prefix(
                "[QQ runtime notice; not a user instruction]\nA sub-agent you started has finished.\n",
            ) else {
                continue;
            };
            let (header, answer) = rest.split_once(":\n\n").expect("a delivery notice header");
            let entry = (answer.to_owned(), header.ends_with("Its answer"));
            if !seen.contains(&entry) {
                seen.push(entry);
            }
        }
    }
    seen
}

fn request_texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages()
        .iter()
        .map(|message| {
            message
                .content()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}

fn assert_tool_results_are_exact(messages: &[Message]) {
    let mut calls = HashMap::<String, Vec<usize>>::new();
    let mut results = HashMap::<String, Vec<usize>>::new();
    for (message_index, message) in messages.iter().enumerate() {
        for block in message.content() {
            match block {
                ContentBlock::ToolCall { id, .. } => {
                    calls.entry(id.clone()).or_default().push(message_index);
                }
                ContentBlock::ToolResult { call_id, .. } => {
                    results
                        .entry(call_id.clone())
                        .or_default()
                        .push(message_index);
                }
                ContentBlock::Text { .. } | ContentBlock::Image { .. } => {}
            }
        }
    }
    for (id, call_positions) in &calls {
        assert_eq!(call_positions.len(), 1, "duplicate ToolCall for {id}");
        let result_positions = results.get(id).unwrap_or_else(|| {
            panic!("missing ToolResult for {id}");
        });
        assert_eq!(result_positions.len(), 1, "duplicate ToolResult for {id}");
        assert!(
            result_positions[0] > call_positions[0],
            "ToolResult for {id} must follow its ToolCall"
        );
    }
    for id in results.keys() {
        assert!(calls.contains_key(id), "orphaned ToolResult for {id}");
    }
}

#[derive(Clone)]
enum AutoCompactScript {
    Text(String),
    Truncated(String),
    TextThenTransportFault(String),
    ReadNoteThenText(String),
    ReadNotesThenText {
        notes: usize,
        text: String,
    },
    ReadNoteRepeatedly {
        turns: usize,
        text: String,
    },
    CallThenText {
        tool: String,
        arguments: String,
        text: String,
    },
    ToolRepeatedlyWithSummaries {
        turns: usize,
        text: String,
        summary: String,
    },

    WritesRepeatedlyWithSummaries {
        turns: usize,
        text: String,
        summary: String,
    },
    ToolRepeatedlyWithProviderOverflow {
        turns: usize,
        overflow_at: usize,
        text: String,
        summary: String,
        overflows: Arc<AtomicUsize>,
    },
    ToolRepeatedlyWithEarlyFault {
        turns: usize,
        fault_at: usize,
        text: String,
        summary: String,
        faults: Arc<AtomicUsize>,
    },
    ShellBatchesAcrossACheckpoint {
        calls: usize,
        text: String,
    },
    ToolCallWithText {
        text: String,
        tool: String,
    },
    ToolRepeatedlyWithTruncatedSummaries {
        turns: usize,
        text: String,
        summary: String,
        cut: usize,
    },
    SearchHistoryThenText(String, String),
    #[cfg(unix)]
    ShellThenRecall {
        command: String,
        recall: serde_json::Value,
        text: String,
    },
    ReadNoteThenTextMeasured {
        text: String,
        input_tokens: u64,
    },
    Fail,
    ContextOverflow,
    Stall,
    Panic,
    Sequence(Vec<AutoCompactScript>),
    Measured {
        script: Box<AutoCompactScript>,
        bytes_per_token: u64,
    },
}

const SHELL_STEP_PADDING: usize = 6_000;

fn bulk_step_arguments(step: usize, padding_bytes: usize) -> String {
    serde_json::json!({
        "result": format!("shell exit=0 elapsed=0.0 bytes={}\nstep {step}\n", padding_bytes + 7),
        "padding_bytes": padding_bytes,
    })
    .to_string()
}

struct AutoCompactLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    scripts: Vec<AutoCompactScript>,
    loads: StdMutex<usize>,
    context_window: Option<u32>,
    max_output_tokens: u32,
    provider_identity: bool,
}

impl RuntimeLoader for AutoCompactLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let mut loads = self.loads.lock().unwrap();
        let script = self
            .scripts
            .get(*loads)
            .cloned()
            .unwrap_or_else(|| AutoCompactScript::Text("done".to_owned()));
        *loads += 1;
        drop(loads);
        let provider = AutoCompactProvider {
            requests: Arc::clone(&self.requests),
            script,
            served: StdMutex::new(0),
        };
        let context_window = self.context_window;
        let max_output_tokens = self.max_output_tokens;
        let provider_identity = self.provider_identity;
        Box::pin(async move {
            Runtime::new(provider, "test-model", max_output_tokens)
                .map(|runtime| runtime.with_context_window(context_window))
                .map(|runtime| {
                    let mut resolved = test_resolved_model(
                        "test/model",
                        runtime.model.to_string(),
                        runtime.max_output_tokens,
                        None,
                    );
                    resolved.context_window = context_window;
                    if !provider_identity {
                        resolved.request_shape = None;
                    }
                    loaded_runtime_with_model(runtime, &request.workspace, resolved)
                })
                .map_err(load_error)
        })
    }
}

struct AutoCompactProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: AutoCompactScript,
    served: StdMutex<usize>,
}

impl Provider for AutoCompactProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        if let AutoCompactScript::Measured {
            script,
            bytes_per_token,
        } = &self.script
        {
            let bytes = crate::measure_messages(request.messages())
                + request.system().map_or(0, |system| system.len() as u64)
                + request
                    .tools()
                    .iter()
                    .map(|tool| {
                        (tool.name().len()
                            + tool.description().len()
                            + tool.input_schema().get().len()) as u64
                    })
                    .sum::<u64>();
            let usage = provider_usage(bytes.div_ceil(*bytes_per_token), 1);
            let inner = AutoCompactProvider {
                requests: Arc::clone(&self.requests),
                script: (**script).clone(),
                served: StdMutex::new(0),
            };
            return Box::pin(inner.stream(request).map(move |event| match event {
                Ok(qq_provider::ProviderEvent::Completed { .. }) => {
                    Ok(qq_provider::ProviderEvent::Completed { usage: Some(usage) })
                }
                Ok(qq_provider::ProviderEvent::Incomplete { reason, .. }) => {
                    Ok(qq_provider::ProviderEvent::Incomplete {
                        usage: Some(usage),
                        reason,
                    })
                }
                other => other,
            }));
        }
        if let AutoCompactScript::Sequence(scripts) = &self.script {
            let mut served = self.served.lock().unwrap();
            let script = scripts
                .get(*served)
                .or(scripts.last())
                .cloned()
                .expect("a sequence script has at least one entry");
            *served += 1;
            drop(served);
            let step = AutoCompactProvider {
                requests: Arc::clone(&self.requests),
                script,
                served: StdMutex::new(0),
            };
            return step.stream(request);
        }
        let already_read = request.messages().last().is_some_and(|message| {
            message
                .content()
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        let prior_results: Vec<String> = request
            .messages()
            .iter()
            .flat_map(|message| message.content().iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        let summarized_before = summarized_turns(&request);
        let last_text = request
            .messages()
            .last()
            .and_then(|message| {
                message.content().iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        self.requests.lock().unwrap().push(request);
        match &self.script {
            AutoCompactScript::Text(text) => text_reply(text.clone()),
            AutoCompactScript::TextThenTransportFault(text) => Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                Err(qq_provider::ProviderError::Transport(
                    "connection reset after visible summary text".to_owned(),
                )),
            ])),
            AutoCompactScript::Truncated(text) => Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                Ok(qq_provider::ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                }),
            ])),
            AutoCompactScript::ToolCallWithText { text, tool } => {
                let id = format!("call_summarizer_{}", prior_results.len());
                let mut events = Vec::new();
                if !text.is_empty() {
                    events.push(Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: text.clone(),
                    }));
                }
                events.extend([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: tool.clone(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: r#"{"path":"canary"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]);
                Box::pin(stream::iter(events))
            }
            AutoCompactScript::CallThenText {
                tool,
                arguments,
                text,
            } => {
                if already_read {
                    text_reply(text.clone())
                } else {
                    provider_script(
                        tool_call("call_tool", tool.clone(), arguments.clone())
                            .into_iter()
                            .chain([completed(None)]),
                    )
                }
            }
            AutoCompactScript::ReadNoteThenText(text) => {
                if already_read {
                    text_reply(text.clone())
                } else {
                    provider_script(
                        tool_call("call_read", "read_file", r#"{"path":"note.txt"}"#)
                            .into_iter()
                            .chain([completed(None)]),
                    )
                }
            }
            AutoCompactScript::ReadNotesThenText { notes, text } => {
                if already_read {
                    text_reply(text.clone())
                } else {
                    let mut events = Vec::new();
                    for note in 0..*notes {
                        let id = format!("call_read_{note}");
                        events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                            id: id.clone(),
                            name: "read_file".to_owned(),
                        }));
                        events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                            id: id.clone(),
                            json: serde_json::json!({"path": format!("note-{note}.txt")})
                                .to_string(),
                        }));
                        events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
                    }
                    events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
                    Box::pin(stream::iter(events))
                }
            }
            AutoCompactScript::ReadNoteThenTextMeasured { text, input_tokens } => {
                let usage = Some(provider_usage(*input_tokens, 1));
                if already_read {
                    provider_script([text_delta(text.clone()), completed(usage)])
                } else {
                    provider_script(
                        tool_call("call_read", "read_file", r#"{"path":"note.txt"}"#)
                            .into_iter()
                            .chain([completed(usage)]),
                    )
                }
            }
            AutoCompactScript::ToolRepeatedlyWithTruncatedSummaries {
                turns,
                text,
                summary,
                cut,
            } => {
                if last_text.contains("cut off at the output token limit") {
                    let summarized = prior_results.len() + summarized_before;
                    let full = format!("{summary}\nturns_done={summarized}");
                    return text_reply(full[*cut..].to_owned());
                }
                if last_text.contains("Summarize this conversation") {
                    let summarized = prior_results.len() + summarized_before;
                    let full = format!("{summary}\nturns_done={summarized}");
                    return Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta {
                            text: full[..*cut].to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Incomplete {
                            usage: None,
                            reason: qq_provider::IncompleteReason::OutputTokens,
                        }),
                    ]));
                }
                let done = prior_results.len() + summarized_before;
                if done >= *turns {
                    text_reply(text.clone())
                } else {
                    let id = format!("call_shell_{done}");
                    provider_script(
                        tool_call(
                            id,
                            "__test_mutate",
                            bulk_step_arguments(done, SHELL_STEP_PADDING),
                        )
                        .into_iter()
                        .chain([completed(None)]),
                    )
                }
            }
            AutoCompactScript::ToolRepeatedlyWithSummaries {
                turns,
                text,
                summary,
            }
            | AutoCompactScript::WritesRepeatedlyWithSummaries {
                turns,
                text,
                summary,
            } => {
                if last_text.contains("Summarize this conversation") {
                    if summary == "__STALL__" {
                        return Box::pin(stream::pending());
                    }
                    let summarized = prior_results.len() + summarized_before;
                    return text_reply(format!("{summary}\nturns_done={summarized}"));
                }
                let done = prior_results.len() + summarized_before;
                if done >= *turns {
                    text_reply(text.clone())
                } else {
                    let id = format!("call_shell_{done}");
                    Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::ToolCallStarted {
                            id: id.clone(),
                            name: if matches!(
                                self.script,
                                AutoCompactScript::WritesRepeatedlyWithSummaries { .. }
                            ) {
                                "write_file".to_owned()
                            } else {
                                "__test_mutate".to_owned()
                            },
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                            id: id.clone(),
                            json: if matches!(
                                self.script,
                                AutoCompactScript::WritesRepeatedlyWithSummaries { .. }
                            ) {
                                serde_json::json!({"path": format!("step-{done}.txt"), "content": "x".repeat(2000)}).to_string()
                            } else {
                                bulk_step_arguments(done, SHELL_STEP_PADDING)
                            },
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
            AutoCompactScript::ShellBatchesAcrossACheckpoint { calls, text } => {
                if last_text.contains("Summarize this conversation") {
                    let summarized = prior_results.len() + summarized_before;
                    return text_reply(format!(
                        "{}\nturns_done={summarized}",
                        valid_summary("batched shell work")
                    ));
                }
                if last_text == crate::SLICE_CHECKPOINT_NOTICE {
                    return text_reply("report: batches so far are done");
                }
                let done = prior_results.len() + summarized_before;
                if done >= *calls {
                    return text_reply(text.clone());
                }
                let mut events = Vec::new();
                for index in done..done + crate::MAX_TOOL_CALLS_PER_TURN {
                    let id = format!("call_batch_{index}");
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "write_file".to_owned(),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: format!(
                            r#"{{"path":"batch/{index}.txt","content":"{}"}}"#,
                            "x".repeat(400)
                        ),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
            AutoCompactScript::ToolRepeatedlyWithEarlyFault {
                turns,
                fault_at,
                text,
                summary,
                faults,
            } => {
                if last_text.contains("Summarize this conversation") {
                    let summarized = prior_results.len() + summarized_before;
                    return text_reply(format!("{summary}\nturns_done={summarized}"));
                }
                if prior_results.len() == *fault_at
                    && summarized_before == 0
                    && faults.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return Box::pin(stream::iter([Err(qq_provider::ProviderError::Api {
                        status: 529,
                        message: "scripted transient outage".to_owned(),
                    })]));
                }
                let done = prior_results.len() + summarized_before;
                if done >= *turns {
                    text_reply(text.clone())
                } else {
                    let id = format!("call_shell_{done}");
                    provider_script(
                        tool_call(
                            id,
                            "__test_mutate",
                            bulk_step_arguments(done, SHELL_STEP_PADDING),
                        )
                        .into_iter()
                        .chain([completed(None)]),
                    )
                }
            }
            AutoCompactScript::ToolRepeatedlyWithProviderOverflow {
                turns,
                overflow_at,
                text,
                summary,
                overflows,
            } => {
                if last_text.contains("Summarize this conversation") {
                    let summarized = prior_results.len() + summarized_before;
                    return text_reply(format!("{summary}\nturns_done={summarized}"));
                }
                if prior_results.len() >= *overflow_at && summarized_before == 0 {
                    overflows.fetch_add(1, Ordering::SeqCst);
                    return Box::pin(stream::iter([Err(
                        qq_provider::ProviderError::ResponseFailed {
                            kind: qq_provider::ProviderErrorKind::ContextExceeded,
                            message: "scripted context overflow".to_owned(),
                        },
                    )]));
                }
                let done = prior_results.len() + summarized_before;
                if done >= *turns {
                    text_reply(text.clone())
                } else {
                    let id = format!("call_shell_{done}");
                    provider_script(
                        tool_call(id, "__test_mutate", bulk_step_arguments(done, 0))
                            .into_iter()
                            .chain([completed(None)]),
                    )
                }
            }
            AutoCompactScript::ReadNoteRepeatedly { turns, text } => {
                if prior_results.len() >= *turns {
                    text_reply(text.clone())
                } else {
                    let id = format!("call_read_{}", prior_results.len());
                    provider_script(
                        tool_call(
                            id,
                            "read_file",
                            serde_json::json!({"path":"note.txt","limit":64+prior_results.len()})
                                .to_string(),
                        )
                        .into_iter()
                        .chain([completed(None)]),
                    )
                }
            }
            AutoCompactScript::SearchHistoryThenText(query, text) => {
                if already_read {
                    text_reply(text.clone())
                } else {
                    provider_script(
                        tool_call(
                            "call_history",
                            crate::runtime::SEARCH_HISTORY_TOOL.to_owned(),
                            serde_json::json!({ "query": query, "limit": 2 }).to_string(),
                        )
                        .into_iter()
                        .chain([completed(None)]),
                    )
                }
            }
            #[cfg(unix)]
            AutoCompactScript::ShellThenRecall {
                command,
                recall,
                text,
            } => {
                let results = &prior_results;
                let handle = results.iter().find_map(|content| {
                    let start = content.find("full output t:")?;
                    let rest = &content[start + "full output ".len()..];
                    let end = rest.find([';', ']'])?;
                    Some(rest[..end].to_owned())
                });
                match (results.len(), handle) {
                    (0, _) => provider_script(
                        tool_call(
                            "call_shell",
                            "shell",
                            serde_json::json!({ "command": command }).to_string(),
                        )
                        .into_iter()
                        .chain([completed(None)]),
                    ),
                    (1, Some(handle)) => {
                        let mut arguments = recall.clone();
                        arguments["handle"] = serde_json::Value::String(handle);
                        provider_script(
                            tool_call("call_recall", "read_tool_result", arguments.to_string())
                                .into_iter()
                                .chain([completed(None)]),
                        )
                    }
                    _ => text_reply(text.clone()),
                }
            }
            AutoCompactScript::Fail => Box::pin(stream::iter([Err(
                qq_provider::ProviderError::Transport("scripted model failure".to_owned()),
            )])),
            AutoCompactScript::ContextOverflow => Box::pin(stream::iter([Err(
                qq_provider::ProviderError::ResponseFailed {
                    kind: qq_provider::ProviderErrorKind::ContextExceeded,
                    message: "scripted context overflow".to_owned(),
                },
            )])),
            AutoCompactScript::Stall => Box::pin(stream::pending()),
            AutoCompactScript::Panic => panic!("injected auto-compaction provider panic"),
            AutoCompactScript::Sequence(_) => unreachable!("sequences dispatch above"),
            AutoCompactScript::Measured { .. } => unreachable!("measured scripts dispatch above"),
        }
    }
}

type AutoCompactHarness = SessionFixture;

async fn auto_compact_harness(scripts: Vec<AutoCompactScript>) -> AutoCompactHarness {
    auto_compact_harness_with_window(scripts, None).await
}

async fn auto_compact_harness_with_window(
    scripts: Vec<AutoCompactScript>,
    context_window: Option<u32>,
) -> AutoCompactHarness {
    auto_compact_harness_with_limits(scripts, context_window, 256).await
}

async fn auto_compact_harness_with_limits(
    scripts: Vec<AutoCompactScript>,
    context_window: Option<u32>,
    max_output_tokens: u32,
) -> AutoCompactHarness {
    auto_compact_harness_with_loader(AutoCompactLoader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        scripts,
        loads: StdMutex::new(0),
        context_window,
        max_output_tokens,
        provider_identity: true,
    })
    .await
}

async fn auto_compact_harness_with_loader(loader: AutoCompactLoader) -> AutoCompactHarness {
    auto_compact_harness_with_loader_and_mode(loader, ApprovalMode::default()).await
}

async fn auto_compact_harness_with_loader_and_mode(
    loader: AutoCompactLoader,
    approval_mode: ApprovalMode,
) -> AutoCompactHarness {
    let requests = Arc::clone(&loader.requests);
    SessionFixture::open(Arc::new(loader), approval_mode)
        .await
        .recording(&requests)
}

async fn queue_prompt(runtime: &SessionRuntime, session_id: SessionId, prompt: String) -> RunId {
    submit(runtime, session_id, prompt).await
}

async fn collect_until(
    events: &mut SessionEventStream,
    stop: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionEventEnvelope> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut observed = Vec::new();
        loop {
            let event = events.next().await.unwrap().unwrap();
            let done = stop(&event.event);
            observed.push(event);
            if done {
                return observed;
            }
        }
    })
    .await
    .unwrap()
}

fn finished_for(run_id: RunId) -> impl Fn(&SessionEvent) -> bool {
    move |event| matches!(event, SessionEvent::RunFinished { run_id: finished, .. } if *finished == run_id)
}

fn position_of(
    observed: &[SessionEventEnvelope],
    predicate: impl Fn(&SessionEvent) -> bool,
) -> usize {
    observed
        .iter()
        .position(|event| predicate(&event.event))
        .unwrap()
}

fn over_threshold_output() -> String {
    "x".repeat(MAX_CONTEXT_BYTES - 100 * 1024)
}

async fn collect_through_finished_generously(
    events: &mut SessionEventStream,
) -> Vec<SessionEventEnvelope> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let finished = matches!(event.event, SessionEvent::RunFinished { .. });
            observed.push(event);
            if finished {
                break;
            }
        }
        observed
    })
    .await
    .unwrap()
}

struct QueueLoader {
    routed: Vec<(&'static str, Arc<dyn Provider>)>,
    queue: StdMutex<Vec<Arc<dyn Provider>>>,
}

impl QueueLoader {
    fn next_provider(&self, request: &RuntimeLoadRequest) -> Arc<dyn Provider> {
        self.routed
            .iter()
            .find(|(model, _)| request.model.model.as_deref() == Some(*model))
            .map(|(_, provider)| Arc::clone(provider))
            .or_else(|| {
                let mut queue = self.queue.lock().unwrap();
                if queue.is_empty() {
                    None
                } else {
                    Some(queue.remove(0))
                }
            })
            .unwrap_or_else(|| Arc::new(StaticTextProvider))
    }
}

impl RuntimeLoader for QueueLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let spawn_model_routes = self
            .routed
            .iter()
            .map(|(model, _)| (*model).to_owned())
            .collect::<Vec<_>>();
        let provider = self.next_provider(&request);
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(spawn_model_routes),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(load_error)
        })
    }
}

struct StaticTextProvider;

impl Provider for StaticTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        text_reply("done")
    }
}

struct HangingProvider;

impl Provider for HangingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::pending())
    }
}

struct MultiSpawnProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    spawns: usize,
    arguments: fn(usize) -> String,
    turn: StdMutex<usize>,
}

impl Provider for MultiSpawnProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            let mut events = Vec::with_capacity(self.spawns * 3 + 1);
            for index in 0..self.spawns {
                let id = format!("call_{index}");
                events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "spawn_agent".to_owned(),
                }));
                events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: (self.arguments)(index),
                }));
                events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
            }
            events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
            Box::pin(stream::iter(events))
        } else {
            text_reply("done")
        }
    }
}

async fn create_claimed_parent(
    store: &Store,
    workspace_path: &Path,
) -> (WorkspaceId, SessionId, ClaimedRun) {
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: workspace_path.to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let root = store
        .command(
            CommandId::generate().unwrap(),
            create_session_command(workspace_id, None, ApprovalMode::default()),
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = root.receipt.outcome else {
        panic!("unexpected receipt")
    };
    store_command(store, session_id, "delegate work")
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    (workspace_id, session_id, claimed)
}

type SpawnHarness = SessionFixture;

async fn spawn_harness_with_loader(
    loader: Arc<dyn RuntimeLoader>,
    max_active_runs: usize,
) -> SpawnHarness {
    SessionFixture::open_with(loader, ApprovalMode::default(), |options| {
        options.max_active_runs = max_active_runs;
    })
    .await
}

async fn spawn_harness(
    routed: Vec<(&'static str, Arc<dyn Provider>)>,
    queue: Vec<Arc<dyn Provider>>,
    max_active_runs: usize,
) -> SpawnHarness {
    spawn_harness_with_loader(
        Arc::new(QueueLoader {
            routed,
            queue: StdMutex::new(queue),
        }),
        max_active_runs,
    )
    .await
}

async fn submit_prompt_to(runtime: &SessionRuntime, session_id: SessionId, prompt: &str) -> RunId {
    submit(runtime, session_id, prompt).await
}

async fn collect_until_run_finished(
    events: &mut SessionEventStream,
    run_id: RunId,
) -> Vec<SessionEventEnvelope> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            let finished = matches!(
                &event.event,
                SessionEvent::RunFinished { run_id: done, .. } if *done == run_id
            );
            observed.push(event);
            if finished {
                break;
            }
        }
        observed
    })
    .await
    .expect("timed out waiting for the run to finish")
}

fn finished_outcome(events: &[SessionEventEnvelope], run_id: RunId) -> Option<RunOutcome> {
    events.iter().find_map(|event| match &event.event {
        SessionEvent::RunFinished {
            run_id: done,
            outcome,
            ..
        } if *done == run_id => Some(outcome.clone()),
        _ => None,
    })
}

struct TextScriptLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    answers: Arc<StdMutex<std::collections::VecDeque<&'static str>>>,
}

struct TextScriptProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    answers: Arc<StdMutex<std::collections::VecDeque<&'static str>>>,
}

impl RuntimeLoader for TextScriptLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = TextScriptProvider {
            requests: Arc::clone(&self.requests),
            answers: Arc::clone(&self.answers),
        };
        load_provider(provider, &request)
    }
}

impl Provider for TextScriptProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let text = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or("out of script");
        if text == "<hang>" {
            return Box::pin(stream::pending());
        }
        text_reply(text.to_owned())
    }
}

type OutputContractHarness = SessionFixture;

async fn output_contract_harness(answers: &[&'static str]) -> OutputContractHarness {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    SessionFixture::open(
        Arc::new(TextScriptLoader {
            requests: Arc::clone(&requests),
            answers: Arc::new(StdMutex::new(answers.iter().copied().collect())),
        }),
        ApprovalMode::ReadOnly,
    )
    .await
    .recording(&requests)
}

fn report_contract(repair_turns: u8) -> qq_protocol::OutputContract {
    qq_protocol::OutputContract {
        schema: serde_json::json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}, "n": {"type": "integer"}},
            "required": ["ok", "n"],
            "additionalProperties": false
        }),
        repair_turns,
    }
}

fn finished_final_output(
    events: &[SessionEventEnvelope],
    run_id: RunId,
) -> Option<Box<FinalOutput>> {
    events.iter().find_map(|event| match &event.event {
        SessionEvent::RunFinished {
            run_id: done,
            final_output,
            ..
        } if *done == run_id => final_output.clone(),
        _ => None,
    })
}

struct WriteChildLoader {
    inner: QueueLoader,
}

impl RuntimeLoader for WriteChildLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = self.inner.next_provider(&request);
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_delegation(qq_protocol::DelegationRoster {
                            roster: Vec::new(),
                            default_role: qq_protocol::DelegationRole::Balanced,
                            max_depth: 1,
                            write_children: true,
                        }),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(load_error)
        })
    }
}

async fn write_child_harness(
    child: Arc<dyn Provider>,
    parent_script: Vec<(&'static str, String)>,
    parent_mode: ApprovalMode,
) -> (SpawnHarness, Arc<StdMutex<Vec<ModelRequest>>>) {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: parent_script,
        turn: StdMutex::new(0),
    });
    let harness = SessionFixture::open_with(
        Arc::new(WriteChildLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![Arc::clone(&parent), parent]),
            },
        }),
        parent_mode,
        |options| {
            options.max_active_runs = 8;
            options.approval_timeout = Some(Duration::from_secs(2));
        },
    )
    .await;
    (harness, parent_requests)
}

struct DepthLoader {
    inner: QueueLoader,
    max_depth: u16,
    write_children: bool,
    pricing: Option<ModelPricing>,
}

impl RuntimeLoader for DepthLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = self.inner.next_provider(&request);
        let spawn_model_routes = self
            .inner
            .routed
            .iter()
            .map(|(model, _)| (*model).to_owned())
            .collect::<Vec<_>>();
        let delegation = qq_protocol::DelegationRoster {
            roster: Vec::new(),
            default_role: qq_protocol::DelegationRole::Balanced,
            max_depth: self.max_depth,
            write_children: self.write_children,
        };
        let pricing = self.pricing.clone();
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime
                            .with_spawn_model_routes(spawn_model_routes)
                            .with_delegation(delegation),
                        &request.workspace,
                        pricing,
                    )
                })
                .map_err(load_error)
        })
    }
}

struct MeteredDelegationProvider {
    inner: ScriptedRunProvider,
    usage: Vec<Option<TokenUsage>>,
    turn: AtomicUsize,
}

impl Provider for MeteredDelegationProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let turn = self.turn.fetch_add(1, Ordering::SeqCst);
        let usage = self.usage[turn.min(self.usage.len() - 1)].map(provider_usage_of);
        Box::pin(self.inner.stream(request).map(move |event| match event {
            Ok(qq_provider::ProviderEvent::Completed { .. }) => {
                Ok(qq_provider::ProviderEvent::Completed { usage })
            }
            event => event,
        }))
    }
}

struct AuditLoader {
    inner: QueueLoader,
    mode: crate::runtime::AuditMode,
    max_revisions: u16,
}

impl RuntimeLoader for AuditLoader {
    fn validate_spawn_model(
        &self,
        _workspace: String,
        selection: ModelSelection,
        advertised: Arc<[String]>,
    ) -> SpawnModelValidationFuture {
        assert!(
            advertised
                .iter()
                .any(|route| Some(route) == selection.model.as_ref()),
            "the auditor's roster route was not retained: {advertised:?}"
        );
        Box::pin(std::future::ready(Ok(())))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = self.inner.next_provider(&request);
        let mode = self.mode;
        let max_revisions = self.max_revisions;
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime
                            .with_delegation(qq_protocol::DelegationRoster {
                                roster: vec![qq_protocol::DelegationRosterEntry {
                                    route: "test/auditor".to_owned(),
                                    role: qq_protocol::DelegationRole::Strong,
                                    note: None,
                                    effort: None,
                                    context_window: None,
                                    max_output_tokens: None,
                                    relative_cost_permille: None,
                                }],
                                default_role: qq_protocol::DelegationRole::Strong,
                                max_depth: 1,
                                write_children: false,
                            })
                            .with_audit(crate::runtime::AuditPolicy {
                                mode,
                                max_revisions,
                                role: qq_protocol::DelegationRole::Strong,
                            }),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(load_error)
        })
    }
}

async fn audit_harness(
    parent: Arc<dyn Provider>,
    auditor: Arc<dyn Provider>,
    mode: crate::runtime::AuditMode,
    max_revisions: u16,
) -> SpawnHarness {
    spawn_harness_with_loader(
        Arc::new(AuditLoader {
            inner: QueueLoader {
                routed: vec![("test/auditor", auditor)],
                queue: StdMutex::new(vec![parent]),
            },
            mode,
            max_revisions,
        }),
        8,
    )
    .await
}

fn audit_events(
    observed: &[SessionEventEnvelope],
    run_id: RunId,
) -> (Option<SessionId>, Vec<AuditRecord>) {
    let started = observed.iter().find_map(|event| match &event.event {
        SessionEvent::RunAuditStarted {
            run_id: audited,
            audit_session_id,
        } if *audited == run_id => Some(*audit_session_id),
        _ => None,
    });
    let completed = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunAuditCompleted {
                run_id: audited,
                audit,
            } if *audited == run_id => Some(audit.clone()),
            _ => None,
        })
        .collect();
    (started, completed)
}

struct ChildBudgetLoader {
    inner: QueueLoader,
    write_children: bool,
    child_context_window: Option<u32>,
    audit: crate::runtime::AuditMode,
}

impl RuntimeLoader for ChildBudgetLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = self.inner.next_provider(&request);
        let write_children = self.write_children;
        let audit = self.audit;
        let child_context_window = (request.model.model.as_deref() == Some("test/child"))
            .then_some(self.child_context_window)
            .flatten();
        let route = request
            .model
            .model
            .clone()
            .unwrap_or_else(|| "test/model".to_owned());
        let mut pricing = budget_pricing();
        if request.model.model.as_deref() == Some("test/child") {
            pricing.input_usd_nanos_per_token = 2_000;
            pricing.output_usd_nanos_per_token = 3_000;
        }
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime_for_route(
                        runtime
                            .with_context_window(child_context_window)
                            .with_delegation(qq_protocol::DelegationRoster {
                                roster: vec![qq_protocol::DelegationRosterEntry {
                                    route: "test/child".to_owned(),
                                    role: qq_protocol::DelegationRole::Balanced,
                                    note: None,
                                    effort: None,
                                    context_window: None,
                                    max_output_tokens: None,
                                    relative_cost_permille: None,
                                }],
                                default_role: qq_protocol::DelegationRole::Balanced,
                                max_depth: 1,
                                write_children,
                            })
                            .with_audit(crate::runtime::AuditPolicy {
                                mode: audit,
                                max_revisions: 1,
                                role: qq_protocol::DelegationRole::Balanced,
                            }),
                        &request.workspace,
                        route,
                        Some(pricing),
                    )
                })
                .map_err(load_error)
        })
    }
}

struct UsageProvider {
    inner: Arc<dyn Provider>,
    usage: Option<TokenUsage>,
}

impl Provider for UsageProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let usage = self.usage.map(provider_usage_of);
        Box::pin(self.inner.stream(request).map(move |event| match event {
            Ok(qq_provider::ProviderEvent::Completed { .. }) => {
                Ok(qq_provider::ProviderEvent::Completed { usage })
            }
            event => event,
        }))
    }
}

async fn child_budget_harness_with_window(
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    write_children: bool,
    audit: crate::runtime::AuditMode,
    child_context_window: Option<u32>,
) -> SpawnHarness {
    SessionFixture::open(
        Arc::new(ChildBudgetLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
            write_children,
            child_context_window,
            audit,
        }),
        ApprovalMode::Full,
    )
    .await
}

async fn submit_child_budget_prompt(harness: &SpawnHarness, limits: RunLimits) -> RunId {
    submit(
        &harness.runtime,
        harness.session_id,
        Prompt::text("delegate").limits(limits),
    )
    .await
}

fn provider_usage_of(usage: TokenUsage) -> qq_provider::ProviderUsage {
    qq_provider::ProviderUsage {
        input_tokens: usage.input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: None,
    }
}

fn budget_pricing() -> ModelPricing {
    ModelPricing {
        input_usd_nanos_per_token: 1_000,
        output_usd_nanos_per_token: 2_000,
        cache_read_usd_nanos_per_token: None,
        cache_write_usd_nanos_per_token: None,
        context_tier: None,
        provenance: "test".to_owned(),
    }
}

fn exhaustion_of(observed: &[SessionEventEnvelope], run_id: RunId) -> BudgetExhaustion {
    match finished_outcome(observed, run_id) {
        Some(RunOutcome::BudgetExhausted { exhaustion }) => *exhaustion,
        other => panic!("expected a budget_exhausted outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn workspace_catalog_pages_registered_roots_and_preserves_identity_after_restart() {
    use qq_protocol::{WorkspaceCatalogCursor, WorkspaceCatalogRequest};
    let directory = test_directory();
    let database = directory.path().join("sessions.sqlite3");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let empty = runtime
        .workspace_catalog(WorkspaceCatalogRequest::default())
        .await
        .unwrap();
    assert!(empty.workspaces.is_empty());
    assert_eq!(empty.next, None);
    assert_eq!(empty.store_id, runtime.store_id());
    let mut expected = Vec::new();
    for name in ["api", "frontend", "worker"] {
        let path = directory.path().join(name);
        std::fs::create_dir(&path).unwrap();
        let (id, _) = resolve_workspace(&runtime, &path).await;
        expected.push(WorkspaceSummary {
            id,
            path: std::fs::canonicalize(&path)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        });
    }
    expected.sort_by_key(|workspace| workspace.id);
    let first = runtime
        .workspace_catalog(WorkspaceCatalogRequest {
            after: None,
            limit: 2,
        })
        .await
        .unwrap();
    assert_eq!(first.workspaces, expected[..2]);
    let cursor = first.next.unwrap();
    assert_eq!(
        cursor,
        WorkspaceCatalogCursor {
            store_id: runtime.store_id(),
            workspace_id: expected[1].id
        }
    );
    for limit in [0, qq_protocol::MAX_WORKSPACE_PAGE + 1] {
        assert!(matches!(
            runtime
                .workspace_catalog(WorkspaceCatalogRequest { after: None, limit })
                .await,
            Err(SessionRuntimeError::InvalidPageLimit)
        ));
    }
    let foreign = WorkspaceCatalogCursor {
        store_id: StoreId::generate().unwrap(),
        ..cursor
    };
    assert!(matches!(
        runtime
            .workspace_catalog(WorkspaceCatalogRequest {
                after: Some(foreign),
                limit: 2
            })
            .await,
        Err(SessionRuntimeError::CursorStoreMismatch)
    ));
    let store_id = runtime.store_id();
    runtime.shutdown().await.unwrap();
    drop(runtime);
    std::fs::remove_dir(directory.path().join("worker")).unwrap();
    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(database),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let second = reopened
        .workspace_catalog(WorkspaceCatalogRequest {
            after: Some(cursor),
            limit: 2,
        })
        .await
        .unwrap();
    assert_eq!(second.store_id, store_id);
    assert_eq!(second.workspaces, expected[2..]);
    assert_eq!(second.next, None);
    assert_eq!(
        reopened
            .workspace_catalog(WorkspaceCatalogRequest::default())
            .await
            .unwrap()
            .workspaces,
        expected
    );
    reopened.shutdown().await.unwrap();
}
