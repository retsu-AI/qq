//! Shared harness for the session tests: scripted loaders and providers,
//! runtime and store fixtures, and the event collectors. Tests live in the
//! theme modules below.

use std::sync::atomic::AtomicBool;

use super::*;

mod accounting;
mod approvals;
mod budgets;
mod commands;
mod compaction;
mod context_capacity;
mod contract;
mod delegation;
mod feeds;
mod migrations;
mod replay_identity;
mod runs;
mod settlement;
mod streaming;

use std::sync::Mutex as StdMutex;

use async_stream::stream as async_stream;

use futures_util::{StreamExt, stream};

use qq_protocol::{BudgetExhaustion, BudgetLimitKind};

use qq_provider::{ModelRequest, Provider, ProviderStream};

use tempfile::TempDir;

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

/// Compiles a test plan. Test loaders run inside the runtime's async
/// context, where the blocking compile is acceptable for the tiny
/// temporary workspaces tests use.
fn loaded_runtime_with_model(
    runtime: Runtime,
    workspace: &str,
    resolved: ResolvedModel,
) -> LoadedRuntime {
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
        Box::pin(async move {
            Runtime::new(ScriptedProvider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime,
                        &request.workspace,
                        Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: Some(100),
                            cache_write_usd_nanos_per_token: Some(300),
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ScriptedProvider;

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        // A compaction request ends with the summarization instruction;
        // answer it with a structurally valid summary so validation
        // passes, and everything else with "hello".
        let summarizing = request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"));
        if summarizing {
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: valid_summary("hello"),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 10,
                        cache_read_input_tokens: 2,
                        cache_write_input_tokens: 1,
                        output_tokens: 5,
                        reasoning_tokens: None,
                    }),
                }),
            ]));
        }
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "hel".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "l".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "o".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }),
            }),
        ]))
    }
}

struct MutableResolvedLoader {
    resolved_model: Arc<StdMutex<ResolvedModel>>,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for MutableResolvedLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let resolved_model = self.resolved_model.lock().unwrap().clone();
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(
                CapturedResolvedProvider { requests },
                resolved_model.provider_model.clone(),
                resolved_model.max_output_tokens,
            )
            .map(|runtime| runtime.with_context_window(resolved_model.context_window))
            .map(|runtime| {
                LoadedRuntime::compile_blocking_for_profile(
                    &runtime,
                    resolved_model,
                    PathBuf::from(&request.workspace),
                    request.profile,
                )
                .expect("test plan compiles")
            })
            .map_err(|error| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: error.to_string(),
            })
        })
    }
}

struct CapturedResolvedProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for CapturedResolvedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "done".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
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
                    Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }

            Runtime::new(CountingTextProvider(provider_calls), "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct PricedHangingLoader;

impl RuntimeLoader for PricedHangingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async move {
            Runtime::new(HangingProvider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime,
                        &request.workspace,
                        Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: Some(100),
                            cache_write_usd_nanos_per_token: Some(300),
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct UsageSequenceLoader {
    usages: StdMutex<Vec<Option<qq_provider::ProviderUsage>>>,
}

impl RuntimeLoader for UsageSequenceLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let usage = self.usages.lock().unwrap().remove(0);
        Box::pin(async move {
            Runtime::new(UsageSequenceProvider { usage }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct UsageSequenceProvider {
    usage: Option<qq_provider::ProviderUsage>,
}

impl Provider for UsageSequenceProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "answer".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: self.usage }),
        ]))
    }
}

struct ReasoningLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for ReasoningLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(ReasoningProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ReasoningProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for ReasoningProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let mut events = vec![Ok(qq_provider::ProviderEvent::ReasoningStarted {
            kind: qq_provider::ReasoningKind::Summary,
        })];
        events.extend((0..64).map(|_| {
            Ok(qq_provider::ProviderEvent::ReasoningDelta {
                kind: qq_provider::ReasoningKind::Summary,
                text: "private rationale ".to_owned(),
            })
        }));
        events.extend([
            Ok(qq_provider::ProviderEvent::ReasoningCompleted {
                kind: qq_provider::ReasoningKind::Summary,
            }),
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "ans".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "wer".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::ReasoningStarted {
                kind: qq_provider::ReasoningKind::ExposedThinking,
            }),
            Ok(qq_provider::ProviderEvent::ReasoningDelta {
                kind: qq_provider::ReasoningKind::ExposedThinking,
                text: "late rationale".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::ReasoningCompleted {
                kind: qq_provider::ReasoningKind::ExposedThinking,
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]);
        Box::pin(stream::iter(events))
    }
}

struct HangingReasoningLoader {
    buffered: Arc<tokio::sync::Notify>,
}

impl RuntimeLoader for HangingReasoningLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let buffered = Arc::clone(&self.buffered);
        Box::pin(async move {
            Runtime::new(HangingReasoningProvider { buffered }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct HangingReasoningProvider {
    buffered: Arc<tokio::sync::Notify>,
}

impl Provider for HangingReasoningProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let buffered = Arc::clone(&self.buffered);
        Box::pin(async_stream! {
            yield Ok(qq_provider::ProviderEvent::ReasoningStarted {
                kind: qq_provider::ReasoningKind::Summary,
            });
            yield Ok(qq_provider::ProviderEvent::ReasoningDelta {
                kind: qq_provider::ReasoningKind::Summary,
                text: "first".to_owned(),
            });
            yield Ok(qq_provider::ProviderEvent::ReasoningDelta {
                kind: qq_provider::ReasoningKind::Summary,
                text: "buffered".to_owned(),
            });
            buffered.notify_one();
            std::future::pending::<()>().await;
        })
    }
}

struct ChunkingLoader;

impl RuntimeLoader for ChunkingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async move {
            Runtime::new(ChunkingProvider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ChunkingProvider;

impl Provider for ChunkingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: String::new(),
            }),
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "é".repeat(MAX_TEXT_CHUNK_BYTES / 2 + 8),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct EmptyThenTextLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for EmptyThenTextLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(EmptyThenTextProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct EmptyThenTextProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for EmptyThenTextProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        if turn == 0 {
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: None,
            })]))
        } else {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "ok".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}

struct CapturingLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for CapturingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(DelayedProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct DelayedProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

struct ToolLoopLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for ToolLoopLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(ToolLoopProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ToolLoopProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for ToolLoopProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        if turn == 0 {
            Box::pin(stream::iter([
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
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 4,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 2,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 6,
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

struct RenewableSliceLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    checkpoint_wait: Option<Arc<tokio::sync::Notify>>,
    metered_empty_checkpoint: bool,
}

impl RuntimeLoader for RenewableSliceLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        let checkpoint_wait = self.checkpoint_wait.clone();
        let metered_empty_checkpoint = self.metered_empty_checkpoint;
        Box::pin(async move {
            Runtime::new(
                RenewableSliceProvider {
                    requests,
                    checkpoint_wait,
                    metered_empty_checkpoint,
                },
                "test-model",
                256,
            )
            .map(|runtime| {
                loaded_runtime(
                    runtime,
                    &request.workspace,
                    metered_empty_checkpoint.then_some(ModelPricing {
                        input_usd_nanos_per_token: 1_000,
                        output_usd_nanos_per_token: 2_000,
                        cache_read_usd_nanos_per_token: Some(100),
                        cache_write_usd_nanos_per_token: Some(300),
                        context_tier: None,
                        provenance: "test".to_owned(),
                    }),
                )
            })
            .map_err(|error| RuntimeLoadError {
                kind: RunFailureKind::Configuration,
                message: error.to_string(),
            })
        })
    }
}

struct RenewableSliceProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    checkpoint_wait: Option<Arc<tokio::sync::Notify>>,
    metered_empty_checkpoint: bool,
}

impl Provider for RenewableSliceProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let current = requests.len();
        requests.push(request.clone());
        drop(requests);

        let tool_turns = crate::MAX_TOOL_CALLS_PER_SLICE / crate::MAX_TOOL_CALLS_PER_TURN;
        if request.tools().is_empty() {
            if let Some(checkpoint_wait) = &self.checkpoint_wait {
                checkpoint_wait.notify_one();
                return Box::pin(stream::pending());
            }
            if self.metered_empty_checkpoint {
                return Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 3,
                        cache_read_input_tokens: 1,
                        cache_write_input_tokens: 2,
                        output_tokens: 5,
                        reasoning_tokens: None,
                    }),
                })]));
            }
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "slice checkpoint".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
        }
        if current > tool_turns {
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "task complete".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
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
            } else {
                ("read_file", r#"{"path":"note.txt"}"#)
            };
            events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: name.to_owned(),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: json.to_owned(),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(qq_provider::ProviderEvent::Completed {
            usage: self
                .metered_empty_checkpoint
                .then_some(qq_provider::ProviderUsage {
                    input_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 1,
                    reasoning_tokens: None,
                }),
        }));
        Box::pin(stream::iter(events))
    }
}

