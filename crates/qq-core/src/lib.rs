//! Agent runtime, session behavior, tools, and persistence.

#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_stream::stream;
use futures_core::Stream;
use futures_util::{StreamExt, stream as futures_stream};
use qq_protocol::{
    ApprovalMode, ContentHash, RunActivity, RunCommand, RunEvent, RunFailureKind,
    RunPromptIdentity, TokenUsage, ToolCallId,
};
use qq_provider::{
    ContentBlock, Message, ModelRequest, Provider, ProviderErrorKind, ProviderEvent, Role,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod approval;
mod mcp;
mod runtime;
mod sessions;
mod tools;
mod workspace;

pub use runtime::TurnRetryPolicy;
use runtime::{
    AGENT_PROMPT_VERSION, GateDecision, PendingToolCall, RuntimeEvent, RuntimeToolCall,
    SPAWN_UNAVAILABLE_RESULT, SpawnAgentFuture, SpawnAgentOutcome, SubagentSpawner, ToolGate,
    ToolGateFuture, TurnBlock, agent_system_prompt, attempts_message,
    is_transient_provider_failure, tool_schema_hash,
};

pub use mcp::{MCP_TOOL_PREFIX, McpCallFuture, McpRegistry, McpSpecsFuture, McpToolResult};
pub use sessions::{
    ApprovalReviewer, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, ReviewFuture,
    ReviewRequest, ReviewVerdict, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest,
    RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError, SessionRuntimeOptions,
    SpawnModelValidationFuture, WorkerRuntimeLoadFuture, WorkspaceGrantAuthority,
    WorkspaceGrantSeed,
};

pub type RunStream = Pin<Box<dyn Stream<Item = RunEvent> + Send + 'static>>;
type RuntimeStream = Pin<Box<dyn Stream<Item = RuntimeEvent> + Send + 'static>>;

const MAX_TOOL_CALLS_PER_TURN: usize = 16;
// A runaway-loop backstop for one internal execution slice, not a task
// completion limit. Before a new model turn can exceed this ceiling, QQ
// records a tool-free checkpoint, resets the counter, and continues the same
// run with tools restored.
const MAX_TOOL_CALLS_PER_SLICE: usize = 256;
const SLICE_CHECKPOINT_NOTICE: &str = "This execution slice is at its safe tool-call boundary, so no tools \
are available for this reply. Record a concise checkpoint of what was accomplished, what \
remains, and the exact next step. QQ will persist this checkpoint and continue the same run \
with tools restored.";
const SLICE_CONTINUATION_NOTICE: &str = "Continue the task from the preceding persisted \
checkpoint. Tools are available again. Do not stop at a progress summary: complete the user's \
request unless an explicit overall budget, cancellation, or genuine failure prevents it.";
const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_TOOL_CALL_ID_BYTES: usize = 1_024;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_RUN_MODEL_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUN_REASONING_BYTES: usize = 1024 * 1024;
const MAX_PARALLEL_READS: usize = 4;
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Capabilities granted to one model run. Guidance is allowed for durable
/// user commands, including explicit follow-ups in a child session. Runtime-
/// authored compactions and model-authored child tasks remain restricted.
pub(crate) struct RunCapabilities {
    spawner: Option<Arc<dyn SubagentSpawner>>,
    allow_guidance: bool,
    slash_is_literal: bool,
}

impl RunCapabilities {
    pub(crate) fn user(spawner: Option<Arc<dyn SubagentSpawner>>) -> Self {
        Self {
            spawner,
            allow_guidance: true,
            slash_is_literal: false,
        }
    }

    /// Marks a leading slash as already normalized from the durable `//`
    /// escape. The message is provider-ready and must not be reinterpreted as
    /// a guidance invocation.
    pub(crate) fn with_literal_slash(mut self, literal: bool) -> Self {
        self.slash_is_literal = literal;
        self
    }

    pub(crate) const fn restricted() -> Self {
        Self {
            spawner: None,
            allow_guidance: false,
            slash_is_literal: false,
        }
    }
}

struct StaticPolicyGate {
    mode: ApprovalMode,
    /// Workspace-configured grants (today: MCP allowlist entries by exact
    /// namespaced name). Mode still wins: read-only denies granted tools.
    grants: approval::SessionGrants,
}

impl ToolGate for StaticPolicyGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let class = approval::classify(&call.name, &call.arguments);
        let decision = match approval::evaluate(self.mode, &call.name, &class, &self.grants) {
            approval::PolicyDecision::Execute => GateDecision::Execute,
            approval::PolicyDecision::Deny => GateDecision::Deny {
                message: approval::POLICY_DENIED_RESULT.to_owned(),
            },
            approval::PolicyDecision::RequireApproval => GateDecision::Deny {
                message: approval::UNATTENDED_DENIED_RESULT.to_owned(),
            },
        };
        Box::pin(std::future::ready(decision))
    }
}

/// Runs protocol commands against a configured model provider.
#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    model: Arc<str>,
    max_output_tokens: u32,
    mcp: Option<Arc<dyn McpRegistry>>,
    spawn_model_routes: Arc<[String]>,
    turn_retry: TurnRetryPolicy,
}

impl Runtime {
    pub fn new(
        provider: impl Provider + 'static,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        Self::with_provider(Arc::new(provider), model, max_output_tokens)
    }

    /// Creates a runtime without reboxing an already shared provider.
    pub fn with_provider(
        provider: Arc<dyn Provider>,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(RuntimeConfigError::EmptyModel);
        }
        if max_output_tokens == 0 {
            return Err(RuntimeConfigError::ZeroMaxOutputTokens);
        }