/// Turn one streams text and then requests a tool call; turn two streams
/// closing text. Exercises per-turn assistant messages around calls.
struct TurnTextLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for TurnTextLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(TurnTextProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
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
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}

struct RefusalProvider;

impl Provider for RefusalProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::RefusalDelta {
                text: "cannot complete that task".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

impl Provider for DelayedProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let summarizing = request_texts(&request)
            .last()
            .is_some_and(|text| text.starts_with("Summarize this conversation"));
        self.requests.lock().unwrap().push(request);
        Box::pin(async_stream! {
            tokio::time::sleep(Duration::from_millis(20)).await;
            yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: if summarizing { valid_summary("answer") } else { "answer".to_owned() },
            });
            yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
        })
    }
}

/// Requests `tool` once per tool turn, then completes with text.
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
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
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
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: format!("call_{turn}"),
                    name: self.tool.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: format!("call_{turn}"),
                    json: self.arguments.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: format!("call_{turn}"),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: self.usage }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: self.usage }),
            ]))
        }
    }
}

/// Replays a fixed tool-call script per run: run N issues its scripted
/// calls one per model turn, then completes with text.
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
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
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
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: valid_summary("done"),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
        }
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        match self.script.get(current) {
            Some((name, arguments)) => Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: format!("call_{current}"),
                    name: (*name).to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: format!("call_{current}"),
                    json: arguments.clone(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: format!("call_{current}"),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ])),
            None => Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ])),
        }
    }
}

struct ScriptedRunsHarness {
    _directory: TempDir,
    runtime: SessionRuntime,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    workspace_path: PathBuf,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    events: SessionEventStream,
}

async fn scripted_runs_harness(
    mode: ApprovalMode,
    runs: Vec<Vec<(&'static str, String)>>,
) -> ScriptedRunsHarness {
    scripted_runs_harness_with_authority(mode, runs, None).await
}

async fn scripted_runs_harness_with_authority(
    mode: ApprovalMode,
    runs: Vec<Vec<(&'static str, String)>>,
    grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
) -> ScriptedRunsHarness {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.grant_authority = grant_authority;
    let runtime = SessionRuntime::open(
        options,
        Arc::new(ScriptedRunsLoader {
            requests: Arc::clone(&requests),
            runs,
            loads: StdMutex::new(0),
        }),
    )
    .await
    .unwrap();
    let workspace_path = directory.path().to_owned();
    let (workspace_id, _) = resolve_workspace(&runtime, &workspace_path).await;
    let created = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: mode,
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    ScriptedRunsHarness {
        _directory: directory,
        runtime,
        requests,
        workspace_path,
        workspace_id,
        session_id,
        events,
    }
}

async fn submit_prompt(harness: &ScriptedRunsHarness, prompt: &str) -> RunId {
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text(prompt.to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    run_id
}

struct ApprovalHarness {
    _directory: TempDir,
    runtime: SessionRuntime,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    events: SessionEventStream,
}

async fn approval_harness(
    mode: ApprovalMode,
    tool: &'static str,
    arguments: &'static str,
    tool_turns: usize,
    approval_timeout: Duration,
) -> ApprovalHarness {
    approval_harness_with_authority(mode, tool, arguments, tool_turns, approval_timeout, None).await
}

async fn approval_harness_with_authority(
    mode: ApprovalMode,
    tool: &'static str,
    arguments: &'static str,
    tool_turns: usize,
    approval_timeout: Duration,
    grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
) -> ApprovalHarness {
    approval_harness_with_reviewer(
        mode,
        tool,
        arguments,
        tool_turns,
        approval_timeout,
        grant_authority,
        None,
    )
    .await
}

async fn approval_harness_with_reviewer(
    mode: ApprovalMode,
    tool: &'static str,
    arguments: &'static str,
    tool_turns: usize,
    approval_timeout: Duration,
    grant_authority: Option<Arc<dyn WorkspaceGrantAuthority>>,
    approval_reviewer: Option<Arc<dyn ApprovalReviewer>>,
) -> ApprovalHarness {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions {
            database_path: directory.path().join("sessions.sqlite3"),
            max_active_runs: 1,
            approval_timeout,
            grant_authority,
            approval_reviewer,
        },
        Arc::new(ApprovalLoader {
            requests: Arc::clone(&requests),
            tool,
            arguments,
            tool_turns,
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: mode,
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("mutate something".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    ApprovalHarness {
        _directory: directory,
        runtime,
        requests,
        workspace_id,
        session_id,
        run_id,
        events,
    }
}

async fn collect_until_approval_requested(
    events: &mut SessionEventStream,
) -> (Vec<SessionEventEnvelope>, ToolCallSnapshot) {
    tokio::time::timeout(Duration::from_secs(2), async {
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
    let directory = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    (directory, runtime)
}

/// Records the model each run is loaded with, then behaves like a
/// one-tool-turn approval run: the run parks at a `__test_mutate`
/// approval, which gives tests a deterministic "run is active" point.
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
            usage: Some(qq_provider::ProviderUsage {
                input_tokens: 7,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 1,
                reasoning_tokens: None,
            }),
        };
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| loaded_runtime_for_route(runtime, &request.workspace, route, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct SessionManagementHarness {
    directory: TempDir,
    runtime: SessionRuntime,
    models: Arc<StdMutex<Vec<Option<String>>>>,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    store_id: StoreId,
    events: SessionEventStream,
}

async fn session_management_harness() -> SessionManagementHarness {
    let directory = tempfile::tempdir().unwrap();
    let models = Arc::new(StdMutex::new(Vec::new()));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(RecordingApprovalLoader {
            models: Arc::clone(&models),
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    // These management tests park a run at its tool approval, so the
    // session must ask rather than auto-execute the scripted mutation.
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Ask).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    SessionManagementHarness {
        directory,
        runtime,
        models,
        requests,
        workspace_id,
        session_id,
        store_id: created.committed_through.store_id,
        events,
    }
}

/// Drives one `read_file` call through the store with a spill attached
/// and returns the store, the session, the call id, and the digest.
async fn store_with_one_spill(
    directory: &std::path::Path,
    text: &str,
) -> (Store, SessionId, ToolCallId, String) {
    let store = Store::open(directory.join("sessions.sqlite3"))
        .await
        .unwrap();
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: directory.to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let created = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let (tool_call_id, digest) = spill_one_call(&store, session_id, text, "first").await;
    (store, session_id, tool_call_id, digest)
}

/// Submits a prompt, claims the run, records one `read_file` call whose
/// result cites a spill of `text`, and finishes the run.
async fn spill_one_call(
    store: &Store,
    session_id: SessionId,
    text: &str,
    prompt: &str,
) -> (ToolCallId, String) {
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text(prompt.to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    let tool_call_id = ToolCallId::generate().unwrap();
    let call = RuntimeToolCall {
        id: tool_call_id,
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "call_0".to_owned(),
        name: "read_file".to_owned(),
        effect: crate::catalog::EffectClass::ReadOnly,
        arguments: r#"{"path":"big.txt"}"#.to_owned(),
        rejection: None,
    };
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolCall {
                        id: call.provider_call_id.clone(),
                        name: call.name.clone(),
                        arguments: serde_json::from_str(&call.arguments).unwrap(),
                    }],
                ),
                calls: vec![call],
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    store.start_tool_call(&claimed, tool_call_id).await.unwrap();
    let digest = crate::workspace::content_hash(text.as_bytes());
    let handle = format!(
        "t:read_file:{}:{}",
        &tool_call_id.to_string()[..8],
        &digest[..8]
    );
    store
        .finish_tool_call(
            &claimed,
            tool_call_id,
            format!("head\n…[qq: 9 bytes / 1 lines omitted; full output {handle}; read_tool_result offset=2]…\ntail\n"),
            false,
            Vec::new(),
            None,
            Some(crate::tools::SpillRecord {
                text: text.to_owned(),
                digest: digest.clone(),
                omitted_from_line: 2,
            }),
        )
        .await
        .unwrap();
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    (tool_call_id, digest)
}

fn rewrite_run_capacity_schema(path: &Path, base_declaration: &str, increment_declaration: &str) {
    let connection = Connection::open(path).unwrap();
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'runs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let schema = schema
        .replacen("context_base_bytes INTEGER", base_declaration, 1)
        .replacen(
            "context_increment_bytes INTEGER NOT NULL DEFAULT 0",
            increment_declaration,
            1,
        );
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             PRAGMA legacy_alter_table = ON;
             ALTER TABLE runs RENAME TO runs_valid;
             DROP TABLE runs_valid;",
        )
        .unwrap();
    connection.execute_batch(&schema).unwrap();
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct R4StreamMeasurement {
    bytes: usize,
    transactions: u64,
    elapsed_ns: u128,
    peak_temporary_rss_bytes: u64,
}

#[cfg(target_os = "linux")]
struct R4RssSampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<u64>>,
}

#[cfg(target_os = "linux")]
impl R4RssSampler {
    fn start() -> Self {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = std::sync::Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let mut peak = r4_current_rss_bytes();
            while !worker_stop.load(std::sync::atomic::Ordering::Acquire) {
                peak = peak.max(r4_current_rss_bytes());
                std::thread::sleep(Duration::from_millis(1));
            }
            peak.max(r4_current_rss_bytes())
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> u64 {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.worker.take().unwrap().join().unwrap()
    }
}

#[cfg(target_os = "linux")]
impl Drop for R4RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(target_os = "linux")]
fn r4_current_rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| {
            let kib = line.strip_prefix("VmRSS:")?.split_whitespace().next()?;
            kib.parse::<u64>().ok()
        })
        .unwrap()
        .saturating_mul(1024)
}

#[cfg(target_os = "linux")]
fn measure_r4_append_only_stream(bytes: usize) -> R4StreamMeasurement {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (mut connection, _) = open_database(&path).unwrap();
    let workspace_id = WorkspaceId::from_bytes([1; 16]);
    let session_id = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    let message_id = MessageId::from_bytes([4; 16]);
    connection
        .execute(
            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                 id, workspace_id, title, status, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'S', 'idle', 1, 1)",
            params![session_id.to_string(), workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(
                 id, session_id, command_id, user_message_id,
                 assistant_message_id, status, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?4, 'completed', 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                CommandId::from_bytes([5; 16]).to_string(),
                message_id.to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO messages(
                 id, session_id, run_id, ordinal, turn_ordinal, role, state,
                 output, refusal, created_at_ms
             ) VALUES (?1, ?2, ?3, 1, 1, 'assistant', 'complete', '', '', 1)",
            params![
                message_id.to_string(),
                session_id.to_string(),
                run_id.to_string(),
            ],
        )
        .unwrap();
    let baseline_rss = r4_current_rss_bytes();
    let sampler = R4RssSampler::start();
    let content = "x".repeat(bytes);
    let started = std::time::Instant::now();
    let mut transactions = 0_u64;
    for chunk in content.as_bytes().chunks(OUTPUT_BATCH_BYTES) {
        let transaction = connection.transaction().unwrap();
        insert_message_chunk(
            &transaction,
            message_id,
            TextChannel::Output,
            std::str::from_utf8(chunk).unwrap(),
        )
        .unwrap();
        transaction.commit().unwrap();
        transactions += 1;
    }
    let message = load_message(&connection, message_id).unwrap();
    let elapsed_ns = started.elapsed().as_nanos();
    let peak_temporary_rss_bytes = sampler.finish().saturating_sub(baseline_rss);
    assert_eq!(message.output, content);
    assert_eq!(
        transactions,
        u64::try_from(bytes.div_ceil(OUTPUT_BATCH_BYTES)).unwrap()
    );
    R4StreamMeasurement {
        bytes,
        transactions,
        elapsed_ns,
        peak_temporary_rss_bytes,
    }
}

#[cfg(target_os = "linux")]
fn parse_r4_stream_measurement(output: &[u8]) -> R4StreamMeasurement {
    let output = String::from_utf8_lossy(output);
    let line = output
        .lines()
        .find(|line| line.starts_with("r4_stream "))
        .unwrap_or_else(|| panic!("R4 child produced no measurement: {output}"));
    let mut fields = line.split_whitespace().skip(1).map(|field| {
        let (name, value) = field.split_once('=').unwrap();
        (name, value)
    });
    let bytes = fields.next().unwrap();
    let transactions = fields.next().unwrap();
    let elapsed = fields.next().unwrap();
    let rss = fields.next().unwrap();
    assert_eq!(bytes.0, "bytes");
    assert_eq!(transactions.0, "transactions");
    assert_eq!(elapsed.0, "elapsed_ns");
    assert_eq!(rss.0, "peak_temporary_rss_bytes");
    assert!(fields.next().is_none());
    R4StreamMeasurement {
        bytes: bytes.1.parse().unwrap(),
        transactions: transactions.1.parse().unwrap(),
        elapsed_ns: elapsed.1.parse().unwrap(),
        peak_temporary_rss_bytes: rss.1.parse().unwrap(),
    }
}

#[cfg(target_os = "linux")]
fn run_r4_stream_diagnostic_child(
    executable: &Path,
    bytes: usize,
    child_bytes: &str,
) -> std::process::Output {
    let mut child = std::process::Command::new(executable)
        .args([
            "--exact",
            "sessions::tests::r4_append_only_chunk_scaling_diagnostic",
            "--ignored",
            "--nocapture",
        ])
        .env(child_bytes, bytes.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "R4 child timed out: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy)]
enum DenialCapacityPath {
    Policy,
    Client,
    Timeout,
}

impl DenialCapacityPath {
    const fn initial_state(self) -> &'static str {
        match self {
            Self::Policy => "requested",
            Self::Client | Self::Timeout => "awaiting_approval",
        }
    }

    const fn result(self) -> &'static str {
        match self {
            Self::Policy => approval::POLICY_DENIED_RESULT,
            Self::Client => approval::USER_DENIED_RESULT,
            Self::Timeout => approval::TIMEOUT_DENIED_RESULT,
        }
    }
}

fn denial_capacity_fixture(
    path: DenialCapacityPath,
    context_base_bytes: usize,
) -> (TempDir, Connection, StoreId, ClaimedRun, ToolCallId, String) {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let (connection, store_id) = open_database(&database_path).unwrap();
    let workspace_id = WorkspaceId::from_bytes([1; 16]);
    let session_id = SessionId::from_bytes([2; 16]);
    let run_id = RunId::from_bytes([3; 16]);
    let command_id = CommandId::from_bytes([4; 16]);
    let tool_call_id = ToolCallId::from_bytes([5; 16]);
    let provider_call_id = "provider-call".to_owned();
    connection
        .execute(
            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, '/w', 0)",
            [workspace_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, title, status, active_run_id,
                                  created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'S', 'running', ?3, 1, 1)",
            params![
                session_id.to_string(),
                workspace_id.to_string(),
                run_id.to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs(id, session_id, command_id, user_message_id,
                              assistant_message_id, status, context_base_bytes,
                              context_increment_bytes, created_at_ms, started_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, 0, 1, 1)",
            params![
                run_id.to_string(),
                session_id.to_string(),
                command_id.to_string(),
                MessageId::from_bytes([6; 16]).to_string(),
                MessageId::from_bytes([7; 16]).to_string(),
                context_base_bytes,
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tool_calls(
                 id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                 arguments_json, state, requested_at_ms
             ) VALUES (?1, ?2, 1, 1, ?3, 'shell', '{}', ?4, 1)",
            params![
                tool_call_id.to_string(),
                run_id.to_string(),
                provider_call_id,
                path.initial_state(),
            ],
        )
        .unwrap();
    let claimed = ClaimedRun {
        identity: RunIdentity {
            workspace_id,
            session_id,
            run_id,
            command_id,
            kind: RunKind::Prompt,
            child: false,
        },
        workspace: "/w".to_owned(),
        user_initiated: true,
        literal_slash: false,
        session_model: ModelSelection::default(),
        model: ModelSelection::default(),
        messages: Vec::new(),
        context_compaction_attempted: false,
        context_overflow_basis: None,
        context_occupancy: None,
        limits: RunLimits::default(),
        input: Vec::new(),
        profile: AgentProfileId::default(),
        approval_mode: ApprovalMode::default(),
        depth: 0,
        root_run_id: run_id,
        purpose: SessionPurpose::Task,
        cancel_requested: false,
        file_state: Vec::new(),
        pending_steering: Vec::new(),
        output: None,
    };
    (
        directory,
        connection,
        store_id,
        claimed,
        tool_call_id,
        provider_call_id,
    )
}

fn apply_denial_capacity_path(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    path: DenialCapacityPath,
) -> Result<(), SessionRuntimeError> {
    match path {
        DenialCapacityPath::Policy => deny_tool_call(
            connection,
            store_id,
            claimed.identity,
            tool_call_id,
            approval::POLICY_DENIED_RESULT,
        )
        .map(|_| ()),
        DenialCapacityPath::Client => execute_command(
            connection,
            store_id,
            CommandId::from_bytes([8; 16]),
            SessionCommand::RespondToolApproval {
                run_id: claimed.identity.run_id,
                tool_call_id,
                decision: ApprovalDecision::Deny,
            },
            None,
            &WorkspaceGrantSeed::default(),
        )
        .map(|_| ()),
        DenialCapacityPath::Timeout => {
            conclude_tool_approval(connection, store_id, claimed.identity, tool_call_id, true)
                .map(|_| ())
        }
    }
}

fn denial_capacity_state(
    connection: &Connection,
    run_id: RunId,
    tool_call_id: ToolCallId,
) -> (u64, String, Option<String>, u64, u64) {
    let increment = connection
        .query_row(
            "SELECT context_increment_bytes FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let (state, result) = connection
        .query_row(
            "SELECT state, result FROM tool_calls WHERE id = ?1",
            [tool_call_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let events = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    let commands = connection
        .query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))
        .unwrap();
    (increment, state, result, events, commands)
}

async fn claimed_store_fixture() -> (TempDir, Store, ClaimedRun) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: directory.path().to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let created = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("x".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    (directory, store, claimed)
}

async fn streaming_transaction_state(store: &Store, run_id: RunId) -> (u64, u64, u64, u64, u64) {
    store
        .call(Priority::Control, move |connection| {
            let increment = connection.query_row(
                "SELECT context_increment_bytes FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let assistant_messages = connection.query_row(
                "SELECT COUNT(*) FROM messages
                     WHERE run_id = ?1 AND role = 'assistant'",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let chunks = connection.query_row(
                "SELECT COUNT(*) FROM message_chunks c
                     JOIN messages m ON m.id = c.message_id WHERE m.run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let turns = connection.query_row(
                "SELECT COUNT(*) FROM model_turns WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let events =
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            Ok((increment, assistant_messages, chunks, turns, events))
        })
        .await
        .unwrap()
}

/// Blocks the store worker on one job and fills every control-lane slot
/// behind it. The returned guard owns the queued filler calls; releasing
/// the worker drains them. Fresh `Priority::Control` calls are refused
/// while the guard is held.
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
    let blocked_store = runtime.inner.store.clone();
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
    // Other work may already hold control slots; fill whatever remains.
    let mut fillers = Vec::new();
    for _ in 0..store::CONTROL_QUEUE_CAPACITY {
        let store = runtime.inner.store.clone();
        let mut call = Box::pin(async move { store.call(Priority::Control, |_| Ok(())).await });
        // Polling once takes the permit and enqueues the job synchronously,
        // or is refused because the lane is already full.
        match futures_util::poll!(call.as_mut()) {
            std::task::Poll::Pending => fillers.push(tokio::spawn(call)),
            std::task::Poll::Ready(Err(SessionRuntimeError::Overloaded)) => break,
            std::task::Poll::Ready(other) => panic!("unexpected filler result: {other:?}"),
        }
    }
    assert!(matches!(
        runtime
            .inner
            .store
            .call(Priority::Control, |_| Ok(()))
            .await,
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
            SessionCommand::CreateSession {
                workspace_id,
                parent_id,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode,
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap()
}

/// The pre-D7 assembly, one query per message and per turn, kept as the
/// oracle for the joined loader. Any divergence between the two on a
/// seeded store is a regression in the joined query.
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
        // SQLite integers are i64; `u64::MAX` means "everything".
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

        let mut context = Vec::new();
        if let Some(compaction) = compaction {
            context.push(Message::user(format!(
                "{COMPACTION_SUMMARY_PREAMBLE}\n\n{}",
                compaction.summary
            )));
        }
        for id in message_ids {
            let snapshot = load_message(transaction, parse_id(&id)?)?;
            if snapshot.role != MessageRole::User {
                return Err(SessionRuntimeError::CODEC);
            }
            context.push(Message::user(snapshot.output));
            // Reconstruct each run immediately after its prompt rather than
            // following message-row ordinals. Follow-up prompts can be queued
            // while the prior run is active, so its later committed output still
            // belongs before the follow-up in model context.
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
                    reference_append_run_turns(transaction, snapshot.run_id, &mut context)?;
                } else {
                    reference_append_legacy_run_messages(
                        transaction,
                        snapshot.run_id,
                        &mut context,
                    )?;
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
        }
        // The reference predates the stored effect column and prunes by
        // name alone; the differential fixtures use built-in tools only.
        let context_rewritten = prune_stale_tool_results(&mut context, &HashMap::new());
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
    fn reference_append_run_turns(
        transaction: &Connection,
        run_id: RunId,
        context: &mut Vec<Message>,
    ) -> Result<(), SessionRuntimeError> {
        let mut statement = transaction.prepare(
            "SELECT turn_ordinal, assistant_content_json, truncated FROM model_turns
                 WHERE run_id = ?1 ORDER BY turn_ordinal",
        )?;
        let turns = statement
            .query_map([run_id.to_string()], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        // Applied steering carries the ordinal of the turn whose request first
        // included it; it is replayed as a user message immediately before that
        // turn, after the preceding turn's tool results.
        let mut statement = transaction.prepare(
            "SELECT turn_ordinal, output FROM messages
                 WHERE run_id = ?1 AND steering = 1 AND state = 'complete'
                 ORDER BY turn_ordinal, ordinal",
        )?;
        let mut steering = statement
            .query_map([run_id.to_string()], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<std::collections::VecDeque<_>, _>>()?;
        drop(statement);
        for (turn_ordinal, content_json, truncated) in turns {
            while steering
                .front()
                .is_some_and(|(applied_before, _)| *applied_before <= turn_ordinal)
            {
                let (_, text) = steering.pop_front().expect("front was just checked");
                context.push(Message::user(text));
            }
            let content: Vec<ContentBlock> =
                serde_json::from_str::<Vec<PersistedContentBlock>>(&content_json)?
                    .into_iter()
                    .map(ContentBlock::from)
                    .collect();

            let mut statement = transaction.prepare(
                "SELECT provider_call_id, result, is_error FROM tool_calls
                     WHERE run_id = ?1 AND turn_ordinal = ?2 AND result IS NOT NULL
                     ORDER BY call_ordinal",
            )?;
            let mut recorded = statement
                .query_map(params![run_id.to_string(), turn_ordinal], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        (row.get::<_, String>(1)?, row.get::<_, bool>(2)?),
                    ))
                })?
                .collect::<Result<HashMap<String, (String, bool)>, _>>()?;
            drop(statement);
            // Emit exactly one result per ToolCall block, in block order.
            // A block without a recorded result (a crash between the
            // turn commit and its tool_calls rows in an older store)
            // gets an explicit interrupted result so replayed context
            // stays provider-valid instead of poisoning the session.
            let results = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. } => Some(match recorded.remove(id) {
                        Some((content, is_error)) => ContentBlock::ToolResult {
                            call_id: id.clone(),
                            content,
                            is_error,
                        },
                        None => ContentBlock::ToolResult {
                            call_id: id.clone(),
                            content: INTERRUPTED_TOOL_RESULT.to_owned(),
                            is_error: true,
                        },
                    }),
                    ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
                })
                .collect::<Vec<_>>();
            context.push(Message::new(Role::Assistant, content));
            if !results.is_empty() {
                context.push(Message::tool_results(results));
            }
            // A truncated turn was followed in the live run by the continuation
            // notice; replaying it keeps the assembled context identical to the
            // request the model actually saw (and preserves role alternation).
            if truncated {
                context.push(Message::user(crate::OUTPUT_TRUNCATED_CONTINUE_NOTICE));
            }
        }
        // Steering applied for a turn that never committed (the run settled
        // first) still reached the model's request; keep it so the transcript
        // the user saw is the transcript the next run continues from.
        for (_, text) in steering {
            context.push(Message::user(text));
        }
        Ok(())
    }
}

/// Asserts the joined context loader assembles exactly what the reference
/// (per-message, per-turn) loader assembles for `session_id`.
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
    // A bounded prefix must agree too: `through_ordinal` scoping is part
    // of the contract the reload path depends on.
    let (joined, _) = load_model_context_with_rewrite_status(&transaction, session_id, 1).unwrap();
    let (reference, _) =
        reference_assembly::reference_load_model_context(&transaction, session_id, 1).unwrap();
    assert_eq!(joined.len(), reference.len(), "bounded message count");
}

async fn collect_through_finished(events: &mut SessionEventStream) -> Vec<SessionEventEnvelope> {
    // Generous upper bound: the auto-compaction tests stream multi-MiB
    // outputs concurrently, which can starve lighter tests of CPU.
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

/// Collects events until the compaction commits (`SessionCompacted`),
/// which is published after the internal run's `RunFinished`.
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

/// A structurally valid summarizer reply carrying `body` under every
/// required section, so validation passes and tests can still grep for it.
fn valid_summary(body: &str) -> String {
    format!(
        "1. Intent: {body}\n2. Decisions and constraints: {body}\n3. Work state: {body}\n\
         4. Files touched: {body}\n5. Errors: {body}\n6. User messages: {body}"
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

/// The concatenated text of each message in a captured provider request.
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
                ContentBlock::Text { .. } => {}
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

/// One scripted model load for the auto-compaction tests: what the
/// provider streams for that run.
#[derive(Clone)]
enum AutoCompactScript {
    /// Streams the text and completes.
    Text(String),
    /// Reads `note.txt` on the first turn, then streams the text: seeds a
    /// prunable read-only result into the transcript.
    ReadNoteThenText(String),
    /// Calls `search_history` with the query on the first turn, then
    /// streams the text.
    SearchHistoryThenText(String, String),
    /// Runs `shell` with the command on the first turn; on the next turn
    /// calls `read_tool_result` with the `t:` handle found in that
    /// result's marker and the given JSON arguments, then streams the
    /// text. The handle is discovered the way a model would.
    ShellThenRecall {
        command: String,
        recall: serde_json::Value,
        text: String,
    },
    /// Fails the model stream with a transport error.
    Fail,
    /// Fails the model stream with a context-window overflow.
    ContextOverflow,
    /// Never yields: the run parks until cancelled.
    Stall,
    /// Panics synchronously when the provider stream is created.
    Panic,
}

struct AutoCompactLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    scripts: Vec<AutoCompactScript>,
    loads: StdMutex<usize>,
    context_window: Option<u32>,
    max_output_tokens: u32,
    /// Whether the resolved model carries a secret-free provider identity.
    /// Custom/LiteLLM deployments and dynamic AWS region chains do not.
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
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct AutoCompactProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: AutoCompactScript,
}

impl Provider for AutoCompactProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        // The turn following this run's own tool call ends with the
        // tool result; earlier runs' results sit before the new prompt.
        let already_read = request.messages().last().is_some_and(|message| {
            message
                .content()
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        // Every tool result so far, oldest first, for scripts that
        // discover a handle the way a model would.
        let prior_results: Vec<String> = request
            .messages()
            .iter()
            .flat_map(|message| message.content().iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        self.requests.lock().unwrap().push(request);
        match &self.script {
            AutoCompactScript::Text(text) => Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ])),
            AutoCompactScript::ReadNoteThenText(text) => {
                if already_read {
                    Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::ToolCallStarted {
                            id: "call_read".to_owned(),
                            name: "read_file".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_read".to_owned(),
                            json: r#"{"path":"note.txt"}"#.to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                            id: "call_read".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
            AutoCompactScript::SearchHistoryThenText(query, text) => {
                if already_read {
                    Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::ToolCallStarted {
                            id: "call_history".to_owned(),
                            name: crate::runtime::SEARCH_HISTORY_TOOL.to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_history".to_owned(),
                            json: serde_json::json!({ "query": query, "limit": 2 }).to_string(),
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                            id: "call_history".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
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
                    (0, _) => Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::ToolCallStarted {
                            id: "call_shell".to_owned(),
                            name: "shell".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_shell".to_owned(),
                            json: serde_json::json!({ "command": command }).to_string(),
                        }),
                        Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                            id: "call_shell".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ])),
                    (1, Some(handle)) => {
                        let mut arguments = recall.clone();
                        arguments["handle"] = serde_json::Value::String(handle);
                        Box::pin(stream::iter([
                            Ok(qq_provider::ProviderEvent::ToolCallStarted {
                                id: "call_recall".to_owned(),
                                name: "read_tool_result".to_owned(),
                            }),
                            Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                                id: "call_recall".to_owned(),
                                json: arguments.to_string(),
                            }),
                            Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                                id: "call_recall".to_owned(),
                            }),
                            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                        ]))
                    }
                    _ => Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.clone() }),
                        Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                    ])),
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
        }
    }
}