        Ok(Self {
            provider,
            model,
            max_output_tokens,
            mcp: None,
            spawn_model_routes: Arc::from([]),
            turn_retry: TurnRetryPolicy::default(),
        })
    }

    /// Overrides the transient-failure retry policy applied to model turns.
    #[must_use]
    pub fn with_turn_retry_policy(mut self, policy: TurnRetryPolicy) -> Self {
        self.turn_retry = policy;
        self
    }

    /// Attaches a registry of configuration-declared MCP servers. Its cached
    /// tool declarations join the built-ins for every run of this runtime,
    /// and `mcp__`-named calls dispatch to it.
    #[must_use]
    pub fn with_mcp_registry(mut self, registry: Arc<dyn McpRegistry>) -> Self {
        self.mcp = Some(registry);
        self
    }

    /// Restricts model-visible sub-agent overrides to authenticated canonical
    /// routes supplied by the embedding application. Omission still resolves
    /// through the configured worker model and persisted parent selection.
    #[must_use]
    pub fn with_spawn_model_routes(mut self, mut routes: Vec<String>) -> Self {
        routes.sort();
        routes.dedup();
        routes.retain(|route| !route.trim().is_empty());
        self.spawn_model_routes = routes.into();
        self
    }

    /// Runs one command and returns events as they become available.
    pub fn run(&self, command: RunCommand) -> RunStream {
        self.run_in_workspace(
            command,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    /// Runs one command with read-only tools scoped to `workspace`.
    pub fn run_in_workspace(&self, command: RunCommand, workspace: PathBuf) -> RunStream {
        public_run_stream(
            self.run_messages_in_workspace(vec![Message::user(command.into_prompt())], workspace),
        )
    }

    /// Runs a multi-turn model/tool loop with explicit prior conversation context.
    pub fn run_messages(&self, messages: Vec<Message>) -> RunStream {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        public_run_stream(self.run_messages_in_workspace(messages, workspace))
    }

    fn run_messages_in_workspace(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
    ) -> RuntimeStream {
        self.run_messages_in_workspace_with_cancellation(
            messages,
            workspace,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn run_messages_in_workspace_with_cancellation(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
    ) -> RuntimeStream {
        // Configuration allowlists are the only grants a gate-less run has;
        // read-only mode still denies them inside `evaluate`.
        let grants = approval::SessionGrants {
            tools: self
                .mcp
                .as_ref()
                .map(|registry| registry.config_grants().into_iter().collect())
                .unwrap_or_default(),
            shell_prefixes: Vec::new(),
        };
        self.run_loop(
            messages,
            workspace,
            cancelled,
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::Ask,
                grants,
            }),
            Arc::new(workspace::FileState::default()),
        )
    }

    pub(crate) fn run_loop(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
    ) -> RuntimeStream {
        self.run_loop_with_spawner(
            messages,
            workspace,
            cancelled,
            gate,
            file_state,
            RunCapabilities::user(None),
        )
    }

    pub(crate) fn run_loop_with_spawner(
        &self,
        mut messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
        capabilities: RunCapabilities,
    ) -> RuntimeStream {
        let provider = Arc::clone(&self.provider);
        let model = Arc::clone(&self.model);
        let max_output_tokens = self.max_output_tokens;
        let mcp = self.mcp.clone();
        let spawn_model_routes = Arc::clone(&self.spawn_model_routes);
        let turn_retry = self.turn_retry;
        Box::pin(stream! {
            let RunCapabilities {
                spawner,
                allow_guidance,
                slash_is_literal,
            } = capabilities;
            let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancelled));
            yield RuntimeEvent::Started;

            if messages.is_empty() || messages.iter().any(|message| !message.has_content()) {
                yield RuntimeEvent::Failed {
                    kind: RunFailureKind::InvalidCommand,
                    message: "conversation messages must not be empty".to_owned(),
                };
                return;
            }

            let parsed_invocation = match if allow_guidance && !slash_is_literal {
                workspace::parse_invocation(&mut messages)
            } else {
                Ok(workspace::ParsedInvocation {
                    guidance: None,
                })
            } {
                Ok(request) => request,
                Err(error) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::InvalidCommand,
                        message: error.to_string(),
                    };
                    return;
                }
            };

            let (workspace, workspace_instructions, selected_guidance) = match workspace::prepare_workspace(
                workspace,
                Arc::clone(&cancelled),
                parsed_invocation.guidance,
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error @ (workspace::WorkspacePreparationError::Canonicalize { .. }
                    | workspace::WorkspacePreparationError::Open { .. })) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::InvalidCommand,
                        message: format!("could not open the workspace directory: {error}"),
                    };
                    return;
                }
                Err(workspace::WorkspacePreparationError::Guidance(error)) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::InvalidCommand,
                        message: error.to_string(),
                    };
                    return;
                }
                Err(error) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    };
                    return;
                }
            };
            // MCP declarations join the built-ins once per run: the cached
            // specs are fetched here (connecting lazily on first use) so
            // every turn of the run sees one stable tool list. The `mcp__`
            // prefix keeps collisions with built-ins impossible, and specs
            // that do not carry it are discarded to keep dispatch unambiguous.
            let mut tool_specs = tools::specs();
            // The sub-agent tool is declared only when this run may spawn:
            // depth is one, so child runs (spawner-less) never see it.
            if spawner.is_some() {
                tool_specs.push(tools::spawn_agent_spec(&spawn_model_routes));
            }
            if let Some(registry) = &mcp {
                for spec in registry.tool_specs().await {
                    if spec.name().starts_with(MCP_TOOL_PREFIX)
                        && spec.name().len() <= MAX_TOOL_NAME_BYTES
                        && !tool_specs.iter().any(|existing| existing.name() == spec.name())
                    {
                        tool_specs.push(spec);
                    }
                }
            }
            let system: Arc<str> = Arc::from(agent_system_prompt(
                workspace.path(),
                &tool_specs,
                &workspace_instructions,
                selected_guidance.as_ref(),
            ));
            yield RuntimeEvent::Prepared {
                identity: RunPromptIdentity {
                    version: AGENT_PROMPT_VERSION,
                    instruction_hash: workspace_instructions.hash(),
                    system_prompt_hash: Some(ContentHash::from_bytes(
                        Sha256::digest(system.as_bytes()).into(),
                    )),
                    tool_schema_hash: Some(tool_schema_hash(&tool_specs)),
                    selected_guidance: selected_guidance
                        .as_ref()
                        .map(|guidance| Box::new(guidance.identity())),
                },
            };

            let mut slice_tool_calls = 0_usize;
            let mut model_text_bytes = 0_usize;
            let mut continuing_slice = false;
            for turn_ordinal in 1..=u16::MAX {
                // Reserve enough capacity for the largest valid provider
                // turn. Without this reservation, a slice at (for example)
                // 255 calls could accept a 16-call turn and overshoot its
                // strict ceiling before reaching the next turn boundary.
                // The tool-free checkpoint is persisted but is not the run's
                // terminal outcome; the next turn starts a new slice.
                let checkpoint_turn = slice_tool_calls
                    .saturating_add(MAX_TOOL_CALLS_PER_TURN)
                    > MAX_TOOL_CALLS_PER_SLICE;
                let continuation_turn = std::mem::take(&mut continuing_slice);
                // Transient provider failures (overload, rate limits, dropped
                // connections) re-issue this turn with backoff instead of
                // failing the run — but only while nothing user-visible has
                // streamed, so a retry can never duplicate output.
                let mut attempt = 1_u32;
                let (blocks, pending_calls, terminal_usage) = 'turn: loop {
                let request =
                    ModelRequest::new(Arc::clone(&model), messages.clone(), max_output_tokens);
                let request = if checkpoint_turn {
                    request.with_system(format!("{system}\n\n{SLICE_CHECKPOINT_NOTICE}"))
                } else if continuation_turn {
                    request
                        .with_tools(tool_specs.clone())
                        .with_system(format!("{system}\n\n{SLICE_CONTINUATION_NOTICE}"))
                } else {
                    request
                        .with_tools(tool_specs.clone())
                        .with_system(Arc::clone(&system))
                };
                let mut activity = RunActivity::WaitingForProvider;
                if attempt == 1 {
                    yield RuntimeEvent::ActivityChanged { activity };
                }
                let mut provider_events = provider.stream(request);
                let mut pending_calls = Vec::<PendingToolCall>::new();
                let mut calls_by_provider_id = HashMap::<String, usize>::new();
                let mut blocks = Vec::<TurnBlock>::new();
                let mut terminal_usage = None;
                let mut completed = false;
                let mut reasoning_bytes = 0_usize;
                let mut open_reasoning = None;
                let mut turn_streamed = false;

                while let Some(event) = provider_events.next().await {
                    match event {
                        Ok(ProviderEvent::ReasoningStarted { kind }) => {
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider started a reasoning block before completing the previous block".to_owned(),
                                };
                                return;
                            }
                            open_reasoning = Some(kind);
                            turn_streamed = true;
                            if activity != RunActivity::Reasoning {
                                activity = RunActivity::Reasoning;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            yield RuntimeEvent::ReasoningStarted { kind };
                        }
                        Ok(ProviderEvent::ReasoningDelta { kind, text }) => {
                            if open_reasoning != Some(kind) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider streamed reasoning outside its matching block".to_owned(),
                                };
                                return;
                            }
                            if reasoning_bytes.saturating_add(text.len()) > MAX_RUN_REASONING_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "displayable reasoning exceeded the 1 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            reasoning_bytes += text.len();
                            if !text.is_empty() {
                                yield RuntimeEvent::ReasoningDelta { kind, text };
                            }
                        }
                        Ok(ProviderEvent::ReasoningCompleted { kind }) => {
                            if open_reasoning != Some(kind) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider completed an unknown reasoning block".to_owned(),
                                };
                                return;
                            }
                            open_reasoning = None;
                            yield RuntimeEvent::ReasoningCompleted { kind };
                        }
                        Ok(ProviderEvent::OutputTextDelta { text }) => {
                            turn_streamed = true;
                            if activity != RunActivity::GeneratingResponse {
                                activity = RunActivity::GeneratingResponse;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "model text exceeded the 16 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            model_text_bytes += text.len();
                            append_turn_text(&mut blocks, &text);
                            yield RuntimeEvent::OutputTextDelta { text };
                        }
                        Ok(ProviderEvent::RefusalDelta { text }) => {
                            turn_streamed = true;
                            if activity != RunActivity::GeneratingResponse {
                                activity = RunActivity::GeneratingResponse;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "model text exceeded the 16 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            model_text_bytes += text.len();
                            append_turn_text(&mut blocks, &text);
                            yield RuntimeEvent::RefusalDelta { text };
                        }
                        Ok(ProviderEvent::ToolCallStarted { id, name }) => {
                            turn_streamed = true;
                            if activity != RunActivity::PreparingToolCall {
                                activity = RunActivity::PreparingToolCall;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if id.is_empty() || id.len() > MAX_TOOL_CALL_ID_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider tool call ID is empty or exceeds 1 KiB".to_owned(),
                                };
                                return;
                            }
                            if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider tool name is empty or exceeds 128 bytes".to_owned(),
                                };
                                return;
                            }
                            if pending_calls.len() >= MAX_TOOL_CALLS_PER_TURN {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("model requested more than {MAX_TOOL_CALLS_PER_TURN} tools in one turn"),
                                };
                                return;
                            }
                            // The per-slice ceiling reserves a complete turn
                            // at the top of the loop; a call arriving on the
                            // declared tool-free checkpoint turn is a provider
                            // bug.
                            if checkpoint_turn {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider requested a tool on the tool-free checkpoint turn, which declares none".to_owned(),
                                };
                                return;
                            }
                            if calls_by_provider_id.contains_key(&id) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider reused tool call ID {id:?} in one turn"),
                                };
                                return;
                            }
                            let index = pending_calls.len();
                            calls_by_provider_id.insert(id.clone(), index);
                            pending_calls.push(PendingToolCall {
                                provider_call_id: id,
                                name,
                                arguments: String::new(),
                                parsed_arguments: None,
                                argument_error: None,
                                completed: false,
                            });
                            slice_tool_calls += 1;
                            blocks.push(TurnBlock::ToolCall(index));
                        }
                        Ok(ProviderEvent::ToolCallArgumentsDelta { id, json }) => {
                            let Some(index) = calls_by_provider_id.get(&id).copied() else {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider streamed arguments for unknown tool call {id:?}"),
                                };
                                return;
                            };
                            let call = &mut pending_calls[index];
                            if call.completed {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider streamed arguments after completing tool call {id:?}"),
                                };
                                return;
                            }
                            if call.arguments.len().saturating_add(json.len()) > MAX_TOOL_ARGUMENT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("tool call {id:?} arguments exceed the 64 KiB limit"),
                                };
                                return;
                            }
                            call.arguments.push_str(&json);
                        }
                        Ok(ProviderEvent::ToolCallCompleted { id }) => {
                            let Some(index) = calls_by_provider_id.get(&id).copied() else {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider completed unknown tool call {id:?}"),
                                };
                                return;
                            };
                            let call = &mut pending_calls[index];
                            if call.completed {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider completed tool call {id:?} twice"),
                                };
                                return;
                            }
                            let arguments = if call.arguments.trim().is_empty() {
                                "{}"
                            } else {
                                &call.arguments
                            };
                            // Malformed argument JSON is the model's mistake, not a
                            // run failure: return a retryable tool error instead.
                            let parsed = match serde_json::from_str(arguments) {
                                Ok(arguments) => arguments,
                                Err(error) => {
                                    call.argument_error = Some(format!(
                                        "tool call arguments were not valid JSON: {error}"
                                    ));
                                    serde_json::Value::Object(serde_json::Map::new())
                                }
                            };
                            call.arguments = serde_json::to_string(&parsed)
                                .expect("a parsed JSON value must serialize");
                            call.parsed_arguments = Some(parsed);
                            call.completed = true;
                        }
                        Ok(ProviderEvent::Completed { usage }) => {
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider completed the turn with an unfinished reasoning block".to_owned(),
                                };
                                return;
                            }
                            if let Some(call) = pending_calls.iter().find(|call| !call.completed) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!(
                                        "provider completed the turn before tool call {:?}",
                                        call.provider_call_id
                                    ),
                                };
                                return;
                            }
                            terminal_usage = usage.map(provider_usage);
                            completed = true;
                            break;
                        }
                        Err(error) => {
                            if is_transient_provider_failure(error.kind())
                                && !turn_streamed
                                && attempt < turn_retry.max_attempts()
                            {
                                let delay = turn_retry.delay(attempt);
                                attempt += 1;
                                tokio::time::sleep(delay).await;
                                continue 'turn;
                            }
                            yield RuntimeEvent::Failed {
                                kind: run_failure_kind(error.kind()),
                                message: attempts_message(error.to_string(), attempt),
                            };
                            return;
                        }
                    }
                }

                if !completed {
                    // A stream that ends without a terminal event is the same
                    // transient class as a dropped connection.
                    if !turn_streamed && attempt < turn_retry.max_attempts() {
                        let delay = turn_retry.delay(attempt);
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        continue 'turn;
                    }
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::ProviderProtocol,
                        message: attempts_message(
                            "provider stream ended without a terminal event".to_owned(),
                            attempt,
                        ),
                    };
                    return;
                }

                break 'turn (blocks, pending_calls, terminal_usage);
                };

                let assistant_content = blocks
                    .into_iter()
                    .filter_map(|block| match block {
                        TurnBlock::Text(text) if text.is_empty() => None,
                        TurnBlock::Text(text) => Some(ContentBlock::Text { text }),
                        TurnBlock::ToolCall(index) => {
                            let call = &pending_calls[index];
                            Some(ContentBlock::ToolCall {
                                id: call.provider_call_id.clone(),
                                name: call.name.clone(),
                                arguments: call
                                    .parsed_arguments
                                    .clone()
                                    .expect("completed calls have parsed arguments"),
                            })
                        }
                    })
                    .collect::<Vec<_>>();
                let assistant = Message::new(Role::Assistant, assistant_content);
                let mut calls = Vec::with_capacity(pending_calls.len());
                let mut id_generation_failed = None;
                for (index, pending) in pending_calls.into_iter().enumerate() {
                    let id = match ToolCallId::generate() {
                        Ok(id) => id,
                        Err(error) => {
                            id_generation_failed = Some(error.to_string());
                            break;
                        }
                    };
                    calls.push(RuntimeToolCall {
                        id,
                        turn_ordinal,
                        call_ordinal: u16::try_from(index + 1)
                            .expect("the per-turn tool bound fits u16"),
                        provider_call_id: pending.provider_call_id,
                        name: pending.name,
                        arguments: pending.arguments,
                        argument_error: pending.argument_error,
                    });
                }
                if let Some(message) = id_generation_failed {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Server,
                        message,
                    };
                    return;
                }
                // The completed turn and its requested calls travel on one event
                // so the store can persist them atomically.
                yield RuntimeEvent::AssistantTurnCompleted {
                    turn_ordinal,
                    message: assistant.clone(),
                    usage: terminal_usage,
                    calls: calls.clone(),
                };

                if checkpoint_turn && !assistant.has_content() {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::ProviderResponse,
                        message: "provider returned an empty slice checkpoint".to_owned(),
                    };
                    return;
                }
                if calls.is_empty() && checkpoint_turn {
                    messages.push(assistant);
                    slice_tool_calls = 0;
                    continuing_slice = true;
                    continue;
                }
                if calls.is_empty() {
                    yield RuntimeEvent::Completed;
                    return;
                }
                messages.push(assistant);

                // Policy resolves sequentially in request order, after the
                // turn and its `requested` call rows are persisted, so
                // approval prompts arrive one at a time. Calls with malformed
                // arguments never reach the gate: there is nothing executable
                // to approve, so they short-circuit to their tool error below.
                let mut results = vec![None; calls.len()];
                for (index, call) in calls.iter().enumerate() {
                    if call.argument_error.is_some() {
                        continue;
                    }
                    match gate.resolve(call).await {
                        GateDecision::Execute => {}
                        GateDecision::Deny { message } => {
                            results[index] = Some(tools::ToolExecutionResult {
                                content: message.clone(),
                                is_error: true,
                                file_state: None,
                            });
                            yield RuntimeEvent::ToolCallDenied { id: call.id, message };
                        }
                    }
                }
                let approved = calls
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| results[*index].is_none())
                    .map(|(_, call)| call.clone())
                    .collect::<Vec<_>>();
                for call in &approved {
                    yield RuntimeEvent::ToolCallStarted { id: call.id };
                }

                let execute_one = |call: RuntimeToolCall,
                                   output: Option<
                    tokio::sync::mpsc::UnboundedSender<String>,
                >| {
                    let workspace = workspace.clone();
                    let file_state = Arc::clone(&file_state);
                    let cancelled = Arc::clone(&cancelled);
                    let mcp = mcp.clone();
                    let spawner = spawner.clone();
                    async move {
                        let result = match call.argument_error.clone() {
                            Some(error) => tools::ToolExecutionResult {
                                content: error,
                                is_error: true,
                                file_state: None,
                            },
                            // spawn_agent dispatches to the session layer. A
                            // run without a spawner rejects the call outright:
                            // the declaration is already absent there, but a
                            // model may still guess the name.
                            None if call.name == tools::SPAWN_AGENT_TOOL => match &spawner {
                                Some(spawner) => {
                                    match serde_json::from_str::<tools::SpawnAgentArgs>(
                                        &call.arguments,
                                    ) {
                                        Ok(arguments) if arguments.task.trim().is_empty() => {
                                            tools::bounded_result(
                                                "task must not be empty".to_owned(),
                                                true,
                                            )
                                        }
                                        Ok(mut arguments) => {
                                            arguments.model = arguments.model.and_then(|model| {
                                                let model = model.trim().to_owned();
                                                (!model.is_empty()).then_some(model)
                                            });
                                            // The advertised route list is
                                            // schema guidance only; the
                                            // session spawner validates every
                                            // resolved route against the
                                            // authenticated served model list
                                            // at spawn time, before any
                                            // durable child state exists.
                                            let outcome = spawner
                                                .spawn(arguments.task, arguments.model)
                                                .await;
                                            tools::bounded_result(
                                                outcome.content,
                                                outcome.is_error,
                                            )
                                        }
                                        Err(error) => tools::bounded_result(
                                            format!("invalid arguments: {error}"),
                                            true,
                                        ),
                                    }
                                }
                                None => {
                                    tools::bounded_result(SPAWN_UNAVAILABLE_RESULT.to_owned(), true)
                                }
                            },
                            // MCP calls dispatch to the shared registry; the
                            // outcome flows through the same bounded-result
                            // truncation as built-in tools, so an MCP call is
                            // indistinguishable from a built-in on the wire.
                            None if call.name.starts_with(MCP_TOOL_PREFIX) && mcp.is_some() => {
                                let outcome = mcp
                                    .expect("the MCP registry was just checked")
                                    .call(call.name.clone(), call.arguments.clone(), cancelled)
                                    .await;
                                tools::bounded_result(outcome.content, outcome.is_error)
                            }
                            None => {
                                tools::execute(
                                    workspace,
                                    file_state,
                                    call.name.clone(),
                                    call.arguments.clone(),
                                    cancelled,
                                    output,
                                )
                                .await
                            }
                        };
                        (call, result)
                    }
                };
                // Read-only turns overlap under a small bound; a turn with any
                // mutating or shell call runs entirely in request order so
                // side effects never interleave and every read is
                // deterministically ordered against the mutations.
                let sequential = approved.iter().any(|call| {
                    !matches!(
                        approval::classify(&call.name, &call.arguments),
                        approval::ToolClass::ReadOnly
                    )
                });
                if sequential {
                    for call in approved {
                        // Live output chunks (shell) interleave with execution:
                        // drain the channel while the call runs so long
                        // commands render as they print.
                        let call_id = call.id;
                        let (delta_sender, mut deltas) =
                            tokio::sync::mpsc::unbounded_channel::<String>();
                        let mut execution = Box::pin(execute_one(call, Some(delta_sender)));
                        let (call, result) = loop {
                            tokio::select! {
                                biased;
                                chunk = deltas.recv() => match chunk {
                                    Some(chunk) => {
                                        yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                                    }
                                    // The execution dropped its sender early;
                                    // nothing more can stream, so just finish.
                                    None => break execution.await,
                                },
                                completed = &mut execution => break completed,
                            }
                        };
                        // Chunks sent in the execution's final poll may still
                        // be buffered; drain them before the terminal event.
                        while let Ok(chunk) = deltas.try_recv() {
                            yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                        }
                        results[usize::from(call.call_ordinal - 1)] = Some(result.clone());
                        let display = (!result.is_error)
                            .then(|| approval::edit_result_display(&call.name, &call.arguments))
                            .flatten();
                        yield RuntimeEvent::ToolCallFinished {
                            id: call.id,
                            result: result.content,
                            is_error: result.is_error,
                            file_state: result.file_state,
                            display,
                        };
                    }
                } else {
                    let mut executions = futures_stream::iter(
                        approved.into_iter().map(|call| execute_one(call, None)),
                    )
                        .buffer_unordered(MAX_PARALLEL_READS);
                    while let Some((call, result)) = executions.next().await {
                        results[usize::from(call.call_ordinal - 1)] = Some(result.clone());
                        yield RuntimeEvent::ToolCallFinished {
                            id: call.id,
                            result: result.content,
                            is_error: result.is_error,
                            file_state: result.file_state,
                            // Read-only turns never carry an edit display.
                            display: None,
                        };
                    }
                }
                let result_blocks = calls
                    .iter()
                    .zip(results.into_iter())
                    .map(|(call, result)| {
                        let result = result.expect("every bounded tool execution completed");
                        ContentBlock::ToolResult {
                            call_id: call.provider_call_id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                        }
                    })
                    .collect();
                messages.push(Message::tool_results(result_blocks));
            }

            yield RuntimeEvent::Failed {
                kind: RunFailureKind::Policy,
                message: "run exhausted the durable u16 model-turn ordinal space".to_owned(),
            };
        })
    }
}