struct AutoCompactHarness {
    _directory: TempDir,
    runtime: SessionRuntime,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    workspace_path: PathBuf,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    events: SessionEventStream,
}

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
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::clone(&loader.requests);
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(loader),
    )
    .await
    .unwrap();
    let workspace_path = directory.path().to_owned();
    let (workspace_id, _) = resolve_workspace(&runtime, &workspace_path).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, approval_mode).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    AutoCompactHarness {
        _directory: directory,
        runtime,
        requests,
        workspace_path,
        workspace_id,
        session_id,
        events,
    }
}

async fn queue_prompt(runtime: &SessionRuntime, session_id: SessionId, prompt: String) -> RunId {
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text(prompt)],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    run_id
}

/// Collects events until `stop` matches (inclusive), with a generous
/// timeout: the auto-compaction tests stream multi-MiB outputs.
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

/// A large prior answer that still fits its originating run, while a
/// maximum-sized queued prompt pushes the next request past the storage
/// backstop. The compaction request omits that queued prompt and fits.
fn over_threshold_output() -> String {
    "x".repeat(MAX_CONTEXT_BYTES - 100 * 1024)
}

async fn project_terminal_run_with_tool_boundaries(
    outcome: RunOutcome,
) -> (Vec<Message>, Vec<Message>) {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: directory.path().to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let created = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("inspect the tool boundaries".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    let completed_call_id = ToolCallId::generate().unwrap();
    let started_call_id = ToolCallId::generate().unwrap();
    let awaiting_call_id = ToolCallId::generate().unwrap();
    let untouched_call_id = ToolCallId::generate().unwrap();
    let calls = vec![
        RuntimeToolCall {
            id: completed_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "completed-call".to_owned(),
            name: "read_file".to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            arguments: r#"{"path":"completed.txt"}"#.to_owned(),
            rejection: None,
        },
        RuntimeToolCall {
            id: started_call_id,
            turn_ordinal: 1,
            call_ordinal: 2,
            provider_call_id: "started-call".to_owned(),
            name: "read_file".to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            arguments: r#"{"path":"first.txt"}"#.to_owned(),
            rejection: None,
        },
        RuntimeToolCall {
            id: awaiting_call_id,
            turn_ordinal: 1,
            call_ordinal: 3,
            provider_call_id: "awaiting-call".to_owned(),
            name: "shell".to_owned(),
            effect: crate::catalog::EffectClass::Shell,
            arguments: r#"{"command":"true"}"#.to_owned(),
            rejection: None,
        },
        RuntimeToolCall {
            id: untouched_call_id,
            turn_ordinal: 1,
            call_ordinal: 4,
            provider_call_id: "untouched-call".to_owned(),
            name: "read_file".to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            arguments: r#"{"path":"second.txt"}"#.to_owned(),
            rejection: None,
        },
    ];
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    calls
                        .iter()
                        .map(|call| {
                            ContentBlock::tool_call(
                                call.provider_call_id.clone(),
                                call.name.clone(),
                                &serde_json::from_str::<serde_json::Value>(&call.arguments)
                                    .unwrap(),
                            )
                        })
                        .collect(),
                ),
                calls,
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
            },
        )
        .await
        .unwrap();
    store
        .start_tool_call(&claimed, completed_call_id)
        .await
        .unwrap();
    store
        .finish_tool_call(
            &claimed,
            completed_call_id,
            "persisted result".to_owned(),
            false,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
    store
        .start_tool_call(&claimed, started_call_id)
        .await
        .unwrap();
    store
        .request_tool_approval(&claimed, awaiting_call_id, ApprovalPreviews::default())
        .await
        .unwrap();
    let finished = store
        .finish_run(&claimed, outcome, None, TeardownComplete::nothing_ran())
        .await
        .unwrap();
    let after = finished.last().unwrap().cursor;
    store.close().await.unwrap();
    drop(store);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(connection);
    let restarted_database_path = directory.path().join("restarted-sessions.sqlite3");
    std::fs::copy(&database_path, &restarted_database_path).unwrap();

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue safely".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let _ = collect_through_finished(&mut events).await;

    let projected_before_restart = {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        requests[0].messages().to_vec()
    };
    drop(runtime);

    let restarted_requests = Arc::new(StdMutex::new(Vec::new()));
    let restarted = SessionRuntime::open(
        SessionRuntimeOptions::new(restarted_database_path),
        Arc::new(CapturingLoader {
            requests: Arc::clone(&restarted_requests),
        }),
    )
    .await
    .unwrap();
    let mut restarted_events = restarted
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    restarted
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("continue safely".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let _ = collect_through_finished(&mut restarted_events).await;
    let restarted_requests = restarted_requests.lock().unwrap();
    assert_eq!(restarted_requests.len(), 1);
    (
        projected_before_restart,
        restarted_requests[0].messages().to_vec(),
    )
}

/// Truncates the first turn of every run at the output limit, then
/// completes on the second turn.
struct TruncatingLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl RuntimeLoader for TruncatingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            Runtime::new(TruncatingProvider { requests }, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct TruncatingProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for TruncatingProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request);
        drop(requests);
        let usage = Some(qq_provider::ProviderUsage {
            input_tokens: 4,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 2,
            reasoning_tokens: None,
        });
        // Even-numbered requests (the first of each run) truncate.
        if turn.is_multiple_of(2) {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "first half".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Incomplete {
                    usage,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: " second half".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage }),
            ]))
        }
    }
}