/// Translates the richer internal runtime stream into the direct public API.
/// Session execution consumes the internal preparation and tool events
/// separately so it can persist them before publishing visible state.
fn public_run_stream(mut events: RuntimeStream) -> RunStream {
    Box::pin(stream! {
        while let Some(event) = events.next().await {
            match event {
                RuntimeEvent::Started => yield RunEvent::Started,
                RuntimeEvent::Prepared { .. } => {}
                RuntimeEvent::ActivityChanged { activity } => {
                    yield RunEvent::ActivityChanged { activity };
                }
                RuntimeEvent::ReasoningStarted { kind } => {
                    yield RunEvent::ReasoningStarted { kind };
                }
                RuntimeEvent::ReasoningDelta { kind, text } => {
                    yield RunEvent::ReasoningDelta { kind, text };
                }
                RuntimeEvent::ReasoningCompleted { kind } => {
                    yield RunEvent::ReasoningCompleted { kind };
                }
                RuntimeEvent::OutputTextDelta { text } => {
                    yield RunEvent::OutputTextDelta { text };
                }
                RuntimeEvent::RefusalDelta { text } => {
                    yield RunEvent::RefusalDelta { text };
                }
                RuntimeEvent::AssistantTurnCompleted { usage: Some(usage), .. } => {
                    yield RunEvent::Usage { usage };
                }
                RuntimeEvent::AssistantTurnCompleted { usage: None, .. }
                | RuntimeEvent::ToolCallStarted { .. }
                | RuntimeEvent::ToolCallDenied { .. }
                | RuntimeEvent::ToolCallOutputDelta { .. }
                | RuntimeEvent::ToolCallFinished { .. } => {}
                RuntimeEvent::Completed => {
                    yield RunEvent::Completed;
                    return;
                }
                RuntimeEvent::Failed { kind, message } => {
                    yield RunEvent::Failed { kind, message };
                    return;
                }
            }
        }
    })
}