struct ContextBudgetLoader;

impl RuntimeLoader for ContextBudgetLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        Box::pin(async move {
            Runtime::new(ContextBudgetProvider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ContextBudgetProvider;

impl Provider for ContextBudgetProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "x".repeat(MAX_CONTEXT_BYTES + 1),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

/// Scripted grant authority: hands every session a fixed seed and
/// answers promotions with a fixed outcome, recording every request.
struct ScriptedGrantAuthority {
    seed: WorkspaceGrantSeed,
    outcome: WorkspaceGrantOutcome,
    seeded: StdMutex<Vec<PathBuf>>,
    promotions: StdMutex<Vec<(PathBuf, ApprovalGrant)>>,
}

impl ScriptedGrantAuthority {
    fn new(seed: WorkspaceGrantSeed, outcome: WorkspaceGrantOutcome) -> Arc<Self> {
        Arc::new(Self {
            seed,
            outcome,
            seeded: StdMutex::new(Vec::new()),
            promotions: StdMutex::new(Vec::new()),
        })
    }
}

impl WorkspaceGrantAuthority for ScriptedGrantAuthority {
    fn seed_grants(&self, workspace: &Path) -> GrantSeedFuture {
        self.seeded.lock().unwrap().push(workspace.to_owned());
        Box::pin(std::future::ready(self.seed.clone()))
    }

    fn promote_grant(&self, workspace: &Path, grant: &ApprovalGrant) -> GrantPromotionFuture {
        self.promotions
            .lock()
            .unwrap()
            .push((workspace.to_owned(), grant.clone()));
        Box::pin(std::future::ready(self.outcome.clone()))
    }
}

struct BlockingGrantAuthority {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl WorkspaceGrantAuthority for BlockingGrantAuthority {
    fn seed_grants(&self, _workspace: &Path) -> GrantSeedFuture {
        Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
    }

    fn promote_grant(&self, _workspace: &Path, _grant: &ApprovalGrant) -> GrantPromotionFuture {
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            WorkspaceGrantOutcome::Written {
                path: "/w/.qq/config.ron".to_owned(),
            }
        })
    }
}

struct ObservedBlockingGrantAuthority {
    entered: mpsc::UnboundedSender<ApprovalGrant>,
    release: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl WorkspaceGrantAuthority for ObservedBlockingGrantAuthority {
    fn seed_grants(&self, _workspace: &Path) -> GrantSeedFuture {
        Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
    }

    fn promote_grant(&self, _workspace: &Path, grant: &ApprovalGrant) -> GrantPromotionFuture {
        let _ = self.entered.send(grant.clone());
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        let release = Arc::clone(&self.release);
        let active = Arc::clone(&self.active);
        Box::pin(async move {
            release.acquire().await.unwrap().forget();
            active.fetch_sub(1, Ordering::AcqRel);
            WorkspaceGrantOutcome::Written {
                path: "/w/.qq/config.ron".to_owned(),
            }
        })
    }
}

struct PanickingGrantAuthority;

impl WorkspaceGrantAuthority for PanickingGrantAuthority {
    fn seed_grants(&self, _workspace: &Path) -> GrantSeedFuture {
        Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
    }

    fn promote_grant(&self, _workspace: &Path, _grant: &ApprovalGrant) -> GrantPromotionFuture {
        Box::pin(async { panic!("injected workspace grant authority panic") })
    }
}

/// The `workspace_grant_promoted` event for a responded approval. It is
/// published by a background task, so it may land before or after the
/// run's terminal event: check what was already collected, then poll.
async fn grant_promotion_event(
    observed: &[SessionEventEnvelope],
    events: &mut SessionEventStream,
) -> SessionEventEnvelope {
    if let Some(event) = observed
        .iter()
        .find(|event| matches!(event.event, SessionEvent::WorkspaceGrantPromoted { .. }))
    {
        return event.clone();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if matches!(event.event, SessionEvent::WorkspaceGrantPromoted { .. }) {
                return event;
            }
        }
    })
    .await
    .unwrap()
}

/// A reviewer whose verdict is released by the test: `hold` starts
/// occupied, and dropping or sending on the release channel lets the
/// verdict return. Consultations are counted for never-consulted cases.
struct StubReviewer {
    verdict: ReviewVerdict,
    release: StdMutex<Option<oneshot::Receiver<()>>>,
    consulted: Arc<StdMutex<Vec<ReviewRequest>>>,
}

impl StubReviewer {
    fn immediate(verdict: ReviewVerdict) -> (Arc<Self>, Arc<StdMutex<Vec<ReviewRequest>>>) {
        let consulted = Arc::new(StdMutex::new(Vec::new()));
        (
            Arc::new(Self {
                verdict,
                release: StdMutex::new(None),
                consulted: Arc::clone(&consulted),
            }),
            consulted,
        )
    }

    fn held(verdict: ReviewVerdict) -> (Arc<Self>, oneshot::Sender<()>) {
        let (sender, receiver) = oneshot::channel();
        (
            Arc::new(Self {
                verdict,
                release: StdMutex::new(Some(receiver)),
                consulted: Arc::new(StdMutex::new(Vec::new())),
            }),
            sender,
        )
    }
}

impl ApprovalReviewer for StubReviewer {
    fn review(&self, request: ReviewRequest) -> ReviewFuture {
        self.consulted.lock().unwrap().push(request);
        let release = self.release.lock().unwrap().take();
        let verdict = self.verdict.clone();
        Box::pin(async move {
            if let Some(release) = release {
                let _ = release.await;
            }
            verdict
        })
    }
}

/// Like `collect_through_finished`, with a deadline generous enough for
/// tests that spawn real child processes.
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

/// Loads providers for spawn tests: model-routed entries first (children
/// given an explicit model route), then a per-load queue (deterministic
/// for the single-parent tests: the parent always loads first), then a
/// fallback that completes with "done".
struct QueueLoader {
    routed: Vec<(&'static str, Arc<dyn Provider>)>,
    queue: StdMutex<Vec<Arc<dyn Provider>>>,
}

impl QueueLoader {
    /// The routed provider for the request's model, else the next queued
    /// one, else static text.
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
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ResolvingLoader {
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    worker: Option<ModelSelection>,
    resolutions: Arc<AtomicUsize>,
    loads: Arc<StdMutex<Vec<ModelSelection>>>,
}

impl RuntimeLoader for ResolvingLoader {
    fn resolve_worker_model(
        &self,
        _workspace: String,
        parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        self.resolutions.fetch_add(1, Ordering::AcqRel);
        let selection = self.worker.clone().unwrap_or(parent);
        Box::pin(std::future::ready(Ok(selection)))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.loads.lock().unwrap().push(request.model.clone());
        let mut spawn_model_routes = vec!["test/explicit".to_owned()];
        if let Some(worker) = self.worker.as_ref().and_then(|worker| worker.model.clone()) {
            spawn_model_routes.push(worker);
        }
        let provider = if request.model.model.as_deref() == Some("test/model") {
            Arc::clone(&self.parent)
        } else {
            Arc::clone(&self.child)
        };
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(spawn_model_routes),
                        &request.workspace,
                        None,
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct RejectingWorkerLoader {
    parent: Arc<dyn Provider>,
}

impl RuntimeLoader for RejectingWorkerLoader {
    fn resolve_worker_model(
        &self,
        _workspace: String,
        _parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        Box::pin(std::future::ready(Err(RuntimeLoadError {
            kind: RunFailureKind::Policy,
            message: "configured worker route is denied".to_owned(),
        })))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = Arc::clone(&self.parent);
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

/// Records every spawn-time validation and either accepts or rejects
/// it; loads route the parent model to `parent` and everything else to
/// `child`.
struct ValidatingLoader {
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    worker: Option<ModelSelection>,
    rejection: Option<RuntimeLoadError>,
    validations: Arc<StdMutex<Vec<ModelSelection>>>,
    loads: Arc<StdMutex<Vec<ModelSelection>>>,
}

impl RuntimeLoader for ValidatingLoader {
    fn resolve_worker_model(
        &self,
        _workspace: String,
        parent: ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        let selection = self.worker.clone().unwrap_or(parent);
        Box::pin(std::future::ready(Ok(selection)))
    }

    fn validate_spawn_model(
        &self,
        _workspace: String,
        selection: ModelSelection,
    ) -> SpawnModelValidationFuture {
        self.validations.lock().unwrap().push(selection);
        let result = match &self.rejection {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        };
        Box::pin(std::future::ready(result))
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        self.loads.lock().unwrap().push(request.model.clone());
        let provider = if request.model.model.as_deref() == Some("test/model") {
            Arc::clone(&self.parent)
        } else {
            Arc::clone(&self.child)
        };
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

/// Completes immediately with the text "done".
struct StaticTextProvider;

impl Provider for StaticTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "done".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct AccountingTextProvider {
    usage: qq_provider::ProviderUsage,
}

impl Provider for AccountingTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "done".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(self.usage),
            }),
        ]))
    }
}

struct AccountingSpawnProvider {
    turn: StdMutex<usize>,
}

impl Provider for AccountingSpawnProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let mut turn = self.turn.lock().unwrap();
        let current = *turn;
        *turn += 1;
        drop(turn);
        if current == 0 {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "first".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "first".to_owned(),
                    json: r#"{"task":"first","model":"test/child-first"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "first".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "second".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "second".to_owned(),
                    json: r#"{"task":"second","model":"test/child-second"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "second".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 3,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 5,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 7,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        }
    }
}

struct AccountingSpawnLoader;

impl RuntimeLoader for AccountingSpawnLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider: Arc<dyn Provider> = match request.model.model.as_deref() {
            Some("test/model") => Arc::new(AccountingSpawnProvider {
                turn: StdMutex::new(0),
            }),
            Some("test/child-first") => Arc::new(AccountingTextProvider {
                usage: qq_provider::ProviderUsage {
                    input_tokens: 11,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 13,
                    reasoning_tokens: None,
                },
            }),
            Some("test/child-second") => Arc::new(AccountingTextProvider {
                usage: qq_provider::ProviderUsage {
                    input_tokens: 17,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 19,
                    reasoning_tokens: None,
                },
            }),
            other => panic!("unexpected accounting test model: {other:?}"),
        };
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime.with_spawn_model_routes(vec![
                            "test/child-first".to_owned(),
                            "test/child-second".to_owned(),
                        ]),
                        &request.workspace,
                        Some(ModelPricing {
                            input_usd_nanos_per_token: 1,
                            output_usd_nanos_per_token: 1,
                            cache_read_usd_nanos_per_token: Some(1),
                            cache_write_usd_nanos_per_token: Some(1),
                            context_tier: None,
                            provenance: "accounting-test".to_owned(),
                        }),
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

/// Fails every stream with a transport error.
struct FailingProvider;

impl Provider for FailingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::once(async {
            Err(qq_provider::ProviderError::Transport("offline".to_owned()))
        }))
    }
}

/// Never yields: the run only ends by cancellation.
struct HangingProvider;

impl Provider for HangingProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        Box::pin(stream::pending())
    }
}

struct SpawnThenHangProvider {
    turn: AtomicUsize,
    second_turn_started: Arc<tokio::sync::Notify>,
}

impl Provider for SpawnThenHangProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        if self.turn.fetch_add(1, Ordering::AcqRel) == 0 {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "spawn".to_owned(),
                    name: "spawn_agent".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "spawn".to_owned(),
                    json: r#"{"task":"finish first","model":"test/child"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "spawn".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        } else {
            self.second_turn_started.notify_one();
            Box::pin(stream::pending())
        }
    }
}

/// Tracks how many of its streams are concurrently active before
/// completing with text, to observe the child concurrency cap.
struct GaugedTextProvider {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Provider for GaugedTextProvider {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let active = Arc::clone(&self.active);
        let peak = Arc::clone(&self.peak);
        Box::pin(async_stream! {
            let current = active.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(current, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(50)).await;
            active.fetch_sub(1, Ordering::AcqRel);
            yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "child done".to_owned(),
            });
            yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
        })
    }
}

/// Requests `spawns` spawn_agent calls in one turn, then completes with
/// "done" on the next.
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
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
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
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::default(),
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = root.receipt.outcome else {
        panic!("unexpected receipt")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("delegate work".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claimed = store.claim_next_run(false).await.unwrap().unwrap();
    (workspace_id, session_id, claimed)
}

struct SpawnHarness {
    _directory: TempDir,
    runtime: SessionRuntime,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    events: SessionEventStream,
}

async fn spawn_harness_with_loader(
    loader: Arc<dyn RuntimeLoader>,
    max_active_runs: usize,
) -> SpawnHarness {
    let directory = tempfile::tempdir().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = max_active_runs;
    let runtime = SessionRuntime::open(options, loader).await.unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    SpawnHarness {
        _directory: directory,
        runtime,
        workspace_id,
        session_id,
        events,
    }
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
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text(prompt.to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    run_id
}

async fn completed_instruction_hash(
    runtime: &SessionRuntime,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    events: &mut SessionEventStream,
    prompt: &str,
) -> String {
    let run_id = submit_prompt_to(runtime, session_id, prompt).await;
    collect_through_finished(events).await;
    runtime
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap()
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap()
        .prompt_identity
        .expect("a sent prompt must retain its prompt identity")
        .instruction_hash
        .to_string()
}

/// Collects events until `run_id` finishes, with a timeout generous
/// enough for a parent run that awaits child runs.
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

/// Answers each turn with the next scripted text and records requests.
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
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, None))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
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
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: text.to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