fn append_turn_text(blocks: &mut Vec<TurnBlock>, text: &str) {
    match blocks.last_mut() {
        Some(TurnBlock::Text(existing)) => existing.push_str(text),
        Some(TurnBlock::ToolCall(_)) | None => blocks.push(TurnBlock::Text(text.to_owned())),
    }
}

const fn provider_usage(usage: qq_provider::ProviderUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
    }
}

const fn run_failure_kind(kind: ProviderErrorKind) -> RunFailureKind {
    match kind {
        ProviderErrorKind::Configuration => RunFailureKind::ProviderConfiguration,
        ProviderErrorKind::Authentication => RunFailureKind::ProviderAuthentication,
        ProviderErrorKind::RateLimited => RunFailureKind::ProviderRateLimited,
        ProviderErrorKind::InvalidRequest => RunFailureKind::ProviderInvalidRequest,
        ProviderErrorKind::ContextExceeded => RunFailureKind::ProviderContextExceeded,
        ProviderErrorKind::Unavailable => RunFailureKind::ProviderUnavailable,
        ProviderErrorKind::Transport => RunFailureKind::ProviderTransport,
        ProviderErrorKind::Api => RunFailureKind::ProviderApi,
        ProviderErrorKind::Response => RunFailureKind::ProviderResponse,
        ProviderErrorKind::Protocol => RunFailureKind::ProviderProtocol,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeConfigError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("maximum output tokens must be greater than zero")]
    ZeroMaxOutputTokens,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use futures_util::{StreamExt, stream};
    use qq_protocol::ReasoningKind;
    use qq_provider::{ProviderError, ProviderStream};

    use super::*;

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
                    }),
                }),
            ]))
        }
    }

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
        assert!(system.contains("observable completion criteria"));
        assert!(system.contains("preserve unrelated work"));
        assert!(system.contains("analysis-only"));
        assert!(system.contains("failed tools and tests as evidence"));
        assert!(system.contains("continue when a safe path remains"));
        assert!(system.contains("narrowest relevant verification before broader checks"));
        assert!(system.contains("Do not claim success without evidence"));
        assert!(system.contains("remaining failures and uncertainty honestly"));
        assert!(system.contains("time, token, cost, and safety budgets"));
        assert!(system.contains("root-to-leaf"));
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
                    0 => tool_turn("list-src", "list_dir", r#"{"path":"src"}"#),
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
                                && content == "Follow src fallback.\n"
                        ));
                        tool_turn("list-feature", "list_dir", r#"{"path":"src/feature"}"#)
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
                                && content == "Follow feature policy.\n"
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
                            }]) if call_id == "read-before" && content == "before\n"
                        ));
                        tool_turn(
                            "edit",
                            "edit_file",
                            r#"{"path":"src/feature/note.txt","old_string":"before\n","new_string":"after\n"}"#,
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
                            }]) if call_id == "read-after" && content == "after\n"
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
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
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
        let cancelled = Arc::new(AtomicBool::new(false));
        let run_cancelled = Arc::clone(&cancelled);
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
        cancelled.store(true, Ordering::Release);
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

    #[tokio::test]
    async fn executes_read_tools_and_returns_results_in_request_order() {
        struct ToolLoopProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ToolLoopProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "read".to_owned(),
                            name: "read_file".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "read".to_owned(),
                            json: r#"{"path":"note.txt"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "read".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "list".to_owned(),
                            name: "list_dir".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "list".to_owned(),
                            json: r#"{"path":"."}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "list".to_owned(),
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
        std::fs::write(directory.path().join("note.txt"), "contents\n").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            ToolLoopProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    RuntimeEvent::AssistantTurnCompleted { calls, .. } => Some(calls.len()),
                    _ => None,
                })
                .sum::<usize>(),
            2
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools().len(), 6);
        let system = requests[0]
            .system()
            .expect("agent runs set a system prompt");
        assert!(system.contains("edit_file"));
        assert!(system.contains(directory.path().to_str().unwrap()));
        let result_message = &requests[1].messages()[2];
        assert!(matches!(
            result_message.content(),
            [
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    call_id: second_id,
                    content: second_content,
                    is_error: false,
                }
            ] if call_id == "read"
                && content == "contents\n"
                && second_id == "list"
                && second_content == "note.txt\n"
        ));
    }

    #[tokio::test]
    async fn read_tools_overlap_and_cancellation_stops_in_flight_work() {
        struct ConcurrentProvider {
            turn: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ConcurrentProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request);
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "slow".to_owned(),
                            name: "__test_delay".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "slow".to_owned(),
                            json: r#"{"delay_ms":50,"result":"slow","synchronize":true}"#
                                .to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "slow".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "fast".to_owned(),
                            name: "__test_delay".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "fast".to_owned(),
                            json: r#"{"delay_ms":1,"result":"fast","synchronize":true}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "fast".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            ConcurrentProvider {
                turn: Mutex::new(0),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        let requested = events
            .iter()
            .flat_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { calls, .. } => {
                    calls.iter().map(|call| call.id).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let finished = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished, [requested[1], requested[0]]);
        assert!(matches!(
            requests.lock().unwrap()[1].messages()[2].content(),
            [
                ContentBlock::ToolResult { content, .. },
                ContentBlock::ToolResult {
                    content: second_content,
                    ..
                }
            ] if content == "slow" && second_content == "fast"
        ));

        let workspace = workspace::Workspace::open(directory.path()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = tools::test_executions_started();
        let execution = tokio::spawn(tools::execute(
            workspace,
            Arc::new(workspace::FileState::default()),
            "__test_delay".to_owned(),
            r#"{"delay_ms":500,"result":"late"}"#.to_owned(),
            Arc::clone(&cancelled),
            None,
        ));
        while tools::test_executions_started() == started {
            tokio::task::yield_now().await;
        }
        cancelled.store(true, Ordering::Release);
        let result = execution.await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
    }

    #[tokio::test]
    async fn mutating_calls_execute_sequentially_in_request_order() {
        struct MutatingTurnProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MutatingTurnProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "slow".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "slow".to_owned(),
                            json: r#"{"delay_ms":50,"result":"slow"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "slow".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "fast".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "fast".to_owned(),
                            json: r#"{"delay_ms":1,"result":"fast"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "fast".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MutatingTurnProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        // With the concurrent read path the fast call would finish first (as
        // the read-overlap test proves); a mutating turn must instead finish
        // in request order because side effects may not interleave.
        let events = runtime
            .run_loop(
                vec![Message::user("mutate twice")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    grants: approval::SessionGrants::default(),
                }),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        let requested = events
            .iter()
            .flat_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { calls, .. } => {
                    calls.iter().map(|call| call.id).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let finished = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished { id, result, .. } => Some((*id, result.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            finished.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            requested
        );
        assert_eq!(finished[0].1, "slow");
        assert_eq!(finished[1].1, "fast");
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_calls_stream_output_deltas_before_their_result() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        struct ShellProvider {
            turn: Mutex<usize>,
        }

        impl Provider for ShellProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "run".to_owned(),
                            name: "shell".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "run".to_owned(),
                            json: r#"{"command":"echo streamed-hello"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "run".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            ShellProvider {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_loop(
                vec![Message::user("run the command")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        let started = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .unwrap();
        let delta = events
            .iter()
            .position(|event| matches!(
                event,
                RuntimeEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("streamed-hello")
            ))
            .expect("shell output must stream as deltas");
        let finished = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::ToolCallFinished { result, is_error: false, .. }
                        if result.contains("streamed-hello") && result.ends_with("exit code: 0")
                )
            })
            .expect("the bounded result must follow the streamed output");
        assert!(started < delta && delta < finished);
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    #[tokio::test]
    async fn invalid_tool_argument_json_yields_a_tool_error_and_continues_the_run() {
        struct MalformedArgumentsProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MalformedArgumentsProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "bad".to_owned(),
                            name: "read_file".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "bad".to_owned(),
                            json: r#"{"path": "#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "bad".to_owned(),
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
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MalformedArgumentsProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { is_error: true, result, .. }
                if result.contains("not valid JSON")
        )));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: true,
            }] if call_id == "bad" && content.contains("not valid JSON")
        ));
    }

    #[tokio::test]
    async fn reports_the_underlying_workspace_open_error() {
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_in_workspace(
                RunCommand::new("hello"),
                PathBuf::from("/qq-test-missing-workspace"),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.as_slice(),
            [
                RunEvent::Started,
                RunEvent::Failed {
                    kind: RunFailureKind::InvalidCommand,
                    message,
                }
            ] if message.contains("could not open the workspace directory")
                && message.len() > "could not open the workspace directory: ".len()
        ));
    }

    #[tokio::test]
    async fn gate_less_runs_deny_mutating_tools_and_return_the_error_to_the_model() {
        struct MutatingProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MutatingProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "call_0".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_0".to_owned(),
                            json: "{}".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "call_0".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MutatingProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("mutate")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallDenied { message, .. }
                if message == approval::UNATTENDED_DENIED_RESULT
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        );
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        let requests = requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            }] if content == approval::UNATTENDED_DENIED_RESULT
        ));
    }

    #[tokio::test]
    async fn calls_with_malformed_arguments_short_circuit_without_consulting_the_gate() {
        struct RecordingGate {
            consulted: Arc<AtomicBool>,
        }

        impl ToolGate for RecordingGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                self.consulted.store(true, Ordering::Release);
                Box::pin(std::future::ready(GateDecision::Deny {
                    message: "the gate must not see unexecutable calls".to_owned(),
                }))
            }
        }

        struct MalformedMutatingProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MalformedMutatingProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "bad".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "bad".to_owned(),
                            json: r#"{"broken": "#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "bad".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let consulted = Arc::new(AtomicBool::new(false));
        let runtime = Runtime::new(
            MalformedMutatingProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        // Even though the tool is mutating and the gate would deny it, a call
        // with malformed arguments has nothing executable to approve: it must
        // return its argument error without an approval round trip.
        let events = runtime
            .run_loop(
                vec![Message::user("mutate")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(RecordingGate {
                    consulted: Arc::clone(&consulted),
                }),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(!consulted.load(Ordering::Acquire));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { is_error: true, result, .. }
                if result.contains("not valid JSON")
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    #[tokio::test]
    async fn fails_when_provider_completes_with_an_unfinished_tool_call() {
        struct ToolCallingProvider;

        impl Provider for ToolCallingProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_1".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let runtime = Runtime::new(ToolCallingProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::ActivityChanged {
                activity: RunActivity::PreparingToolCall
            }
        )));
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_provider_tool_metadata() {
        struct OversizedMetadataProvider;

        impl Provider for OversizedMetadataProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([Ok(ProviderEvent::ToolCallStarted {
                    id: "x".repeat(MAX_TOOL_CALL_ID_BYTES + 1),
                    name: "read_file".to_owned(),
                })]))
            }
        }

        let runtime = Runtime::new(OversizedMetadataProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn bounds_model_text_across_the_entire_tool_loop() {
        struct OversizedTextProvider;

        impl Provider for OversizedTextProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([Ok(ProviderEvent::OutputTextDelta {
                    text: "x".repeat(MAX_RUN_MODEL_TEXT_BYTES + 1),
                })]))
            }
        }

        let runtime = Runtime::new(OversizedTextProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::Policy,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn renews_the_internal_tool_budget_until_the_task_completes() {
        struct CompletesAfterCheckpoint {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
            emitted: Arc<Mutex<usize>>,
            checkpoint_at: Arc<Mutex<Option<usize>>>,
        }

        impl Provider for CompletesAfterCheckpoint {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                if request.tools().is_empty() {
                    let emitted = *self.emitted.lock().unwrap();
                    *self.checkpoint_at.lock().unwrap() = Some(emitted);
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "slice checkpoint".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }

                let mut emitted = self.emitted.lock().unwrap();
                let required = MAX_TOOL_CALLS_PER_SLICE + 1;
                if *emitted >= required {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "task complete".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }

                // Fifteen calls per provider turn deliberately leaves the
                // first slice at 255. A rollover implementation that checks
                // only after a turn would accept two more calls and overshoot
                // the 256-call ceiling.
                let first = *emitted;
                let count = (required - first).min(MAX_TOOL_CALLS_PER_TURN - 1);
                *emitted += count;
                drop(emitted);
                let mut events = Vec::with_capacity(count * 3 + 1);
                for index in first..first + count {
                    let id = format!("call-{index}");
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "unknown".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: "{}".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let emitted = Arc::new(Mutex::new(0));
        let checkpoint_at = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            CompletesAfterCheckpoint {
                requests: Arc::clone(&requests),
                emitted: Arc::clone(&emitted),
                checkpoint_at: Arc::clone(&checkpoint_at),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("finish a long task"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.last(), Some(&RunEvent::Completed));
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::OutputTextDelta { text } if text == "slice checkpoint"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::OutputTextDelta { text } if text == "task complete"
        )));
        assert_eq!(*emitted.lock().unwrap(), MAX_TOOL_CALLS_PER_SLICE + 1);
        assert_eq!(*checkpoint_at.lock().unwrap(), Some(255));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Completed))
                .count(),
            1
        );
        let requests = requests.lock().unwrap();
        let checkpoint_index = requests
            .iter()
            .position(|request| request.tools().is_empty())
            .unwrap();
        assert!(!requests[checkpoint_index + 1].tools().is_empty());
    }

    #[tokio::test]
    async fn enforces_the_per_turn_limit_and_checkpoint_request_contract() {
        struct TooManyInOneTurn;

        impl Provider for TooManyInOneTurn {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let events = (0..=MAX_TOOL_CALLS_PER_TURN)
                    .map(|index| {
                        Ok(ProviderEvent::ToolCallStarted {
                            id: format!("call-{index}"),
                            name: "read_file".to_owned(),
                        })
                    })
                    .collect::<Vec<Result<_, ProviderError>>>();
                Box::pin(stream::iter(events))
            }
        }

        let runtime = Runtime::new(TooManyInOneTurn, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                ..
            })
        ));

        struct EndlessToolTurns {
            turn: Mutex<usize>,
        }

        impl Provider for EndlessToolTurns {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let id = format!("call-{}", *turn);
                *turn += 1;
                drop(turn);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "unknown".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: "{}".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let runtime = Runtime::new(
            EndlessToolTurns {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        // A provider that emits a tool call on the tool-free checkpoint turn
        // is violating the request, not the run policy.
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                message,
            }) if message.contains("checkpoint turn")
        ));

        struct EmptyCheckpoint {
            turn: Mutex<usize>,
        }

        impl Provider for EmptyCheckpoint {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                if request.tools().is_empty() {
                    return Box::pin(stream::iter([Ok(ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 3,
                            cache_read_input_tokens: 1,
                            cache_write_input_tokens: 2,
                            output_tokens: 5,
                        }),
                    })]));
                }

                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                let mut events = Vec::with_capacity(MAX_TOOL_CALLS_PER_TURN * 3 + 1);
                for index in 0..MAX_TOOL_CALLS_PER_TURN {
                    let id = format!("empty-checkpoint-{current}-{index}");
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "unknown".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: "{}".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        let runtime = Runtime::new(
            EmptyCheckpoint {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 3,
                    cache_read_input_tokens: 1,
                    cache_write_input_tokens: 2,
                    output_tokens: 5,
                }
            }
        )));
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderResponse,
                message,
            }) if message == "provider returned an empty slice checkpoint"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn turns_provider_errors_into_failed_events() {
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: true,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderTransport,
                message,
            }) if message.contains("offline")
        ));
    }

    fn overloaded() -> ProviderError {
        ProviderError::Api {
            status: 503,
            message: "provider overloaded".to_owned(),
        }
    }

    /// Fails the first `failed_attempts` streams (with an error, or an empty
    /// stream when `failure` returns `None`), then streams text to completion.
    struct RecoveringProvider {
        calls: Arc<std::sync::atomic::AtomicU32>,
        failed_attempts: u32,
        failure: fn() -> Option<ProviderError>,
    }

    impl Provider for RecoveringProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call < self.failed_attempts {
                return match (self.failure)() {
                    Some(error) => Box::pin(stream::once(async move { Err(error) })),
                    None => Box::pin(stream::iter(Vec::new())),
                };
            }
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "recovered".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_provider_failures_and_completes() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            RecoveringProvider {
                calls: Arc::clone(&calls),
                failed_attempts: 2,
                failure: || Some(overloaded()),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RunEvent::Failed { .. })),
            "transient failures must not fail the run: {events:?}"
        );
        assert!(events.contains(&RunEvent::OutputTextDelta {
            text: "recovered".to_owned()
        }));
        assert_eq!(events.last(), Some(&RunEvent::Completed));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_a_stream_that_ends_without_a_terminal_event() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            RecoveringProvider {
                calls: Arc::clone(&calls),
                failed_attempts: 1,
                failure: || None,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(events.last(), Some(&RunEvent::Completed));
    }

    #[tokio::test]
    async fn does_not_retry_non_transient_provider_failures() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            RecoveringProvider {
                calls: Arc::clone(&calls),
                failed_attempts: u32::MAX,
                failure: || {
                    Some(ProviderError::Api {
                        status: 401,
                        message: "invalid key".to_owned(),
                    })
                },
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderAuthentication,
                ..
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_exhausting_transient_retry_attempts() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            RecoveringProvider {
                calls: Arc::clone(&calls),
                failed_attempts: u32::MAX,
                failure: || Some(overloaded()),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 8);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderUnavailable,
                message,
            }) if message.contains("8 attempts")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_after_visible_output_has_streamed() {
        struct MidStreamFailureProvider {
            calls: Arc<std::sync::atomic::AtomicU32>,
        }

        impl Provider for MidStreamFailureProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "partial".to_owned(),
                    }),
                    Err(overloaded()),
                ]))
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            MidStreamFailureProvider {
                calls: Arc::clone(&calls),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderUnavailable,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn disabled_retry_policy_fails_on_the_first_transient_error() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            RecoveringProvider {
                calls: Arc::clone(&calls),
                failed_attempts: u32::MAX,
                failure: || Some(overloaded()),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_retry_policy(TurnRetryPolicy::disabled());

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderUnavailable,
                ..
            })
        ));
    }

    struct MockMcpRegistry {
        specs: Vec<qq_provider::ToolSpec>,
        grants: Vec<String>,
        calls: Arc<Mutex<Vec<(String, String)>>>,
        result: McpToolResult,
    }

    impl MockMcpRegistry {
        fn returning(result: McpToolResult) -> Self {
            Self {
                specs: vec![qq_provider::ToolSpec::new(
                    "mcp__srv__ping",
                    "Ping the fixture server.",
                    serde_json::json!({"type": "object"}),
                )],
                grants: Vec::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
                result,
            }
        }
    }

    impl McpRegistry for MockMcpRegistry {
        fn tool_specs(&self) -> McpSpecsFuture {
            let specs = self.specs.clone();
            Box::pin(async move { specs })
        }

        fn config_grants(&self) -> Vec<String> {
            self.grants.clone()
        }

        fn call(
            &self,
            name: String,
            arguments: String,
            _cancelled: Arc<AtomicBool>,
        ) -> McpCallFuture {
            self.calls.lock().unwrap().push((name, arguments.clone()));
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    /// Scripts one `mcp__srv__ping` call on the first turn, then completes.
    struct McpCallProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for McpCallProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "mcp__srv__ping".to_owned(),
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
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    #[tokio::test]
    async fn merges_mcp_declarations_and_dispatches_granted_calls_to_the_registry() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = MockMcpRegistry::returning(McpToolResult {
            content: "pong".to_owned(),
            is_error: false,
        });
        // A spec that violates the namespace contract must be discarded.
        registry.specs.push(qq_provider::ToolSpec::new(
            "rogue_tool",
            "not namespaced",
            serde_json::json!({"type": "object"}),
        ));
        // The configuration allowlist covers the call, so gate-less Ask mode
        // executes it without an approval round trip.
        registry.grants = vec!["mcp__srv__ping".to_owned()];
        let calls = Arc::clone(&registry.calls);
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("ping")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: false, .. } if result == "pong"
        )));
        let requests = requests.lock().unwrap();
        let names = requests[0]
            .tools()
            .iter()
            .map(qq_provider::ToolSpec::name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"mcp__srv__ping"));
        assert!(
            !names.contains(&"rogue_tool"),
            "specs outside the mcp__ namespace must be discarded"
        );
        assert_eq!(requests[0].tools().len(), 7);
        let system = requests[0].system().unwrap();
        assert!(system.contains("mcp__srv__ping"));
        assert!(system.contains("MCP servers"));
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: false,
            }] if call_id == "call_0" && content == "pong"
        ));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [("mcp__srv__ping".to_owned(), r#"{"value":1}"#.to_owned())]
        );
    }

    #[tokio::test]
    async fn mcp_failures_are_tool_errors_and_results_are_truncated() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let registry = MockMcpRegistry::returning(McpToolResult {
            content: "the server exploded".to_owned(),
            is_error: true,
        });
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));
        let events = runtime
            .run_loop(
                vec![Message::user("ping")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result == "the server exploded"
        )));
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed)),
            "an MCP failure must never fail the run"
        );

        let oversized = "x".repeat(tools::MAX_TOOL_RESULT_BYTES + 1024);
        let registry = MockMcpRegistry::returning(McpToolResult {
            content: oversized,
            is_error: false,
        });
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));
        let events = runtime
            .run_loop(
                vec![Message::user("ping")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        let result = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ToolCallFinished { result, .. } => Some(result.clone()),
                _ => None,
            })
            .expect("the oversized MCP result must still finish");
        assert!(result.len() <= tools::MAX_TOOL_RESULT_BYTES);
        assert!(result.contains("truncated by qq"));
    }

    #[tokio::test]
    async fn ungranted_mcp_calls_are_denied_unattended_and_unknown_names_error() {
        let directory = tempfile::tempdir().unwrap();
        let registry = MockMcpRegistry::returning(McpToolResult {
            content: "pong".to_owned(),
            is_error: false,
        });
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));
        // No configuration grant covers the call: gate-less Ask mode denies
        // without executing, and the run still completes.
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("ping")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallDenied { message, .. }
                if message == approval::UNATTENDED_DENIED_RESULT
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));

        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        // Without a registry, an approved mcp__ call falls through to the
        // built-in dispatcher's precise unknown-tool error.
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop(
                vec![Message::user("ping")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result.contains("unknown tool")
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    /// Scripts one `spawn_agent` call on the first turn, then completes.
    struct SpawnCallProvider {
        turn: Mutex<usize>,
        model: Option<&'static str>,
    }

    impl Provider for SpawnCallProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: self.model.map_or_else(
                            || r#"{"task":"count the widgets"}"#.to_owned(),
                            |model| format!(r#"{{"task":"count the widgets","model":"{model}"}}"#),
                        ),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
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

    type SpawnedTasks = Arc<Mutex<Vec<(String, Option<String>)>>>;

    struct StubSpawner {
        outcome: SpawnAgentOutcome,
        tasks: SpawnedTasks,
    }

    impl SubagentSpawner for StubSpawner {
        fn spawn(&self, task: String, model: Option<String>) -> SpawnAgentFuture {
            self.tasks.lock().unwrap().push((task, model));
            let outcome = self.outcome.clone();
            Box::pin(std::future::ready(outcome))
        }
    }

    #[tokio::test]
    async fn spawner_less_runs_neither_declare_nor_dispatch_spawn_agent() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));

        struct CapturingSpawnProvider {
            inner: SpawnCallProvider,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for CapturingSpawnProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                self.inner.stream(request)
            }
        }

        let runtime = Runtime::new(
            CapturingSpawnProvider {
                inner: SpawnCallProvider {
                    turn: Mutex::new(0),
                    model: None,
                },
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        // run_messages_in_workspace passes no spawner: the tool must be
        // absent from the declarations and rejected by dispatch.
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result == SPAWN_UNAVAILABLE_RESULT
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        let requests = requests.lock().unwrap();
        assert!(
            !requests[0]
                .tools()
                .iter()
                .any(|spec| spec.name() == tools::SPAWN_AGENT_TOOL)
        );
        let system = requests[0].system().unwrap();
        assert!(!system.contains("Delegation:"));
    }

    #[tokio::test]
    async fn spawner_runs_declare_the_tool_and_truncate_oversized_child_answers() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let spawner = Arc::new(StubSpawner {
            outcome: SpawnAgentOutcome {
                content: "x".repeat(tools::MAX_TOOL_RESULT_BYTES + 1024),
                is_error: false,
            },
            tasks: Arc::clone(&tasks),
        });
        let runtime = Runtime::new(
            SpawnCallProvider {
                turn: Mutex::new(0),
                model: None,
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("go")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(Some(spawner)),
            )
            .collect::<Vec<_>>()
            .await;

        let result = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ToolCallFinished {
                    result,
                    is_error: false,
                    ..
                } => Some(result.clone()),
                _ => None,
            })
            .expect("the spawn call must finish successfully");
        assert!(result.len() <= tools::MAX_TOOL_RESULT_BYTES);
        assert!(result.contains("truncated by qq"));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert_eq!(
            tasks.lock().unwrap().as_slice(),
            [("count the widgets".to_owned(), None)]
        );
    }

    #[tokio::test]
    async fn spawn_model_overrides_are_normalized_and_forwarded_to_the_spawner() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        let cases = [
            (None, None),
            (Some(""), None),
            (Some("   "), None),
            (Some("openai-codex/gpt-test"), Some("openai-codex/gpt-test")),
            // Routes outside the advertised schema list still reach the
            // spawner: the session layer validates every resolved route
            // against the served model list at spawn time, so a discovered
            // model absent from the advertised list stays spawnable.
            (Some("openai/gpt-guessed"), Some("openai/gpt-guessed")),
        ];
        for (requested, expected) in cases {
            let directory = tempfile::tempdir().unwrap();
            let tasks = Arc::new(Mutex::new(Vec::new()));
            let spawner = Arc::new(StubSpawner {
                outcome: SpawnAgentOutcome {
                    content: "child answer".to_owned(),
                    is_error: false,
                },
                tasks: Arc::clone(&tasks),
            });
            let runtime = Runtime::new(
                SpawnCallProvider {
                    turn: Mutex::new(0),
                    model: requested,
                },
                "gpt-test",
                256,
            )
            .unwrap()
            .with_spawn_model_routes(vec!["openai-codex/gpt-test".to_owned()]);

            let _events = runtime
                .run_loop_with_spawner(
                    vec![Message::user("go")],
                    directory.path().to_owned(),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AllowAllGate),
                    Arc::new(workspace::FileState::default()),
                    RunCapabilities::user(Some(spawner)),
                )
                .collect::<Vec<_>>()
                .await;

            assert_eq!(
                tasks.lock().unwrap().as_slice(),
                &[("count the widgets".to_owned(), expected.map(str::to_owned))]
            );
        }
    }

    #[test]
    fn agent_prompt_teaches_delegation_only_when_spawn_agent_is_declared() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let without = agent_system_prompt(workspace, &tools::specs(), &instructions, None);
        assert!(!without.contains("spawn_agent"));
        assert!(!without.contains("Delegation:"));

        let mut specs = tools::specs();
        specs.push(tools::spawn_agent_spec(&[]));
        let with = agent_system_prompt(workspace, &specs, &instructions, None);
        assert!(with.contains("spawn_agent"));
        assert!(with.contains("Delegation:"));
        assert!(with.contains("independent questions"));
        assert!(with.contains("read-only sub-agent"));
        assert!(with.contains("Omit spawn_agent's model argument by default"));
        assert!(with.contains("configured worker model"));
        assert!(with.contains("persisted selected model"));
        assert!(with.contains("never guess, translate, or invent one"));
    }
}