struct OutputContractHarness {
    _directory: TempDir,
    runtime: SessionRuntime,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    events: SessionEventStream,
}

async fn output_contract_harness(answers: &[&'static str]) -> OutputContractHarness {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(TextScriptLoader {
            requests: Arc::clone(&requests),
            answers: Arc::new(StdMutex::new(answers.iter().copied().collect())),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: ModelSelection {
                    model: Some("test/model".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                approval_mode: ApprovalMode::ReadOnly,
                profile: AgentProfileId::default(),
                correlation: Correlation::default(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    OutputContractHarness {
        _directory: directory,
        runtime,
        requests,
        workspace_id,
        session_id,
        events,
    }
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

async fn submit_with_contract(
    harness: &OutputContractHarness,
    contract: qq_protocol::OutputContract,
) -> Result<RunId, SessionRuntimeError> {
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("report")],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: Some(Box::new(contract)),
            },
        )
        .await?;
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    Ok(run_id)
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

async fn run_snapshot(harness: &OutputContractHarness, run_id: RunId) -> RunSnapshot {
    harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap()
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap()
}

/// `QueueLoader` whose plans permit write children.
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
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

async fn write_child_harness(
    child: Arc<dyn Provider>,
    parent_script: Vec<(&'static str, String)>,
    reviewer: Option<Arc<dyn ApprovalReviewer>>,
    parent_mode: ApprovalMode,
) -> (SpawnHarness, Arc<StdMutex<Vec<ModelRequest>>>) {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: parent_script,
        turn: StdMutex::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.max_active_runs = 8;
    options.approval_timeout = Duration::from_secs(2);
    if let Some(reviewer) = reviewer {
        options = options.with_approval_reviewer(reviewer);
    }
    let runtime = SessionRuntime::open(
        options,
        Arc::new(WriteChildLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![Arc::clone(&parent), parent]),
            },
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, parent_mode).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    (
        SpawnHarness {
            _directory: directory,
            runtime,
            workspace_id,
            session_id,
            events,
        },
        parent_requests,
    )
}

/// `QueueLoader` whose plans permit nesting to `max_depth` and, when set,
/// write children.
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
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

async fn depth_harness(
    routed: Vec<(&'static str, Arc<dyn Provider>)>,
    queue: Vec<Arc<dyn Provider>>,
    max_depth: u16,
    max_active_runs: usize,
) -> SpawnHarness {
    spawn_harness_with_loader(
        Arc::new(DepthLoader {
            inner: QueueLoader {
                routed,
                queue: StdMutex::new(queue),
            },
            max_depth,
            write_children: false,
            pricing: None,
        }),
        max_active_runs,
    )
    .await
}

/// A child that itself delegates once (to `test/grandchild`) and then
/// answers "child done".
fn delegating_child() -> Arc<dyn Provider> {
    Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"look deeper","model":"test/grandchild"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    })
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

fn metered_delegation(
    script: Vec<(&'static str, String)>,
    usage: Vec<Option<TokenUsage>>,
) -> Arc<dyn Provider> {
    Arc::new(MeteredDelegationProvider {
        inner: ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script,
            turn: StdMutex::new(0),
        },
        usage,
        turn: AtomicUsize::new(0),
    })
}

async fn nested_spend_harness(grandchild_usage: Option<TokenUsage>) -> SpawnHarness {
    let parent = metered_delegation(
        vec![
            (
                "spawn_agent",
                r#"{"task":"survey","model":"test/child"}"#.to_owned(),
            ),
            (
                "spawn_agent",
                r#"{"task":"follow up","model":"test/later-child"}"#.to_owned(),
            ),
        ],
        vec![Some(usage(1, 0))],
    );
    let child = metered_delegation(
        vec![(
            "spawn_agent",
            r#"{"task":"look deeper","model":"test/grandchild"}"#.to_owned(),
        )],
        // A later user prompt in this same child session spends 900 tokens;
        // it is not part of the original delegated run's receipt.
        vec![Some(usage(1, 0)), Some(usage(1, 0)), Some(usage(900, 0))],
    );
    spawn_harness_with_loader(
        Arc::new(DepthLoader {
            inner: QueueLoader {
                routed: vec![
                    ("test/child", child),
                    (
                        "test/grandchild",
                        metered_delegation(Vec::new(), vec![grandchild_usage]),
                    ),
                    (
                        "test/later-child",
                        metered_delegation(Vec::new(), vec![Some(usage(10, 0))]),
                    ),
                ],
                queue: StdMutex::new(vec![parent]),
            },
            max_depth: 2,
            write_children: false,
            pricing: Some(budget_pricing()),
        }),
        8,
    )
    .await
}

async fn submit_nested_budget(harness: &SpawnHarness, cost_bound: bool) -> RunId {
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("delegate within the budget")],
                limits: RunLimits {
                    max_total_tokens: Some(100),
                    max_cost_usd_nanos: cost_bound.then_some(100_000),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("expected queued root");
    };
    run_id
}

/// Spawns `spawns` children on a request without tool results, then
/// answers. Stateless, so one instance serves many concurrent runs.
struct StatelessFanOut {
    spawns: usize,
    route: &'static str,
}

impl Provider for StatelessFanOut {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let has_results = request.messages().iter().any(|message| {
            message
                .content()
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        if has_results {
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
        }
        let route = self.route;
        let mut events = Vec::new();
        for index in 0..self.spawns {
            let id = format!("call_{index}");
            events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: "spawn_agent".to_owned(),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: format!(r#"{{"task":"leaf {index}","model":"{route}"}}"#),
            }));
            events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
        }
        events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

/// `QueueLoader` whose plans audit under the given mode; the roster names
/// `test/auditor` as the strong role so the audit child is routable.
struct AuditLoader {
    inner: QueueLoader,
    mode: crate::runtime::AuditMode,
    max_revisions: u16,
}

impl RuntimeLoader for AuditLoader {
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
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

/// Answers with a fixed JSON verdict, recording every request.
struct VerdictProvider {
    reply: &'static str,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl Provider for VerdictProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: self.reply.to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 50,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }),
            }),
        ]))
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

/// A provider that runs one mutating tool turn (to trigger the heuristic
/// audit) and then answers with the next scripted text.
struct MutateThenScriptProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    answers: StdMutex<std::collections::VecDeque<&'static str>>,
    turn: AtomicUsize,
}

impl Provider for MutateThenScriptProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: "call_0".to_owned(),
                    name: "write_file".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0".to_owned(),
                    json: r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: "call_0".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
        }
        let text = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or("out of script");
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: text.to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage: None }),
        ]))
    }
}

/// Loops one `read_file` call per turn until stopped; each turn reports
/// the configured usage (or none). Text turns (when the request declares
/// no tools) stream a final status line.
struct BudgetLoopLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    usage: Option<TokenUsage>,
    pricing: Option<ModelPricing>,
    hang: bool,
}

impl RuntimeLoader for BudgetLoopLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = BudgetLoopProvider {
            requests: Arc::clone(&self.requests),
            usage: self.usage,
            hang: self.hang,
            turn: AtomicUsize::new(0),
        };
        let pricing = self.pricing.clone();
        Box::pin(async move {
            Runtime::new(provider, "test-model", 256)
                .map(|runtime| loaded_runtime(runtime, &request.workspace, pricing))
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
        })
    }
}

struct ChildBudgetLoader {
    inner: QueueLoader,
    write_children: bool,
    audit: crate::runtime::AuditMode,
}

impl RuntimeLoader for ChildBudgetLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = self.inner.next_provider(&request);
        let write_children = self.write_children;
        let audit = self.audit;
        let mut pricing = budget_pricing();
        if request.model.model.as_deref() == Some("test/child") {
            pricing.input_usd_nanos_per_token = 2_000;
            pricing.output_usd_nanos_per_token = 3_000;
        }
        Box::pin(async move {
            Runtime::with_provider(provider, "test-model", 256)
                .map(|runtime| {
                    loaded_runtime(
                        runtime
                            .with_delegation(qq_protocol::DelegationRoster {
                                roster: vec![qq_protocol::DelegationRosterEntry {
                                    route: "test/child".to_owned(),
                                    role: qq_protocol::DelegationRole::Balanced,
                                    note: None,
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
                        Some(pricing),
                    )
                })
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
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

async fn child_budget_harness(
    parent: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    write_children: bool,
    audit: crate::runtime::AuditMode,
) -> SpawnHarness {
    let directory = tempfile::tempdir().unwrap();
    let (reviewer, _) = StubReviewer::immediate(ReviewVerdict::free(ReviewDecision::Approve));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"))
            .with_approval_reviewer(reviewer),
        Arc::new(ChildBudgetLoader {
            inner: QueueLoader {
                routed: vec![("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
            write_children,
            audit,
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Full).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("expected session")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    SpawnHarness {
        _directory: directory,
        runtime,
        workspace_id,
        session_id,
        events,
    }
}

async fn submit_child_budget_prompt(harness: &SpawnHarness, limits: RunLimits) -> RunId {
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("delegate")],
                limits,
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("expected run")
    };
    run_id
}

struct BudgetLoopProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    usage: Option<TokenUsage>,
    hang: bool,
    turn: AtomicUsize,
}

impl Provider for BudgetLoopProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let has_tools = !request.tools().is_empty();
        self.requests.lock().unwrap().push(request);
        if self.hang {
            return Box::pin(stream::pending());
        }
        let turn = self.turn.fetch_add(1, Ordering::SeqCst);
        if has_tools {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: format!("call_{turn}"),
                    name: "read_file".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: format!("call_{turn}"),
                    json: r#"{"path":"note.txt"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                    id: format!("call_{turn}"),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: self.usage.map(provider_usage_of),
                }),
            ]))
        } else {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "final status".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: self.usage.map(provider_usage_of),
                }),
            ]))
        }
    }
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

struct BudgetHarness {
    _directory: TempDir,
    runtime: SessionRuntime,
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    events: SessionEventStream,
    database_path: PathBuf,
}

async fn budget_harness(loader: BudgetLoopLoader) -> BudgetHarness {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "note").unwrap();
    let requests = Arc::clone(&loader.requests);
    let database_path = directory.path().join("sessions.sqlite3");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        Arc::new(loader),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Auto).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    BudgetHarness {
        _directory: directory,
        runtime,
        requests,
        workspace_id,
        session_id,
        events,
        database_path,
    }
}

async fn queue_limited_prompt(harness: &BudgetHarness, limits: RunLimits) -> RunId {
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("loop".to_owned())],
                limits,
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    run_id
}

fn exhaustion_of(observed: &[SessionEventEnvelope], run_id: RunId) -> BudgetExhaustion {
    match finished_outcome(observed, run_id) {
        Some(RunOutcome::BudgetExhausted { exhaustion }) => *exhaustion,
        other => panic!("expected a budget_exhausted outcome, got {other:?}"),
    }
}
