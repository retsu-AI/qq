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
    ApprovalMode, BudgetLimitKind, ContentHash, DelegationRoster, ModelPricing, RunActivity,
    RunCommand, RunEvent, RunFailureKind, RunLimits, RunPromptIdentity, TokenUsage, ToolCallId,
};
use qq_provider::{
    ContentBlock, Message, ModelRequest, Provider, ProviderErrorKind, ProviderEvent, Role, ToolSpec,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod approval;
pub mod catalog;
pub mod context_source;
pub mod hosts;
mod input;
pub mod plan;
mod runtime;
mod sessions;
mod tools;
mod workspace;

use runtime::{
    AGENT_PROMPT_VERSION, BUDGET_FINAL_RESPONSE_NOTICE, BudgetDecision, BudgetMeter, GateDecision,
    HistorySearcher, PendingToolCall, PreparedRequestWeight, PreparedStaticPrefix, RuntimeEvent,
    RuntimeToolCall, SPAWN_UNAVAILABLE_RESULT, SearchHistoryArgs, SpawnAgentFuture,
    SpawnAgentOutcome, SpawnAgentSpend, SpawnRequest, SubagentSpawner, ToolGate, ToolGateFuture,
    TurnBlock, agent_system_prompt, render_history_matches, tool_schema_measurement,
};

pub use approval::shell_prefix_matches;
pub use context_source::{
    ContextBudget, ContextBundle, ContextCache, ContextFetchFuture, ContextItem, ContextRequest,
    ContextSource, ContextSourceError, FailPolicy, MAX_CONTEXT_SOURCES,
};
pub use hosts::{
    EMBEDDED_TOOL_PREFIX, EmbeddedHostError, EmbeddedToolFuture, EmbeddedToolHandler,
    EmbeddedToolHost, EmbeddedToolHostBuilder, ExternalToolHost, HostCallError, HostCallFuture,
    HostCatalog, HostReadiness, HostShutdownFuture, HostTool, HostToolResult, MCP_TOOL_PREFIX,
    ToolHints,
};
pub use runtime::{
    AUDIT_TOOL_CALL_THRESHOLD, AuditMode, AuditPolicy, AuditRequest, AuditVerdict, AuditedAction,
    MAX_AUDIT_ACTION_BYTES, MAX_AUDIT_ANSWER_BYTES, MAX_AUDIT_FINDING_BYTES, MAX_AUDIT_FINDINGS,
    MAX_PENDING_STEERING,
};
pub use sessions::{
    ApprovalReviewer, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, MAX_CHILD_DEPTH,
    MAX_CHILD_DEPTH_CEILING, MAX_CONCURRENT_CHILDREN_PER_RUN, MAX_DELEGATION_ROSTER,
    MAX_DESCENDANTS_PER_ROOT, MAX_PENDING_PROMPTS, MAX_REPLAY_EVENTS, MAX_REVIEW_ARGUMENT_BYTES,
    MAX_REVIEW_BRIEF_BYTES, MAX_REVIEW_RECENT_ACTIONS, MAX_SPAWNED_CHILDREN_PER_RUN,
    PublishedEvent, PublishedEventStream, RecentAction, ReviewDecision, ReviewFuture, ReviewOrigin,
    ReviewRequest, ReviewSpend, ReviewVerdict, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError,
    SessionRuntimeOptions, SpawnModelValidationFuture, WorkerRuntimeLoadFuture,
    WorkspaceGrantAuthority, WorkspaceGrantSeed, run_cost,
};
pub use workspace::skills::{MAX_INDEXED_SKILLS, MAX_SKILL_DESCRIPTION_BYTES};
pub use workspace::{SkillEntry, SkillIndex, SkillKind};

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
const SHELL_OUTPUT_QUEUE_CAPACITY: usize = 16;
const CONTEXT_MESSAGE_FRAMING_BYTES: u64 = 16;
const CONTEXT_BLOCK_FRAMING_BYTES: u64 = 16;
/// Sent to the model when an interrupt left the transcript ending on an
/// assistant message with no steering to inject.
const INTERRUPT_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe previous \
turn was interrupted by the user. Continue from where it stopped.";
const INTERRUPTED_TOOL_RESULT: &str =
    "Tool execution was interrupted before a durable result was recorded.";
/// Most times one run resumes a turn the provider cut at its output token
/// limit. The cap keeps a model that re-emits the same prefix from spending
/// the whole budget; the typed failure names it.
pub const MAX_OUTPUT_CONTINUATIONS: u16 = 3;
/// Sent after a truncated turn is committed so the model resumes rather than
/// restarts. Assistant/user alternation is preserved because the partial
/// assistant message precedes it.
pub(crate) const OUTPUT_TRUNCATED_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe \
previous response was cut off at the output token limit. Continue exactly from where it \
stopped; do not repeat what was already written.";

enum StreamStep<T> {
    Interrupted,
    Event(Option<T>),
}

/// Resolves when an interrupting steer newer than `handled` arrives; pending
/// forever for runs without steering.
async fn interrupt_requested(steering: &mut Option<runtime::SteeringReceiver>, handled: u64) {
    match steering {
        Some(steering) => loop {
            if *steering.interrupts.borrow() > handled {
                return;
            }
            if steering.interrupts.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        },
        None => std::future::pending().await,
    }
}

/// Drains every steering message that is ready and appends each as a user
/// message. Returns the ids applied, in order, or `None` when nothing was
/// pending. Never waits: steering that arrives after this point waits for
/// the next boundary.
fn apply_steering(
    steering: &mut Option<runtime::SteeringReceiver>,
    messages: &mut Vec<Message>,
    irreducible_message_bytes: &mut u64,
    _turn_ordinal: u16,
) -> Option<Vec<qq_protocol::MessageId>> {
    let steering = steering.as_mut()?;
    let mut applied = Vec::new();
    while let Ok(message) = steering.messages.try_recv() {
        let user = Message::user(message.text);
        *irreducible_message_bytes =
            irreducible_message_bytes.saturating_add(measure_message(&user));
        messages.push(user);
        applied.push(message.message_id);
    }
    (!applied.is_empty()).then_some(applied)
}

/// Executes one `select_tools` call against the run's pin set. Returns the
/// bounded tool result and whether any pin was added.
/// Resolves a `spawn_agent` call's model choice against the roster. Precedence:
/// an explicit `model` (which must be a roster route when a roster exists),
/// then the requested `role`, then the roster's default role. `None` means
/// "no roster and no override": the session layer falls back to the legacy
/// worker model or the parent's selection. Errors are tool results the model
/// can act on.
/// Records one finished tool call for the audit trigger and, when a hook will
/// read it, the auditor's action summary. Bounded: the summary keeps names
/// and targets only and stops growing at `MAX_AUDIT_ACTION_BYTES`.
fn note_audited_action(
    triggers: &mut runtime::AuditTriggers,
    actions: &mut Vec<runtime::AuditedAction>,
    keep_actions: bool,
    call: &RuntimeToolCall,
    result: &tools::ToolExecutionResult,
) {
    triggers.tool_calls = triggers.tool_calls.saturating_add(1);
    match approval::classify(call.effect, &call.name, &call.arguments) {
        approval::ToolClass::Mutating if !result.is_error => triggers.mutated_files = true,
        // Read-only shell (allowlisted VCS reads and the like) does not by
        // itself make a run worth auditing; anything else does.
        approval::ToolClass::Shell { command, .. }
            if !result.is_error && !approval::read_only_shell_command(&command) =>
        {
            triggers.non_read_shell = true;
        }
        _ => {}
    }
    if call.name == tools::SPAWN_AGENT_TOOL && !result.is_error {
        triggers.spawned_children = true;
    }
    if !keep_actions {
        return;
    }
    let target = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|arguments| {
            arguments
                .get("path")
                .or_else(|| arguments.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(|target| bounded_text(target, 200))
        });
    let bytes: usize = actions
        .iter()
        .map(|action| action.tool.len() + action.target.as_ref().map_or(0, String::len) + 8)
        .sum();
    if bytes < runtime::MAX_AUDIT_ACTION_BYTES {
        actions.push(runtime::AuditedAction {
            tool: call.name.clone(),
            target,
            is_error: result.is_error,
        });
    }
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

fn resolve_delegation_route(
    delegation: &DelegationRoster,
    model: Option<String>,
    role: Option<qq_protocol::DelegationRole>,
) -> Result<Option<String>, String> {
    if delegation.roster.is_empty() {
        if role.is_some() {
            return Err(
                "no delegation roster is configured, so role cannot be used; omit role \
                        (and model) to use the configured worker model"
                    .to_owned(),
            );
        }
        return Ok(model);
    }
    if let Some(model) = model {
        if !delegation.contains_route(&model) {
            return Err(format!(
                "model {model:?} is not on the delegation roster; choose a role instead or use \
                 one of the listed routes"
            ));
        }
        return Ok(Some(model));
    }
    let role = role.unwrap_or(delegation.default_role);
    match delegation.route_for_role(role) {
        Some(route) => Ok(Some(route.to_owned())),
        None => Err(format!(
            "no roster entry declares the {} role; choose one of the roles listed in the \
             system prompt",
            role.as_str()
        )),
    }
}

fn select_tools(
    catalog: &catalog::ToolCatalog,
    pins: &mut catalog::PinSet,
    arguments: &str,
) -> (tools::ToolExecutionResult, bool) {
    let arguments = match serde_json::from_str::<catalog::SelectToolsArgs>(arguments) {
        Ok(arguments) if arguments.query.trim().is_empty() => {
            return (
                tools::bounded_result("query must not be empty".to_owned(), true),
                false,
            );
        }
        Ok(arguments) => arguments,
        Err(error) => {
            return (
                tools::bounded_result(format!("invalid arguments: {error}"), true),
                false,
            );
        }
    };
    if catalog.exposure() != catalog::Exposure::Full && catalog.external_len() == 0 {
        return (
            tools::bounded_result("no external tools are available".to_owned(), true),
            false,
        );
    }
    let limit = arguments.limit.clamp(1, catalog::MAX_SELECT_MATCHES);
    let matches = catalog.rank(&arguments.query, pins, limit);
    let mut pinned = Vec::new();
    let mut refused = Vec::new();
    for entry in matches {
        if pins.pin(entry.spec.name()) {
            pinned.push(entry.spec.name().to_owned());
        } else {
            refused.push(entry.spec.name().to_owned());
        }
    }
    let changed = !pinned.is_empty();
    let result = catalog::SelectToolsResult {
        pinned,
        already_pinned: pins
            .names()
            .iter()
            .filter(|name| {
                let lower = arguments.query.to_ascii_lowercase();
                name.to_ascii_lowercase().contains(lower.trim())
            })
            .cloned()
            .collect(),
        refused,
        remaining_pin_slots: catalog::MAX_PINNED_TOOLS.saturating_sub(pins.len()),
    };
    let content = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_owned());
    (tools::bounded_result(content, false), changed)
}

/// Re-pins every tool an earlier `select_tools` result in `messages` pinned,
/// so a recovered run offers the schemas the model already selected. Only
/// names the catalog still holds are pinned.
fn recover_pins(messages: &[Message], catalog: &catalog::ToolCatalog, pins: &mut catalog::PinSet) {
    let mut select_call_ids = std::collections::HashSet::new();
    for message in messages {
        for block in message.content() {
            match block {
                ContentBlock::ToolCall { id, name, .. } if name == catalog::SELECT_TOOLS_TOOL => {
                    select_call_ids.insert(id.as_str());
                }
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error: false,
                } if select_call_ids.contains(call_id.as_str()) => {
                    if let Ok(result) = serde_json::from_str::<catalog::SelectToolsResult>(content)
                    {
                        for name in result.pinned {
                            if catalog.lookup(&name).is_some_and(|entry| {
                                matches!(entry.host, catalog::ToolHost::External { .. })
                            }) {
                                pins.pin(&name);
                            }
                        }
                    }
                }
                ContentBlock::Text { .. }
                | ContentBlock::ToolCall { .. }
                | ContentBlock::ToolResult { .. } => {}
            }
        }
    }
}

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
    allow_tools: bool,
    /// The session's policy is `ReadOnly`: mutating, shell, and non-read
    /// external schemas are withheld from the request instead of offered and
    /// denied. Policy still evaluates every call; this only saves the tokens.
    read_only: bool,
    max_output_tokens: Option<u32>,
    /// Caller-imposed budgets and the pricing that makes the cost bound
    /// measurable. Admission rejects a cost cap without pricing before this
    /// struct is built.
    limits: RunLimits,
    pricing: Option<ModelPricing>,
    /// Full-transcript recall for `search_history`. Session runs install one;
    /// direct runs have no durable history to search.
    history: Option<Arc<dyn HistorySearcher>>,
    /// Steering input from the session layer. Direct runs have none.
    steering: Option<runtime::SteeringReceiver>,
    /// Audits the candidate final answer of a root run. Session roots install
    /// one; children, internal runs, and direct runs have none.
    audit_hook: Option<Arc<dyn runtime::AuditHook>>,
    tool_tasks: Option<tools::ToolTasks>,
}

impl RunCapabilities {
    pub(crate) fn user(spawner: Option<Arc<dyn SubagentSpawner>>) -> Self {
        Self {
            spawner,
            allow_guidance: true,
            slash_is_literal: false,
            allow_tools: true,
            read_only: false,
            max_output_tokens: None,
            limits: RunLimits::default(),
            pricing: None,
            history: None,
            steering: None,
            audit_hook: None,
            tool_tasks: None,
        }
    }

    /// Marks a leading slash as already normalized from the durable `//`
    /// escape. The message is provider-ready and must not be reinterpreted as
    /// a guidance invocation.
    pub(crate) fn with_literal_slash(mut self, literal: bool) -> Self {
        self.slash_is_literal = literal;
        self
    }

    pub(crate) fn without_tools(mut self) -> Self {
        self.allow_tools = false;
        self
    }

    /// Withholds schemas the `ReadOnly` policy would deny anyway.
    pub(crate) fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub(crate) fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub(crate) fn with_limits(mut self, limits: RunLimits, pricing: Option<ModelPricing>) -> Self {
        self.limits = limits;
        self.pricing = pricing;
        self
    }

    pub(crate) fn with_history(mut self, history: Arc<dyn HistorySearcher>) -> Self {
        self.history = Some(history);
        self
    }

    pub(crate) fn with_steering(mut self, steering: runtime::SteeringReceiver) -> Self {
        self.steering = Some(steering);
        self
    }

    pub(crate) fn with_tool_tasks(mut self, tasks: tools::ToolTasks) -> Self {
        self.tool_tasks = Some(tasks);
        self
    }

    pub(crate) fn with_audit_hook(mut self, hook: Arc<dyn runtime::AuditHook>) -> Self {
        self.audit_hook = Some(hook);
        self
    }

    /// Installs a spawner on a restricted run: a model-authored child task at a
    /// depth the roster still permits to delegate.
    pub(crate) fn with_spawner(mut self, spawner: Arc<dyn SubagentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    pub(crate) const fn restricted() -> Self {
        Self {
            spawner: None,
            allow_guidance: false,
            slash_is_literal: false,
            allow_tools: true,
            read_only: false,
            max_output_tokens: None,
            limits: RunLimits {
                max_duration_ms: None,
                max_model_turns: None,
                max_tool_calls: None,
                max_total_tokens: None,
                max_cost_usd_nanos: None,
                max_input_tokens: None,
                max_output_tokens: None,
                max_tool_output_bytes: None,
                max_children: None,
                max_concurrent_children: None,
            },
            pricing: None,
            history: None,
            steering: None,
            audit_hook: None,
            tool_tasks: None,
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
        let class = approval::classify(call.effect, &call.name, &call.arguments);
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
    context_window: Option<u32>,
    /// External tool hosts in contribution order. A compiled plan snapshots
    /// their catalogs; direct runs snapshot them per run.
    pub(crate) hosts: Arc<[Arc<dyn ExternalToolHost>]>,
    /// Pre-turn context sources in registration order, with the shared cache.
    pub(crate) context_sources: Arc<[context_source::RegisteredSource]>,
    pub(crate) context_cache: Arc<ContextCache>,
    spawn_model_routes: Arc<[String]>,
    /// The roster and bounds `spawn_agent` advertises and resolves through.
    pub(crate) delegation: Arc<DelegationRoster>,
    /// When a root run's final answer is audited before completion.
    pub(crate) audit: runtime::AuditPolicy,
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
            context_window: None,
            hosts: Arc::from([]),
            context_sources: Arc::from([]),
            context_cache: Arc::new(ContextCache::default()),
            spawn_model_routes: Arc::from([]),
            delegation: Arc::new(DelegationRoster::default()),
            audit: runtime::AuditPolicy::default(),
        })
    }

    /// Supplies the effective model context window for provider-neutral
    /// request planning. `None` retains the independent storage backstop.
    #[must_use]
    pub fn with_context_window(mut self, context_window: Option<u32>) -> Self {
        self.context_window = context_window;
        self
    }

    /// The resolved-model account of a runtime constructed without
    /// configuration: identity comes from the runtime itself, and every
    /// capability the embedder did not declare is recorded as unsupported or
    /// unknown rather than guessed.
    pub(crate) fn embedded_resolved_model(&self) -> qq_protocol::ResolvedModel {
        qq_protocol::ResolvedModel {
            version: qq_protocol::ResolvedModelVersion::new(1)
                .expect("resolved-model version one is non-zero"),
            request_shape: None,
            route: format!("embedded/{}", self.model),
            provider_model: self.model.to_string(),
            organization: None,
            credential_profile: None,
            max_output_tokens: self.max_output_tokens,
            context_window: self.context_window,
            pricing: None,
            output_token_control: qq_protocol::CapabilitySupport::Unsupported,
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

    /// Attaches an external tool host. Its catalog is snapshotted when a plan
    /// compiles from this runtime and its tools dispatch to it by name.
    #[must_use]
    pub fn with_tool_host(mut self, host: Arc<dyn ExternalToolHost>) -> Self {
        let mut hosts = self.hosts.to_vec();
        hosts.push(host);
        self.hosts = hosts.into();
        self
    }

    /// Registers a bounded pre-turn context source. Sources are consulted
    /// once per run, concurrently, before the first provider request; at
    /// most [`MAX_CONTEXT_SOURCES`] may be registered and later ones are
    /// ignored with no effect on the run.
    #[must_use]
    pub fn with_context_source(mut self, source: Arc<dyn ContextSource>) -> Self {
        if self.context_sources.len() >= MAX_CONTEXT_SOURCES {
            return self;
        }
        let mut sources = self.context_sources.to_vec();
        sources.push(context_source::RegisteredSource::new(source));
        self.context_sources = sources.into();
        self
    }

    /// Replaces the shared context cache (bounds are the embedder's call).
    #[must_use]
    pub fn with_context_cache(mut self, cache: Arc<ContextCache>) -> Self {
        self.context_cache = cache;
        self
    }

    fn config_grants(&self) -> std::collections::HashSet<String> {
        self.hosts
            .iter()
            .flat_map(|host| host.config_grants())
            .collect()
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

    /// Installs the delegation roster the run loop advertises to the model.
    #[must_use]
    pub fn with_delegation(mut self, delegation: DelegationRoster) -> Self {
        self.delegation = Arc::new(delegation);
        self
    }

    /// Sets when a root run's final answer is audited before completion.
    #[must_use]
    pub const fn with_audit(mut self, audit: runtime::AuditPolicy) -> Self {
        self.audit = audit;
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
            self.run_messages_in_workspace(
                vec![Message::user(input::render_text(command.input()))],
                workspace,
            ),
            self.context_window,
        )
    }

    /// Runs a multi-turn model/tool loop with explicit prior conversation context.
    pub fn run_messages(&self, messages: Vec<Message>) -> RunStream {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        public_run_stream(
            self.run_messages_in_workspace(messages, workspace),
            self.context_window,
        )
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
            tools: self.config_grants(),
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

    /// Runs the loop from a runtime that was not compiled into a plan: the
    /// workspace is canonicalized and opened, and instructions are read, for
    /// this run only. Durable sessions compile once and call
    /// [`CompiledAgentPlan::execute`] directly.
    pub(crate) fn run_loop_with_spawner(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
        capabilities: RunCapabilities,
    ) -> RuntimeStream {
        let runtime = self.clone();
        Box::pin(stream! {
            let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancelled));
            yield RuntimeEvent::Started;
            let (opened, _instructions) = match workspace::prepare_workspace(
                workspace,
                Arc::clone(&cancelled),
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
                Err(error) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    };
                    return;
                }
            };
            // Compilation re-opens the workspace and re-reads instructions on
            // a blocking thread; the direct path pays this once per run, the
            // same filesystem work it always did.
            let profile = plan::AgentProfile::embedded(&runtime, opened.path().to_owned());
            let compiled = match tokio::task::spawn_blocking(move || {
                plan::CompiledAgentPlan::compile_blocking(profile)
            })
            .await
            {
                Ok(Ok(plan)) => plan,
                Ok(Err(error)) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    };
                    return;
                }
                Err(_) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Server,
                        message: "plan compilation stopped unexpectedly".to_owned(),
                    };
                    return;
                }
            };
            drop(opened);
            let mut events = compiled.execute(messages, cancelled, gate, file_state, capabilities);
            while let Some(event) = events.next().await {
                match event {
                    // The wrapper already announced the start.
                    RuntimeEvent::Started => {}
                    event => yield event,
                }
            }
        })
    }
}

impl plan::CompiledAgentPlan {
    /// Runs one command from this plan with read-only tools scoped to the
    /// plan's workspace, the direct (non-durable) counterpart of a session
    /// run. Configuration allowlists are the only grants.
    pub fn run(self: &Arc<Self>, command: RunCommand) -> RunStream {
        let grants = approval::SessionGrants {
            tools: self.runtime.config_grants(),
            shell_prefixes: Vec::new(),
        };
        public_run_stream(
            self.execute(
                vec![Message::user(input::render_text(command.input()))],
                Arc::new(AtomicBool::new(false)),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants,
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None),
            ),
            self.runtime.context_window,
        )
    }

    /// Executes one run from this plan. No filesystem discovery happens
    /// before the first provider request unless the prompt invokes a command
    /// or skill, whose document is then read from the already opened
    /// workspace.
    pub(crate) fn execute(
        self: &Arc<Self>,
        mut messages: Vec<Message>,
        cancelled: Arc<AtomicBool>,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
        capabilities: RunCapabilities,
    ) -> RuntimeStream {
        let plan = Arc::clone(self);
        let provider = Arc::clone(&plan.runtime.provider);
        let model = Arc::clone(&plan.runtime.model);
        let model_max_output_tokens = plan.runtime.max_output_tokens;
        let catalog = Arc::clone(&plan.catalog);
        let skills = Arc::clone(&plan.skills);
        let roster_text = plan.roster_text.clone();
        let pack_roots = Arc::clone(&plan.pack_roots);
        let persona = plan.persona.clone();
        let hosts = Arc::clone(&plan.hosts);
        let context_sources = Arc::clone(&plan.runtime.context_sources);
        let context_cache = Arc::clone(&plan.runtime.context_cache);
        let profile_name = plan.descriptor().profile.as_str().to_owned();
        let delegation = Arc::clone(&plan.runtime.delegation);
        Box::pin(stream! {
            let RunCapabilities {
                spawner,
                allow_guidance,
                slash_is_literal,
                allow_tools,
                read_only,
                max_output_tokens,
                limits,
                pricing,
                history,
                steering,
                audit_hook,
                tool_tasks,
            } = capabilities;
            let tool_tasks = tool_tasks.unwrap_or_default();
            let mut steering = steering;
            let mut handled_interrupt = steering
                .as_ref()
                .map_or(0, |steering| *steering.interrupts.borrow());
            let max_output_tokens = max_output_tokens
                .unwrap_or(model_max_output_tokens)
                .min(model_max_output_tokens);
            // The wall clock starts at admission, before workspace preparation
            // and provider selection, so caller time bounds mean what they say.
            let mut budget = BudgetMeter::new(limits, pricing, tokio::time::Instant::now());
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

            let workspace = plan.workspace.clone();
            let workspace_instructions = &plan.instructions;
            let selected_guidance = match parsed_invocation.guidance {
                None => None,
                Some(request) => match workspace::prepare_guidance(
                    workspace.clone(),
                    Arc::clone(&pack_roots),
                    Arc::clone(&skills),
                    Arc::clone(&cancelled),
                    request,
                )
                .await
                {
                    Ok(guidance) => Some(guidance),
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
                },
            };
            // Context sources run once, after guidance and before any
            // provider work, each under its own deadline. Their output is
            // appended to this run's system prompt only; nothing durable
            // changes. A fail-closed failure settles the run here.
            let mut context_blocks = String::new();
            let mut context_records = Vec::new();
            if !context_sources.is_empty() {
                let latest_user_text = messages
                    .last()
                    .filter(|message| message.role() == Role::User)
                    .map(|message| {
                        message
                            .content()
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. } => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let request = context_source::ContextRequest {
                    profile: profile_name.clone(),
                    workspace: workspace.path().display().to_string(),
                    latest_user_text,
                    budget: context_source::ContextBudget::default(),
                };
                match context_source::fetch_all(
                    &context_sources,
                    &context_cache,
                    request,
                    Arc::clone(&cancelled),
                )
                .await
                {
                    Ok(rendered) => {
                        for context in rendered {
                            context_blocks.push_str(&context.text);
                            context_records.push(context.record);
                        }
                    }
                    Err((message, record)) => {
                        context_records.push(record);
                        yield RuntimeEvent::Failed {
                            kind: RunFailureKind::ContextSource,
                            message,
                        };
                        return;
                    }
                }
            }
            // The plan's catalog is the tool list: the static tools this run
            // may use (the sub-agent tool only when it may spawn, recall only
            // for durable session runs) plus every external tool under full
            // exposure. Under progressive exposure the model pins external
            // tools with `select_tools`; pins extend this base list.
            let base_specs: Arc<[ToolSpec]> = if allow_tools {
                catalog.base_specs(&catalog::StaticFilter {
                    spawn_agent: spawner.is_some(),
                    search_history: history.is_some(),
                    load_skill: allow_guidance,
                    read_only,
                })
            } else {
                Arc::from([])
            };
            let mut pins = catalog::PinSet::default();
            // A recovered run re-pins what its earlier `select_tools` calls
            // pinned, so the resumed request offers the same schemas.
            if allow_tools && catalog.exposure() == catalog::Exposure::Progressive {
                recover_pins(&messages, &catalog, &mut pins);
            }
            let mut tool_specs: Arc<[ToolSpec]> = if pins.is_empty() {
                Arc::clone(&base_specs)
            } else {
                catalog.specs_with_pins(&base_specs, &pins)
            };
            let system: Arc<str> = Arc::from({
                let mut system = agent_system_prompt(
                    workspace.path(),
                    &base_specs,
                    runtime::PromptSections {
                        tool_index: catalog.index_text().map(Arc::as_ref),
                        roster: roster_text.as_deref(),
                        // Disclosure follows the guidance capability: restricted
                        // runs (compaction, model-authored child tasks) neither
                        // list nor load skills.
                        skill_index: if allow_guidance { skills.disclosure_text() } else { None },
                    },
                    workspace_instructions,
                    persona.as_deref(),
                    selected_guidance.as_ref(),
                );
                system.push_str(&context_blocks);
                system
            });
            let mut tool_schema = tool_schema_measurement(&tool_specs);
            let system_prompt_hash = ContentHash::from_bytes(Sha256::digest(system.as_bytes()).into());
            let mut prompt_identity = Some(Arc::new(RunPromptIdentity {
                    version: AGENT_PROMPT_VERSION,
                    instruction_hash: workspace_instructions.hash(),
                    system_prompt_hash: Some(system_prompt_hash),
                    tool_schema_hash: Some(tool_schema.hash),
                    selected_guidance: selected_guidance
                        .as_ref()
                        .map(|guidance| Box::new(guidance.identity())),
                    catalog_digest: Some(catalog.digest()),
                    exposure: Some(match catalog.exposure() {
                        catalog::Exposure::Full => qq_protocol::ToolExposure::Full,
                        catalog::Exposure::Progressive => qq_protocol::ToolExposure::Progressive,
                    }),
                    context_sources: context_records,
                }));
            // Only the transcript preceding the accepted prompt can be
            // replaced by a between-run compaction. Everything appended by
            // this run is irreducible until the run settles.
            let reducible_messages = messages.len().saturating_sub(1);
            let reducible_message_bytes = measure_messages(&messages[..reducible_messages]);
            let mut irreducible_message_bytes =
                measure_messages(&messages[reducible_messages..]);
            let mut compatible_request: Option<(Arc<str>, bool, u64, u64)> = None;

            let mut slice_tool_calls = 0_usize;
            let mut output_continuations = 0_u16;
            // What this run did, for the heuristic audit trigger and the
            // auditor's action summary. Only roots with a hook keep actions.
            let mut audit_triggers = runtime::AuditTriggers::default();
            let mut audit_actions: Vec<runtime::AuditedAction> = Vec::new();
            let mut audit_revisions = 0_u16;
            let audit_prompt = messages
                .iter()
                .rev()
                .find(|message| message.role() == Role::User)
                .and_then(|message| {
                    message.content().iter().find_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            let mut model_text_bytes = 0_usize;
            let mut continuing_slice = false;
            for turn_ordinal in 1..=u16::MAX {
                // Caller budgets are decided at the turn boundary, before any
                // provider request. A spent work budget grants one tool-free
                // final response; a second spent check, an elapsed wall
                // clock, or unmeasurable cost settles the run here.
                let budget_final_turn = match budget.before_turn(
                    tokio::time::Instant::now(),
                    if allow_tools { MAX_TOOL_CALLS_PER_TURN } else { 0 },
                ) {
                    BudgetDecision::Continue => false,
                    BudgetDecision::FinalResponse(_) => true,
                    BudgetDecision::Exhausted(exhaustion) => {
                        yield RuntimeEvent::BudgetExhausted { exhaustion };
                        return;
                    }
                };
                // Reserve enough capacity for the largest valid provider
                // turn. Without this reservation, a slice at (for example)
                // 255 calls could accept a 16-call turn and overshoot its
                // strict ceiling before reaching the next turn boundary.
                // The tool-free checkpoint is persisted but is not the run's
                // terminal outcome; the next turn starts a new slice.
                let checkpoint_turn = !budget_final_turn
                    && slice_tool_calls
                        .saturating_add(MAX_TOOL_CALLS_PER_TURN)
                        > MAX_TOOL_CALLS_PER_SLICE;
                let continuation_turn = std::mem::take(&mut continuing_slice);
                let request_system: Arc<str> = if budget_final_turn {
                    Arc::from(format!("{system}\n\n{BUDGET_FINAL_RESPONSE_NOTICE}"))
                } else if checkpoint_turn {
                    Arc::from(format!("{system}\n\n{SLICE_CHECKPOINT_NOTICE}"))
                } else if continuation_turn {
                    Arc::from(format!("{system}\n\n{SLICE_CONTINUATION_NOTICE}"))
                } else {
                    Arc::clone(&system)
                };
                let request_has_tools = allow_tools && !checkpoint_turn && !budget_final_turn;
                let request_system_hash = if budget_final_turn || checkpoint_turn || continuation_turn {
                    ContentHash::from_bytes(Sha256::digest(request_system.as_bytes()).into())
                } else {
                    system_prompt_hash
                };
                let system_bytes = u64::try_from(request_system.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(CONTEXT_BLOCK_FRAMING_BYTES);
                let tool_schema_bytes = if request_has_tools {
                    tool_schema.bytes
                } else {
                    0
                };
                let input_bytes = system_bytes
                    .saturating_add(tool_schema_bytes)
                    .saturating_add(reducible_message_bytes)
                    .saturating_add(irreducible_message_bytes);
                let compatible_input_tokens = compatible_request.as_ref().and_then(
                    |(previous_system, previous_had_tools, previous_bytes, previous_tokens)| {
                        (previous_system.as_ref() == request_system.as_ref()
                            && *previous_had_tools == request_has_tools
                            && input_bytes >= *previous_bytes)
                            .then(|| previous_tokens.saturating_add(input_bytes - previous_bytes))
                    },
                );
                yield RuntimeEvent::Prepared {
                    turn_ordinal,
                    identity: prompt_identity.take(),
                    static_prefix: PreparedStaticPrefix::new(
                        request_system_hash,
                        request_has_tools.then_some(tool_schema.hash),
                    ),
                    weight: PreparedRequestWeight {
                        max_output_tokens,
                        system_bytes,
                        tool_schema_bytes,
                        reducible_message_bytes,
                        irreducible_message_bytes,
                        compatible_input_tokens,
                    },
                };
                // The provider owns every retry: it resends only while nothing
                // has streamed, so this loop sees each logical turn once and
                // can never duplicate output.
                let request = ModelRequest::new(
                    Arc::clone(&model),
                    messages.clone(),
                    max_output_tokens,
                );
                let request = if request_has_tools {
                    request
                        .with_tools(Arc::clone(&tool_specs))
                        .with_system(Arc::clone(&request_system))
                } else {
                    request.with_system(Arc::clone(&request_system))
                };
                let mut activity = RunActivity::WaitingForProvider;
                yield RuntimeEvent::ActivityChanged { activity };
                let mut provider_events = provider.stream(request);
                let mut pending_calls = Vec::<PendingToolCall>::new();
                let mut calls_by_provider_id = HashMap::<String, usize>::new();
                let mut blocks = Vec::<TurnBlock>::new();
                let mut terminal_usage = None;
                let mut completed = false;
                let mut reasoning_bytes = 0_usize;
                let mut open_reasoning = None;
                let mut interrupted_turn = false;
                let mut truncated_turn = false;
                let deadline = budget.deadline();

                loop {
                    // The wall clock bounds a hanging provider too: an
                    // elapsed deadline settles the run without waiting for a
                    // stream event that may never arrive.
                    // An interrupting steer ends the stream here. Text that
                    // already streamed is kept as the partial turn; tool
                    // calls the model had begun are dropped, because their
                    // arguments may be incomplete and nothing has executed.
                    let interrupt = async {
                        match &mut steering {
                            Some(steering) => loop {
                                if *steering.interrupts.borrow() > handled_interrupt {
                                    break;
                                }
                                if steering.interrupts.changed().await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            },
                            None => std::future::pending().await,
                        }
                    };
                    let event = tokio::select! {
                        biased;
                        () = async {
                            match deadline {
                                Some(deadline) => tokio::time::sleep_until(deadline).await,
                                None => std::future::pending().await,
                            }
                        } => {
                            let exhaustion = budget.exhaustion(
                                BudgetLimitKind::Duration,
                                false,
                                tokio::time::Instant::now(),
                            );
                            yield RuntimeEvent::BudgetExhausted { exhaustion };
                            return;
                        }
                        () = interrupt => StreamStep::Interrupted,
                        event = provider_events.next() => StreamStep::Event(event),
                    };
                    let event = match event {
                        StreamStep::Interrupted => {
                            interrupted_turn = true;
                            completed = true;
                            break;
                        }
                        StreamStep::Event(Some(event)) => event,
                        StreamStep::Event(None) => break,
                    };
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
                            if budget_final_turn {
                                // The model ignored the tool-free final
                                // response request. The budget still settles
                                // the run: exhaustion is never a provider
                                // failure, and no more work may be spent.
                                let BudgetDecision::Exhausted(mut exhaustion) = budget
                                    .before_turn(tokio::time::Instant::now(), 0)
                                else {
                                    unreachable!("a requested final response always settles the run")
                                };
                                exhaustion.final_response = false;
                                yield RuntimeEvent::BudgetExhausted { exhaustion };
                                return;
                            }
                            if !request_has_tools {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: if checkpoint_turn {
                                        "provider requested a tool on the tool-free checkpoint turn, which declares none".to_owned()
                                    } else {
                                        "provider requested a tool after the request declared no tools".to_owned()
                                    },
                                };
                                return;
                            }
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
                                rejection: None,
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
                                    call.rejection = Some(format!(
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
                        Ok(ProviderEvent::Incomplete { usage, reason: _ }) => {
                            // The turn is a valid prefix but the model was
                            // not done. Text stands; any tool call it had
                            // begun carries incomplete arguments and is
                            // dropped, so nothing from this turn executes.
                            // Continuation (or the typed failure) is decided
                            // after the partial turn is committed.
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::ReasoningCompleted {
                                    kind: open_reasoning.take().expect("checked"),
                                };
                            }
                            terminal_usage = usage.map(provider_usage);
                            truncated_turn = true;
                            completed = true;
                            break;
                        }
                        Err(error) => {
                            yield RuntimeEvent::Failed {
                                kind: run_failure_kind(error.kind()),
                                message: error.to_string(),
                            };
                            return;
                        }
                    }
                }

                if !completed {
                    // The provider restarts a stream that ends before its
                    // first event; one that ends after events is its
                    // protocol violation, and nothing above it may resend.
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::ProviderProtocol,
                        message: "provider stream ended without a terminal event".to_owned(),
                    };
                    return;
                }

                if interrupted_turn || truncated_turn {
                    if interrupted_turn {
                        handled_interrupt = steering
                            .as_ref()
                            .map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                    }
                    // Only fully streamed calls could be executed; an interrupt
                    // or truncation executes none, so the partial turn carries
                    // text alone.
                    blocks.retain(|block| matches!(block, TurnBlock::Text(_)));
                    pending_calls.clear();
                }

                compatible_request = terminal_usage.map(|usage| {
                    (
                        Arc::clone(&request_system),
                        request_has_tools,
                        input_bytes,
                        usage
                            .input_tokens
                            .saturating_add(usage.cache_read_input_tokens)
                            .saturating_add(usage.cache_write_input_tokens),
                    )
                });

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
                    // Effect is resolved once here from the catalog and
                    // travels with the call; policy never re-derives it from
                    // the name. A name the catalog does not hold is not
                    // executable, so it settles as a tool error before any
                    // gate sees it.
                    let known = catalog.lookup(&pending.name).map(|entry| entry.effect);
                    #[cfg(test)]
                    let known = known.or_else(|| tools::test_tool_effect(&pending.name));
                    let (effect, rejection) = match known {
                        Some(effect) => (effect, pending.rejection),
                        None => (
                            catalog::EffectClass::ReadOnly,
                            Some(format!("unknown tool {:?}", pending.name)),
                        ),
                    };
                    calls.push(RuntimeToolCall {
                        id,
                        turn_ordinal,
                        call_ordinal: u16::try_from(index + 1)
                            .expect("the per-turn tool bound fits u16"),
                        provider_call_id: pending.provider_call_id,
                        name: pending.name,
                        arguments: pending.arguments,
                        effect,
                        rejection,
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
                    truncated: truncated_turn,
                };
                budget.charge_turn(terminal_usage);
                budget.charge_tool_calls(calls.len());

                if truncated_turn {
                    // A reserved final response that ran out of room cannot be
                    // continued: the budget already settles the run below.
                    // Otherwise resume, bounded, or settle with the reason.
                    if !budget_final_turn {
                        if output_continuations >= MAX_OUTPUT_CONTINUATIONS {
                            yield RuntimeEvent::Failed {
                                kind: RunFailureKind::ProviderOutputTruncated,
                                message: format!(
                                    "the provider stopped at its output token limit ({max_output_tokens} tokens) on \
                                     {} consecutive turns; the partial answer is in the transcript",
                                    u32::from(MAX_OUTPUT_CONTINUATIONS) + 1
                                ),
                            };
                            return;
                        }
                        output_continuations += 1;
                        yield RuntimeEvent::OutputTruncated {
                            turn_ordinal,
                            continuation: output_continuations,
                        };
                        if assistant.has_content() {
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(&assistant));
                            messages.push(assistant);
                        }
                        if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                            messages.push(Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE));
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(messages.last().expect("just pushed")));
                        }
                        continue;
                    }
                } else {
                    output_continuations = 0;
                }

                if interrupted_turn {
                    yield RuntimeEvent::Interrupted { turn_ordinal };
                    if assistant.has_content() {
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        messages.push(assistant);
                    }
                    // The interrupt exists to apply steering now. Nothing
                    // queued means the client raced a finishing run; continue
                    // with the next turn so the model resumes from its text.
                    if let Some(applied) = apply_steering(
                        &mut steering,
                        &mut messages,
                        &mut irreducible_message_bytes,
                        turn_ordinal.saturating_add(1),
                    ) {
                        for message_id in applied {
                            yield RuntimeEvent::SteeringApplied {
                                message_id,
                                turn_ordinal: turn_ordinal.saturating_add(1),
                            };
                        }
                    }
                    if messages.last().is_some_and(|message| message.role() == Role::Assistant) {
                        // Providers require alternation; an interrupted turn
                        // with no steering to inject cannot be resent as-is.
                        messages.push(Message::user(INTERRUPT_CONTINUE_NOTICE));
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(messages.last().expect("just pushed")));
                    }
                    continue;
                }

                if budget_final_turn {
                    // The reserved final response has been persisted; the
                    // run settles with the limit that spent its budget.
                    let BudgetDecision::Exhausted(exhaustion) = budget.before_turn(
                        tokio::time::Instant::now(),
                        0,
                    ) else {
                        unreachable!("a requested final response always settles the run")
                    };
                    yield RuntimeEvent::BudgetExhausted { exhaustion };
                    return;
                }
                // Cost and token bounds are only observable after a turn. A
                // completed run that overran them settles as exhausted, not
                // completed, so no client can mistake the overrun for success.
                if calls.is_empty()
                    && let Some(kind) = budget.exceeded(tokio::time::Instant::now())
                    && matches!(
                        kind,
                        BudgetLimitKind::Cost
                            | BudgetLimitKind::CostUnknown
                            | BudgetLimitKind::TotalTokens
                    )
                {
                    let exhaustion = budget.exhaustion(kind, false, tokio::time::Instant::now());
                    yield RuntimeEvent::BudgetExhausted { exhaustion };
                    return;
                }

                if checkpoint_turn && !assistant.has_content() {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::ProviderResponse,
                        message: "provider returned an empty slice checkpoint".to_owned(),
                    };
                    return;
                }
                if calls.is_empty() && checkpoint_turn {
                    irreducible_message_bytes = irreducible_message_bytes
                        .saturating_add(measure_message(&assistant));
                    messages.push(assistant);
                    slice_tool_calls = 0;
                    continuing_slice = true;
                    continue;
                }
                if calls.is_empty() {
                    // Steering that arrived during the final turn is not
                    // dropped: the run continues with it instead of
                    // completing, exactly as if the model had called a tool.
                    if let Some(applied) = apply_steering(
                        &mut steering,
                        &mut messages,
                        &mut irreducible_message_bytes,
                        turn_ordinal.saturating_add(1),
                    ) {
                        irreducible_message_bytes = irreducible_message_bytes
                            .saturating_add(measure_message(&assistant));
                        let steering_messages = messages.split_off(messages.len() - applied.len());
                        messages.push(assistant);
                        messages.extend(steering_messages);
                        for message_id in applied {
                            yield RuntimeEvent::SteeringApplied {
                                message_id,
                                turn_ordinal: turn_ordinal.saturating_add(1),
                            };
                        }
                        continue;
                    }
                    // The candidate final answer. A root run whose work meets
                    // the audit trigger hands it to a read-only auditor before
                    // completing; a revise verdict continues the loop once
                    // with the findings, then the revision stands. Budget final
                    // turns settle above and are never audited.
                    // The first answer is always audited; a revision is audited
                    // only while another revision could follow, so the answer
                    // at the cap stands as given.
                    if let Some(hook) = &audit_hook
                        && (audit_revisions == 0
                            || audit_revisions < plan.runtime.audit.max_revisions)
                        && audit_triggers.fires(plan.runtime.audit.mode)
                        && let Ok(child_limits) = budget.child_budget(tokio::time::Instant::now())
                    {
                        let answer = assistant
                            .content()
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut auditing = hook
                            .audit(runtime::AuditRequest {
                                prompt: audit_prompt.clone(),
                                answer: bounded_text(&answer, runtime::MAX_AUDIT_ANSWER_BYTES),
                                actions: audit_actions.clone(),
                                role: plan.runtime.audit.role,
                                revision: audit_revisions,
                            }, child_limits);
                        let verdict = tokio::select! {
                            biased;
                            () = interrupt_requested(&mut steering, handled_interrupt) => None,
                            verdict = &mut auditing => Some(verdict),
                        };
                        drop(auditing);
                        let audit_interrupted = verdict.is_none();
                        let verdict = match verdict {
                            Some(verdict) => verdict,
                            None => {
                                let spends = match hook.drain().await {
                                    Ok(spends) => spends,
                                    Err(error) => {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                        return;
                                    }
                                };
                                let spend = match spends.as_slice() {
                                    [] => SpawnAgentSpend::NONE,
                                    [spend] => *spend,
                                    _ => {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: "audit cleanup found multiple uncharged children".to_owned() };
                                        return;
                                    }
                                };
                                runtime::AuditVerdict {
                                    usage: spend.usage, cost_usd_nanos: spend.cost_usd_nanos,
                                    ..runtime::AuditVerdict::unavailable()
                                }
                            }
                        };
                        budget.charge_child(verdict.usage, verdict.cost_usd_nanos);
                        hook.acknowledge();
                        let revise = verdict.outcome == qq_protocol::AuditOutcome::Revised
                            && audit_revisions < plan.runtime.audit.max_revisions;
                        yield RuntimeEvent::Audited {
                            outcome: verdict.outcome,
                            findings: verdict.findings.clone(),
                            revisions: audit_revisions,
                            usage: verdict.usage,
                            cost_usd_nanos: verdict.cost_usd_nanos,
                            audit_session: verdict.audit_session,
                        };
                        if audit_interrupted {
                            handled_interrupt = steering.as_ref().map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                            irreducible_message_bytes = irreducible_message_bytes.saturating_add(measure_message(&assistant));
                            messages.push(assistant);
                            yield RuntimeEvent::Interrupted { turn_ordinal };
                            if let Some(applied) = apply_steering(&mut steering, &mut messages, &mut irreducible_message_bytes, turn_ordinal.saturating_add(1)) {
                                for message_id in applied {
                                    yield RuntimeEvent::SteeringApplied { message_id, turn_ordinal: turn_ordinal.saturating_add(1) };
                                }
                            }
                            continue;
                        }
                        if revise {
                            audit_revisions += 1;
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(&assistant));
                            messages.push(assistant);
                            let mut notice = String::from(runtime::AUDIT_REVISION_NOTICE);
                            for finding in &verdict.findings {
                                notice.push_str("\n- ");
                                notice.push_str(finding);
                            }
                            messages.push(Message::user(notice));
                            irreducible_message_bytes = irreducible_message_bytes
                                .saturating_add(measure_message(messages.last().expect("just pushed")));
                            continue;
                        }
                    }
                    // Steering accepted while an audit ran still owns the next boundary.
                    if let Some(applied) = apply_steering(&mut steering, &mut messages, &mut irreducible_message_bytes, turn_ordinal.saturating_add(1)) {
                        irreducible_message_bytes = irreducible_message_bytes.saturating_add(measure_message(&assistant));
                        let queued = messages.split_off(messages.len() - applied.len());
                        messages.push(assistant);
                        messages.extend(queued);
                        for message_id in applied { yield RuntimeEvent::SteeringApplied { message_id, turn_ordinal: turn_ordinal.saturating_add(1) }; }
                        continue;
                    }
                    if let Some(kind) = budget.exceeded(tokio::time::Instant::now()) {
                        yield RuntimeEvent::BudgetExhausted { exhaustion: budget.exhaustion(kind, false, tokio::time::Instant::now()) };
                        return;
                    }
                    yield RuntimeEvent::Completed;
                    return;
                }
                irreducible_message_bytes = irreducible_message_bytes
                    .saturating_add(measure_message(&assistant));
                messages.push(assistant);

                // Policy resolves sequentially in request order, after the
                // turn and its `requested` call rows are persisted, so
                // approval prompts arrive one at a time. Calls with malformed
                // arguments never reach the gate: there is nothing executable
                // to approve, so they short-circuit to their tool error below.
                let mut results = vec![None; calls.len()];
                let mut turn_interrupted_in_tools = false;
                for (index, call) in calls.iter().enumerate() {
                    if call.rejection.is_some() {
                        continue;
                    }
                    if turn_interrupted_in_tools {
                        // Calls behind an interrupted approval wait never
                        // execute; they settle like calls behind a cancel.
                        results[index] = Some(tools::ToolExecutionResult {
                            content: INTERRUPTED_TOOL_RESULT.to_owned(),
                            is_error: true,
                            file_state: None,
                        });
                        continue;
                    }
                    // An approval wait is a boundary too: an interrupting
                    // steer withdraws the pending request instead of leaving
                    // the user to answer a question the steer made moot.
                    let decision = {
                        let interrupt = interrupt_requested(&mut steering, handled_interrupt);
                        tokio::select! {
                            biased;
                            () = interrupt => None,
                            decision = gate.resolve(call) => Some(decision),
                        }
                    };
                    let Some(decision) = decision else {
                        turn_interrupted_in_tools = true;
                        results[index] = Some(tools::ToolExecutionResult {
                            content: INTERRUPTED_TOOL_RESULT.to_owned(),
                            is_error: true,
                            file_state: None,
                        });
                        continue;
                    };
                    // Reviewer spend is charged whatever the verdict; the
                    // wrapped decision is then applied like any other.
                    let decision = match decision {
                        GateDecision::Reviewed { decision, spend } => {
                            budget.charge_child(spend.usage, spend.cost_usd_nanos);
                            yield RuntimeEvent::ReviewCharged {
                                usage: spend.usage,
                                cost_usd_nanos: spend.cost_usd_nanos,
                            };
                            *decision
                        }
                        decision => decision,
                    };
                    match decision {
                        GateDecision::Execute => {}
                        GateDecision::Deny { message } => {
                            results[index] = Some(tools::ToolExecutionResult {
                                content: message.clone(),
                                is_error: true,
                                file_state: None,
                            });
                            yield RuntimeEvent::ToolCallDenied { id: call.id, message };
                        }
                        GateDecision::Fail { kind, message } => {
                            yield RuntimeEvent::Failed { kind, message };
                            return;
                        }
                        GateDecision::Reviewed { .. } => {
                            unreachable!("reviewed decisions are unwrapped once, never nested")
                        }
                    }
                }
                let approved = calls
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| results[*index].is_none() && !turn_interrupted_in_tools)
                    .map(|(_, call)| call.clone())
                    .collect::<Vec<_>>();
                for call in &approved {
                    yield RuntimeEvent::ToolCallStarted { id: call.id };
                }

                // `select_tools` mutates run state (the pin set), so it
                // executes here, before the concurrent dispatch below, in
                // request order. It is read-only and instantaneous.
                let mut pins_changed = false;
                for (index, call) in calls.iter().enumerate() {
                    if results[index].is_some() || call.rejection.is_some() {
                        continue;
                    }
                    if !matches!(
                        catalog.lookup(&call.name).map(|entry| entry.host),
                        Some(catalog::ToolHost::SelectTools)
                    ) {
                        continue;
                    }
                    let (result, changed) = select_tools(&catalog, &mut pins, &call.arguments);
                    pins_changed |= changed;
                    results[index] = Some(result.clone());
                    yield RuntimeEvent::ToolCallFinished {
                        id: call.id,
                        result: result.content,
                        is_error: result.is_error,
                        file_state: None,
                        display: None,
                    };
                }
                if pins_changed {
                    tool_specs = catalog.specs_with_pins(&base_specs, &pins);
                    tool_schema = tool_schema_measurement(&tool_specs);
                }
                let approved = approved
                    .into_iter()
                    .filter(|call| results[usize::from(call.call_ordinal - 1)].is_none())
                    .collect::<Vec<_>>();
                let execute_one = |call: RuntimeToolCall,
                                   output: Option<
                    tokio::sync::mpsc::Sender<String>,
                >, child_limits: Result<runtime::ChildBudget, BudgetLimitKind>| {
                    let workspace = workspace.clone();
                    let file_state = Arc::clone(&file_state);
                    let cancelled = Arc::clone(&cancelled);
                    let catalog = Arc::clone(&catalog);
                    let skills = Arc::clone(&skills);
                    let pack_roots = Arc::clone(&pack_roots);
                    let hosts = Arc::clone(&hosts);
                    let spawner = spawner.clone();
                    let tool_tasks = tool_tasks.clone();
                    let history = history.clone();
                    let delegation = Arc::clone(&delegation);
                    // Under progressive exposure only pinned externals were
                    // offered; a call to one that was not is refused with the
                    // way to make it available.
                    let offered = catalog.exposure() == catalog::Exposure::Full
                        || pins.names().contains(&call.name);
                    async move {
                        // `Some` when a sub-agent ran: its spend (or unknown
                        // spend) is charged to the parent's budgets.
                        let mut child_spend: Option<SpawnAgentSpend> = None;
                        let host = catalog.lookup(&call.name).map(|entry| entry.host);
                        let result = match call.rejection.clone() {
                            Some(error) => tools::ToolExecutionResult {
                                content: error,
                                is_error: true,
                                file_state: None,
                            },
                            // spawn_agent dispatches to the session layer. A
                            // run without a spawner rejects the call outright:
                            // the declaration is already absent there, but a
                            // model may still guess the name.
                            None if host == Some(catalog::ToolHost::SpawnAgent) => match &spawner {
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
                                            // Role and roster resolve here, the one
                                            // choke point: an exact model must be a
                                            // roster route when a roster exists, and
                                            // a role maps to the first roster route
                                            // declaring it. The session spawner then
                                            // validates the resolved route against
                                            // the authenticated served model list
                                            // before any durable child state exists.
                                            let resolution = resolve_delegation_route(
                                                &delegation,
                                                arguments.model,
                                                arguments.role,
                                            );
                                            match (child_limits, resolution) {
                                                (_, Err(message)) => tools::bounded_result(message, true),
                                                (Err(kind), Ok(_)) => tools::bounded_result(
                                                    format!(
                                                        "this run cannot afford a sub-agent: its {} budget is spent; continue with what you have",
                                                        kind.as_str()
                                                    ),
                                                    true,
                                                ),
                                                (Ok(child_budget), Ok(model)) => {
                                                    let outcome = spawner
                                                        .spawn(SpawnRequest {
                                                            call_id: call.id,
                                                            task: arguments.task,
                                                            model,
                                                            authority: arguments.authority,
                                                            budget: child_budget,
                                                            purpose: qq_protocol::SessionPurpose::Task,
                                                        })
                                                        .await;
                                                    child_spend = Some(outcome.spend);
                                                    tools::bounded_result(
                                                        outcome.content,
                                                        outcome.is_error,
                                                    )
                                                }
                                            }
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
                            // Full-transcript recall dispatches to the session
                            // layer; the tool is declared only when a searcher
                            // exists, so a guessed call is simply unknown here.
                            None if host == Some(catalog::ToolHost::SearchHistory) && history.is_some() => {
                                let history = history.expect("the history searcher was just checked");
                                match serde_json::from_str::<SearchHistoryArgs>(&call.arguments) {
                                    Ok(arguments) if arguments.query.trim().is_empty() => {
                                        tools::bounded_result("query must not be empty".to_owned(), true)
                                    }
                                    Ok(arguments) => {
                                        let limit = arguments.limit.clamp(1, crate::runtime::MAX_HISTORY_MATCHES);
                                        match history.search(arguments.query.clone(), limit).await {
                                            Ok(matches) => tools::bounded_result(
                                                render_history_matches(&arguments.query, &matches),
                                                false,
                                            ),
                                            Err(error) => tools::bounded_result(error, true),
                                        }
                                    }
                                    Err(error) => tools::bounded_result(
                                        format!("invalid arguments: {error}"),
                                        true,
                                    ),
                                }
                            }
                            // The model asked for a disclosed skill body. Same
                            // bounds as a `/name` invocation; failures are
                            // tool errors, not run failures.
                            None if host == Some(catalog::ToolHost::LoadSkill) => {
                                match serde_json::from_str::<workspace::skills::LoadSkillArgs>(&call.arguments) {
                                    Ok(arguments) => match workspace::load_disclosed_skill(
                                        workspace,
                                        pack_roots,
                                        skills,
                                        cancelled,
                                        arguments.name.trim().to_owned(),
                                    )
                                    .await
                                    {
                                        Ok(guidance) => {
                                            tools::bounded_result(guidance.render_for_tool(), false)
                                        }
                                        Err(error) => tools::bounded_result(error.to_string(), true),
                                    },
                                    Err(error) => tools::bounded_result(
                                        format!("invalid arguments: {error}"),
                                        true,
                                    ),
                                }
                            }
                            // External calls dispatch to their host by index;
                            // the outcome flows through the same bounded-result
                            // truncation as built-in tools, so an external call
                            // is indistinguishable from a built-in on the wire.
                            None if let Some(catalog::ToolHost::External { .. }) = host
                                && !offered =>
                            {
                                tools::bounded_result(
                                    format!(
                                        "{} is not available in this run yet; call {} with keywords \
                                         describing it first",
                                        call.name, catalog::SELECT_TOOLS_TOOL
                                    ),
                                    true,
                                )
                            }
                            None if let Some(catalog::ToolHost::External { host: index }) = host => {
                                match hosts[index]
                                    .call(call.name.clone(), call.arguments.clone(), cancelled)
                                    .await
                                {
                                    Ok(outcome) => tools::bounded_result(outcome.content, outcome.is_error),
                                    Err(error) => hosts::host_error_result(&error),
                                }
                            }
                            None => {
                                tools::execute(
                                    workspace,
                                    file_state,
                                    call.name.clone(),
                                    call.arguments.clone(),
                                    cancelled,
                                    output,
                                    tool_tasks,
                                )
                                .await
                            }
                        };
                        (call, result, child_spend)
                    }
                };
                // Read-only turns overlap under a small bound; a turn with any
                // mutating, shell, or external call runs entirely in request
                // order so side effects never interleave and every read is
                // deterministically ordered against the mutations. Only a
                // read child may overlap: a write child is a mutation.
                // Finite spend cannot be granted independently to overlapping children.
                // Unbounded and duration-only read fanout retains its concurrency.
                let bounded_child_spend = limits.max_cost_usd_nanos.is_some()
                    || limits.max_total_tokens.is_some()
                    || limits.max_input_tokens.is_some()
                    || limits.max_output_tokens.is_some();
                let sequential = approved.iter().any(|call| {
                    (bounded_child_spend && catalog.lookup(&call.name).is_some_and(|entry| entry.host == catalog::ToolHost::SpawnAgent))
                    || !matches!(
                        approval::classify(call.effect, &call.name, &call.arguments),
                        approval::ToolClass::ReadOnly
                    )
                });
                if sequential {
                    for call in approved {
                        // Live output chunks (shell) interleave with execution:
                        // drain the channel while the call runs so long
                        // commands render as they print.
                        let call_id = call.id;
                        let mut call_id_holder = Some(call.clone());
                        let (delta_sender, mut deltas) =
                            tokio::sync::mpsc::channel::<String>(SHELL_OUTPUT_QUEUE_CAPACITY);
                        let mut execution = Box::pin(execute_one(call, Some(delta_sender), budget.child_budget(tokio::time::Instant::now())));
                        let mut output_closed = false;
                        let (call, result, child_spend) = loop {
                            let interrupt = interrupt_requested(&mut steering, handled_interrupt);
                            tokio::select! {
                                biased;
                                () = interrupt => {
                                    // Stop dispatch, then await owned local work
                                    // and children before applying steering.
                                    drop(execution);
                                    if let Err(error) = tool_tasks.drain().await {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                        return;
                                    }
                                    if let Some(spawner) = &spawner {
                                        match spawner.drain().await {
                                            Ok(spends) => for spend in spends { budget.charge_child(spend.usage, spend.cost_usd_nanos); },
                                            Err(error) => {
                                                yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                                return;
                                            }
                                        }
                                    }
                                    break (call_id_holder.take().expect("call retained"), tools::ToolExecutionResult {
                                        content: INTERRUPTED_TOOL_RESULT.to_owned(),
                                        is_error: true,
                                        file_state: None,
                                    }, None);
                                }
                                chunk = deltas.recv(), if !output_closed => match chunk {
                                    Some(chunk) => {
                                        yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                                    }
                                    // Keep selecting interruption after output closes.
                                    None => output_closed = true,
                                },
                                completed = &mut execution => break completed,
                            }
                        };
                        if let Err(error) = tool_tasks.check() {
                            yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                            return;
                        }
                        let interrupted_here = result.content == INTERRUPTED_TOOL_RESULT && result.is_error && result.file_state.is_none() && steering.as_ref().is_some_and(|steering| *steering.interrupts.borrow() > handled_interrupt);
                        // Chunks sent in the execution's final poll may still
                        // be buffered; drain them before the terminal event.
                        while let Ok(chunk) = deltas.try_recv() {
                            yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                        }
                        if let Some(spend) = child_spend {
                            budget.charge_child(spend.usage, spend.cost_usd_nanos);
                            if let Some(spawner) = &spawner { spawner.acknowledge(call.id); }
                        }
                        note_audited_action(
                            &mut audit_triggers,
                            &mut audit_actions,
                            audit_hook.is_some(),
                            &call,
                            &result,
                        );
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
                        if interrupted_here {
                            turn_interrupted_in_tools = true;
                            break;
                        }
                    }
                } else {
                    let child_limits = budget.child_budget(tokio::time::Instant::now());
                    let mut executions = futures_stream::iter(
                        approved.into_iter().map(|call| execute_one(call, None, child_limits)),
                    )
                        .buffer_unordered(MAX_PARALLEL_READS);
                    loop {
                        let interrupt = interrupt_requested(&mut steering, handled_interrupt);
                        let next = tokio::select! {
                            biased;
                            () = interrupt => {
                                turn_interrupted_in_tools = true;
                                break;
                            }
                            next = executions.next() => next,
                        };
                        let Some((call, result, child_spend)) = next else {
                            break;
                        };
                        if let Err(error) = tool_tasks.check() {
                            yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                            return;
                        }
                        if let Some(spend) = child_spend {
                            budget.charge_child(spend.usage, spend.cost_usd_nanos);
                            if let Some(spawner) = &spawner { spawner.acknowledge(call.id); }
                        }
                        note_audited_action(
                            &mut audit_triggers,
                            &mut audit_actions,
                            audit_hook.is_some(),
                            &call,
                            &result,
                        );
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
                if turn_interrupted_in_tools {
                    if let Err(error) = tool_tasks.drain().await {
                                        yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                        return;
                                    }
                    if let Some(spawner) = &spawner {
                        match spawner.drain().await {
                            Ok(spends) => for spend in spends { budget.charge_child(spend.usage, spend.cost_usd_nanos); },
                            Err(error) => {
                                yield RuntimeEvent::Failed { kind: RunFailureKind::Server, message: error.to_string() };
                                return;
                            }
                        }
                    }
                    handled_interrupt = steering
                        .as_ref()
                        .map_or(handled_interrupt, |steering| *steering.interrupts.borrow());
                    // Calls that never finished settle as interrupted so the
                    // transcript stays provider-valid: one result per call.
                    for (index, call) in calls.iter().enumerate() {
                        if results[index].is_none() {
                            results[index] = Some(tools::ToolExecutionResult {
                                content: INTERRUPTED_TOOL_RESULT.to_owned(),
                                is_error: true,
                                file_state: None,
                            });
                            yield RuntimeEvent::ToolCallFinished {
                                id: call.id,
                                result: INTERRUPTED_TOOL_RESULT.to_owned(),
                                is_error: true,
                                file_state: None,
                                display: None,
                            };
                        }
                    }
                    yield RuntimeEvent::Interrupted { turn_ordinal };
                }
                let result_blocks = calls
                    .iter()
                    .zip(results.into_iter())
                    .map(|(call, result)| {
                        let result = result.expect("every bounded tool execution completed");
                        budget.charge_tool_output(result.content.len());
                        ContentBlock::ToolResult {
                            call_id: call.provider_call_id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                        }
                    })
                    .collect();
                let tool_results = Message::tool_results(result_blocks);
                irreducible_message_bytes = irreducible_message_bytes
                    .saturating_add(measure_message(&tool_results));
                messages.push(tool_results);
                // The boundary: every result of this turn is in context, and
                // the next request has not been built. Steering joins here as
                // a user message after the tool results.
                if let Some(applied) = apply_steering(
                    &mut steering,
                    &mut messages,
                    &mut irreducible_message_bytes,
                    turn_ordinal.saturating_add(1),
                ) {
                    for message_id in applied {
                        yield RuntimeEvent::SteeringApplied {
                            message_id,
                            turn_ordinal: turn_ordinal.saturating_add(1),
                        };
                    }
                }
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
fn public_run_stream(mut events: RuntimeStream, context_window: Option<u32>) -> RunStream {
    Box::pin(stream! {
        while let Some(event) = events.next().await {
            match event {
                RuntimeEvent::Started => yield RunEvent::Started,
                RuntimeEvent::Prepared { weight, .. } => {
                    let plan = sessions::context::plan(sessions::context::ContextInput {
                        context_window,
                        max_output_tokens: weight.max_output_tokens,
                        system_bytes: weight.system_bytes,
                        tool_schema_bytes: weight.tool_schema_bytes,
                        reducible_message_bytes: weight.reducible_message_bytes,
                        irreducible_message_bytes: weight.irreducible_message_bytes,
                        compatible_input_tokens: weight.compatible_input_tokens,
                        // The direct compatibility path has no durable
                        // between-run compaction lifecycle.
                        compaction: sessions::context::CompactionDisposition::Unsupported,
                    });
                    if let Some(message) = sessions::context::rejection_message(plan) {
                        yield RunEvent::Failed {
                            kind: RunFailureKind::Policy,
                            message,
                        };
                        return;
                    }
                }
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
                | RuntimeEvent::ToolCallFinished { .. }
                // Direct runs have no steering channel, so these never fire.
                | RuntimeEvent::SteeringApplied { .. }
                | RuntimeEvent::Interrupted { .. }
                // Direct runs have no reviewer or auditor either.
                | RuntimeEvent::ReviewCharged { .. }
                | RuntimeEvent::Audited { .. }
                // Continuation is transparent to the direct stream: the text
                // keeps flowing and the typed failure names exhaustion.
                | RuntimeEvent::OutputTruncated { .. } => {}
                RuntimeEvent::Completed => {
                    yield RunEvent::Completed;
                    return;
                }
                RuntimeEvent::Failed { kind, message } => {
                    yield RunEvent::Failed { kind, message };
                    return;
                }
                // The direct compatibility path imposes no caller limits, so
                // this cannot occur; surface it truthfully rather than panic.
                RuntimeEvent::BudgetExhausted { exhaustion } => {
                    yield RunEvent::Failed {
                        kind: RunFailureKind::Policy,
                        message: exhaustion.message,
                    };
                    return;
                }
            }
        }
    })
}

fn measure_messages(messages: &[Message]) -> u64 {
    messages.iter().fold(0_u64, |total, message| {
        total.saturating_add(measure_message(message))
    })
}

fn measure_message(message: &Message) -> u64 {
    message
        .content()
        .iter()
        .fold(CONTEXT_MESSAGE_FRAMING_BYTES, |total, block| {
            let content = match block {
                ContentBlock::Text { text } => u64::try_from(text.len()).unwrap_or(u64::MAX),
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => u64::try_from(id.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(name.len()).unwrap_or(u64::MAX))
                    .saturating_add(json_value_bytes(arguments)),
                ContentBlock::ToolResult {
                    call_id, content, ..
                } => u64::try_from(call_id.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(content.len()).unwrap_or(u64::MAX)),
            };
            total
                .saturating_add(CONTEXT_BLOCK_FRAMING_BYTES)
                .saturating_add(content)
        })
}

fn json_value_bytes(value: &serde_json::Value) -> u64 {
    struct ByteCounter(u64);

    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = ByteCounter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        u64::MAX
    } else {
        counter.0
    }
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
        reasoning_tokens: usage.reasoning_tokens,
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
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
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
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        }
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
            tools::ToolTasks::default(),
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

    /// Truncates the first `truncations` turns at the output limit (each with
    /// its own text prefix and, on the first, a half-streamed tool call), then
    /// completes with a final chunk. Records every request for inspection.
    struct TruncatingProvider {
        truncations: usize,
        /// Cut a tool call mid-arguments on the first truncated turn.
        cut_tool_call: bool,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for TruncatingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            let usage = Some(qq_provider::ProviderUsage {
                input_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 7,
                reasoning_tokens: None,
            });
            if turn < self.truncations {
                let mut events = vec![Ok(ProviderEvent::OutputTextDelta {
                    text: format!("part{turn} "),
                })];
                if turn == 0 && self.cut_tool_call {
                    // A tool call cut mid-arguments must never execute.
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: "cut".to_owned(),
                        name: "read_file".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "cut".to_owned(),
                        json: r#"{"path":"AGEN"#.to_owned(),
                    }));
                }
                events.push(Ok(ProviderEvent::Incomplete {
                    usage,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                }));
                Box::pin(stream::iter(events))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "end".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage }),
                ]))
            }
        }
    }

    #[tokio::test]
    async fn truncated_turns_are_committed_and_continued_within_the_cap() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: 2,
                cut_tool_call: true,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("write a long answer")],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;

        // Three provider turns, each committed; the two truncated ones flagged
        // and carrying no calls.
        let turns = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted {
                    turn_ordinal,
                    calls,
                    truncated,
                    ..
                } => Some((*turn_ordinal, calls.len(), *truncated)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(turns, vec![(1, 0, true), (2, 0, true), (3, 0, false)]);
        let continuations = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::OutputTruncated {
                    turn_ordinal,
                    continuation,
                } => Some((*turn_ordinal, *continuation)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(continuations, vec![(1, 1), (2, 2)]);
        assert_eq!(events.last(), Some(&RuntimeEvent::Completed));
        // The half-streamed tool call never reached the tool loop.
        assert!(!events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallStarted { .. } | RuntimeEvent::ToolCallDenied { .. }
        )));

        // The final request carries both partial turns with the continuation
        // notice after each, so the model resumes rather than restarts, and
        // alternation holds.
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let transcript = requests[2]
            .messages()
            .iter()
            .map(|message| {
                let text = match message.content().first() {
                    Some(ContentBlock::Text { text }) => text.as_str(),
                    _ => "",
                };
                (message.role(), text)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            transcript,
            vec![
                (Role::User, "write a long answer"),
                (Role::Assistant, "part0 "),
                (Role::User, OUTPUT_TRUNCATED_CONTINUE_NOTICE),
                (Role::Assistant, "part1 "),
                (Role::User, OUTPUT_TRUNCATED_CONTINUE_NOTICE),
            ]
        );
        // Tools stay available on continuation turns.
        assert!(!requests[2].tools().is_empty());
    }

    #[tokio::test]
    async fn truncation_past_the_cap_settles_with_a_typed_failure() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("write a long answer")],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;

        let expected_turns = usize::from(MAX_OUTPUT_CONTINUATIONS) + 1;
        assert_eq!(requests.lock().unwrap().len(), expected_turns);
        let committed = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::AssistantTurnCompleted {
                        truncated: true,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(committed, expected_turns, "every partial turn is durable");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::OutputTruncated { .. }))
                .count(),
            usize::from(MAX_OUTPUT_CONTINUATIONS)
        );
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Failed {
                kind: RunFailureKind::ProviderOutputTruncated,
                message,
            }) if message.contains("256 tokens") && message.contains("4 consecutive turns")
        ));
    }

    #[tokio::test]
    async fn a_truncated_budget_final_turn_settles_as_exhausted_not_continued() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        // One model turn: the first request is already the reserved final
        // response, so its truncation must not spend a second turn.
        // (A tool call on that turn is already a budget failure; this case
        // covers plain text running out of room.)
        let limits = RunLimits {
            max_model_turns: Some(1),
            ..RunLimits::default()
        };
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("write a long answer")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_limits(limits, None),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AssistantTurnCompleted {
                truncated: true,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::OutputTruncated { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::BudgetExhausted { exhaustion })
                if exhaustion.limit == BudgetLimitKind::ModelTurns
        ));
    }

    #[tokio::test]
    async fn a_paused_turn_with_no_text_resumes_from_the_original_prompt() {
        struct PausingProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for PausingProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([Ok(ProviderEvent::Incomplete {
                        usage: None,
                        reason: qq_provider::IncompleteReason::Paused,
                    })]))
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

        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            PausingProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.last(), Some(&RuntimeEvent::Completed));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::OutputTruncated {
                continuation: 1,
                ..
            }
        )));
        // A paused turn with no text commits no assistant message, so there
        // is nothing to append a notice after: the resume request repeats
        // the original prompt alone and alternation holds.
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].messages().len(), 1);
        assert_eq!(requests[1].messages()[0].role(), Role::User);
    }

    #[tokio::test]
    async fn content_filter_stops_still_fail_as_provider_response() {
        struct FilteredProvider;

        impl Provider for FilteredProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "partial".to_owned(),
                    }),
                    Err(ProviderError::ResponseIncomplete(
                        "content_filter".to_owned(),
                    )),
                ]))
            }
        }

        let runtime = Runtime::new(FilteredProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderResponse,
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
    async fn a_toolless_request_rejects_provider_tool_calls_before_a_second_poll() {
        struct ToolOnToollessProvider {
            calls: Arc<AtomicUsize>,
        }

        impl Provider for ToolOnToollessProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "unexpected".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "unexpected".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        struct DenyAllGate;

        impl ToolGate for DenyAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Deny {
                    message: "tools disabled".to_owned(),
                }))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(
            ToolOnToollessProvider {
                calls: Arc::clone(&calls),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("summarize")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(DenyAllGate),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).without_tools(),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                message,
            }) if message.contains("declared no tools")
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallStarted { .. }
                | RuntimeEvent::ToolCallDenied { .. }
                | RuntimeEvent::ToolCallFinished { .. }
        )));
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
                            reasoning_tokens: None,
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
                    ..
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

    /// Counts stream calls and fails every one of them the scripted way.
    struct FailingProvider {
        calls: Arc<std::sync::atomic::AtomicU32>,
        failure: fn() -> Option<ProviderError>,
    }

    impl Provider for FailingProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match (self.failure)() {
                Some(error) => Box::pin(stream::once(async move { Err(error) })),
                None => Box::pin(stream::iter(Vec::new())),
            }
        }
    }

    /// The provider is the single retry owner. Whatever the failure — a
    /// transient error before any output, a stream that ends without a
    /// terminal event, or a failure after output has streamed — the run loop
    /// issues exactly one request per logical turn and reports the provider's
    /// error as-is. Amplification is therefore exactly 1.0 above the provider.
    #[tokio::test(start_paused = true)]
    async fn the_run_loop_never_resends_a_turn() {
        type Case = (fn() -> Option<ProviderError>, RunFailureKind, &'static str);
        let cases: [Case; 3] = [
            (
                || Some(overloaded()),
                RunFailureKind::ProviderUnavailable,
                "provider overloaded",
            ),
            (
                || Some(ProviderError::Transport("offline".to_owned())),
                RunFailureKind::ProviderTransport,
                "offline",
            ),
            (
                || None,
                RunFailureKind::ProviderProtocol,
                "ended without a terminal event",
            ),
        ];
        for (failure, kind, needle) in cases {
            let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let runtime = Runtime::new(
                FailingProvider {
                    calls: Arc::clone(&calls),
                    failure,
                },
                "gpt-test",
                256,
            )
            .unwrap();
            let events = runtime
                .run(RunCommand::new("hello"))
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{kind:?}: core must issue one request per turn"
            );
            assert!(
                matches!(
                    events.last(),
                    Some(RunEvent::Failed { kind: got, message })
                        if *got == kind && message.contains(needle)
                ),
                "{kind:?}: {events:?}"
            );
        }

        // After visible output, a failure ends the run with the partial turn
        // intact and still no resend.
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
        assert!(events.contains(&RunEvent::OutputTextDelta {
            text: "partial".to_owned()
        }));
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderUnavailable,
                ..
            })
        ));
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

        fn call(
            &self,
            name: String,
            arguments: String,
            _cancelled: Arc<AtomicBool>,
        ) -> HostCallFuture {
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
        let mut registry = MockMcpRegistry::returning(HostToolResult {
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
        .with_tool_host(Arc::new(registry));

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
        assert!(system.contains("external tool hosts"));
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
        let registry = MockMcpRegistry::returning(HostToolResult {
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
        .with_tool_host(Arc::new(registry));
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
        let registry = MockMcpRegistry::returning(HostToolResult {
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
        .with_tool_host(Arc::new(registry));
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
        let registry = MockMcpRegistry::returning(HostToolResult {
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
        .with_tool_host(Arc::new(registry));
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
        // Direct runs have no durable transcript, so history recall is
        // withheld the same way.
        assert!(
            !requests[0]
                .tools()
                .iter()
                .any(|spec| spec.name() == runtime::SEARCH_HISTORY_TOOL)
        );
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
        let spawner = Arc::new(StubSpawner::new(
            SpawnAgentOutcome {
                content: "x".repeat(tools::MAX_TOOL_RESULT_BYTES + 1024),
                is_error: false,
                spend: SpawnAgentSpend::NONE,
                session_id: None,
            },
            Arc::clone(&tasks),
        ));
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
            let spawner = Arc::new(StubSpawner::new(
                SpawnAgentOutcome {
                    content: "child answer".to_owned(),
                    is_error: false,
                    spend: SpawnAgentSpend::NONE,
                    session_id: None,
                },
                Arc::clone(&tasks),
            ));
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

    #[tokio::test]
    async fn children_receive_the_parents_remaining_budget_and_their_usage_rolls_up() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        // Turn one spawns a child and reports its own usage; the child then
        // reports usage large enough to spend the parent's token cap.
        struct MeteredSpawnProvider {
            turn: Mutex<usize>,
        }

        impl Provider for MeteredSpawnProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                let usage = Some(qq_provider::ProviderUsage {
                    input_tokens: 100,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 50,
                    reasoning_tokens: None,
                });
                if current == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "call_0".to_owned(),
                            name: "spawn_agent".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_0".to_owned(),
                            json: r#"{"task":"count the widgets"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "call_0".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage }),
                    ]))
                } else {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage }),
                    ]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let spawner = Arc::new(StubSpawner::new(
            SpawnAgentOutcome {
                content: "child answer".to_owned(),
                is_error: false,
                spend: SpawnAgentSpend {
                    cost_usd_nanos: Some(0),
                    usage: Some(TokenUsage {
                        input_tokens: 700,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 200,
                        reasoning_tokens: None,
                    }),
                },
                session_id: None,
            },
            Arc::new(Mutex::new(Vec::new())),
        ));
        let limits = RunLimits {
            max_total_tokens: Some(1_000),
            max_input_tokens: Some(1_000),
            max_model_turns: Some(10),
            ..RunLimits::default()
        };
        let runtime = Runtime::new(
            MeteredSpawnProvider {
                turn: Mutex::new(0),
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
                RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>))
                    .with_limits(limits, None),
            )
            .collect::<Vec<_>>()
            .await;

        // The child was admitted with what the parent had left after turn
        // one (150 tokens spent of 1_000 total, 100 of 1_000 input) and none
        // of the parent's per-run caps.
        let requests = spawner.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].budget.limits.max_total_tokens, Some(850));
        assert_eq!(requests[0].budget.limits.max_input_tokens, Some(900));
        assert_eq!(requests[0].budget.limits.max_model_turns, None);
        assert_eq!(requests[0].budget.limits.max_duration_ms, None);
        drop(requests);

        // Parent 150 + child 900 = 1_050 > 1_000: the child's tokens exhaust
        // the parent's total-token bound after the turn that ran it.
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::BudgetExhausted { exhaustion })
                if exhaustion.limit == BudgetLimitKind::TotalTokens
        ));
    }

    #[tokio::test]
    async fn a_spawn_the_parent_cannot_afford_is_refused_as_a_tool_error() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        // Turn one spends the whole input-token cap and then asks to spawn.
        struct ExhaustedSpawnProvider {
            turn: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ExhaustedSpawnProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request);
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
                            json: r#"{"task":"count the widgets"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "call_0".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed {
                            usage: Some(qq_provider::ProviderUsage {
                                input_tokens: 500,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                output_tokens: 10,
                                reasoning_tokens: None,
                            }),
                        }),
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
        let spawner = Arc::new(StubSpawner::new(
            SpawnAgentOutcome {
                content: "never runs".to_owned(),
                is_error: false,
                spend: SpawnAgentSpend::NONE,
                session_id: None,
            },
            Arc::new(Mutex::new(Vec::new())),
        ));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            ExhaustedSpawnProvider {
                turn: Mutex::new(0),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        // Input tokens are only observed after the turn, so the meter grants
        // the reserved final response; the spawn inside that turn's tool
        // loop must still be refused rather than handed a zero budget.
        let limits = RunLimits {
            max_input_tokens: Some(500),
            ..RunLimits::default()
        };
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("go")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>))
                    .with_limits(limits, None),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(
            spawner.requests.lock().unwrap().is_empty(),
            "no child was admitted"
        );
        let refusal = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ToolCallFinished {
                    result, is_error, ..
                } => Some((result.clone(), *is_error)),
                _ => None,
            })
            .expect("the spawn call settles as a tool result");
        assert!(refusal.1);
        assert!(
            refusal.0.contains("cannot afford a sub-agent") && refusal.0.contains("input_tokens"),
            "{}",
            refusal.0
        );
    }

    #[test]
    fn agent_prompt_teaches_delegation_only_when_spawn_agent_is_declared() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let without = agent_system_prompt(
            workspace,
            &tools::specs(),
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert!(!without.contains("spawn_agent"));
        assert!(!without.contains("Delegation:"));

        let mut specs = tools::specs();
        specs.push(tools::spawn_agent_spec(&[], &DelegationRoster::default()));
        let with = agent_system_prompt(
            workspace,
            &specs,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert!(with.contains("spawn_agent"));
        assert!(with.contains("Delegation:"));
        assert!(with.contains("independent questions"));
        assert!(with.contains("read-only sub-agent"));
        assert!(with.contains("Omit spawn_agent's model argument by default"));
        assert!(with.contains("configured worker model"));
        assert!(with.contains("persisted selected model"));
        assert!(with.contains("never guess, translate, or invent one"));
    }

    #[test]
    fn delegation_route_resolution_prefers_model_then_role_then_default() {
        use qq_protocol::{DelegationRole, DelegationRoster, DelegationRosterEntry};
        let entry = |route: &str, role| DelegationRosterEntry {
            route: route.to_owned(),
            role,
            note: None,
            context_window: None,
            max_output_tokens: None,
            relative_cost_permille: None,
        };
        let roster = DelegationRoster {
            roster: vec![
                entry("openai/fast", DelegationRole::Fast),
                entry("anthropic/balanced", DelegationRole::Balanced),
                entry("anthropic/balanced-2", DelegationRole::Balanced),
            ],
            default_role: DelegationRole::Balanced,
            max_depth: 1,
            write_children: false,
        };

        // Exact model wins and must be a roster route.
        assert_eq!(
            resolve_delegation_route(
                &roster,
                Some("openai/fast".to_owned()),
                Some(DelegationRole::Balanced)
            ),
            Ok(Some("openai/fast".to_owned()))
        );
        assert!(
            resolve_delegation_route(&roster, Some("openai/other".to_owned()), None)
                .unwrap_err()
                .contains("not on the delegation roster")
        );
        // Role maps to the first entry declaring it.
        assert_eq!(
            resolve_delegation_route(&roster, None, Some(DelegationRole::Balanced)),
            Ok(Some("anthropic/balanced".to_owned()))
        );
        // Default role when neither is given.
        assert_eq!(
            resolve_delegation_route(&roster, None, None),
            Ok(Some("anthropic/balanced".to_owned()))
        );
        // A role nobody declares is a tool error naming it.
        assert!(
            resolve_delegation_route(&roster, None, Some(DelegationRole::Strong))
                .unwrap_err()
                .contains("strong role")
        );

        // Without a roster the legacy path is untouched: any model passes
        // through for spawn-time validation, and role is refused.
        let none = DelegationRoster::default();
        assert_eq!(
            resolve_delegation_route(&none, Some("x/y".to_owned()), None),
            Ok(Some("x/y".to_owned()))
        );
        assert_eq!(resolve_delegation_route(&none, None, None), Ok(None));
        assert!(
            resolve_delegation_route(&none, None, Some(DelegationRole::Fast))
                .unwrap_err()
                .contains("no delegation roster")
        );
    }

    #[test]
    fn roster_prompt_names_the_current_model_and_each_route_with_relative_cost() {
        use qq_protocol::{DelegationRole, DelegationRoster, DelegationRosterEntry};
        let roster = DelegationRoster {
            roster: vec![
                DelegationRosterEntry {
                    route: "openai/fast".to_owned(),
                    role: DelegationRole::Fast,
                    note: Some("lookups, breadth".to_owned()),
                    context_window: Some(400_000),
                    max_output_tokens: None,
                    relative_cost_permille: Some(150),
                },
                DelegationRosterEntry {
                    route: "anthropic/same".to_owned(),
                    role: DelegationRole::Balanced,
                    note: None,
                    context_window: Some(200_000),
                    max_output_tokens: None,
                    relative_cost_permille: Some(1000),
                },
                DelegationRosterEntry {
                    route: "anthropic/strong".to_owned(),
                    role: DelegationRole::Strong,
                    note: None,
                    context_window: None,
                    max_output_tokens: None,
                    relative_cost_permille: Some(2_500),
                },
                DelegationRosterEntry {
                    route: "custom/unpriced".to_owned(),
                    role: DelegationRole::Strong,
                    note: None,
                    context_window: Some(1_500),
                    max_output_tokens: None,
                    relative_cost_permille: None,
                },
            ],
            default_role: DelegationRole::Balanced,
            max_depth: 1,
            write_children: false,
        };
        let text =
            runtime::delegation_roster_text("anthropic/current", Some(1_000_000), &roster).unwrap();
        assert_eq!(
            text,
            "         - You are running as anthropic/current (1M context). Roster (default role: balanced):\n\
             \x20          - fast: openai/fast — 400k context; ~15% of your cost; lookups, breadth\n\
             \x20          - balanced: anthropic/same — 200k context; same cost as you\n\
             \x20          - strong: anthropic/strong — ~2.5x your cost\n\
             \x20          - strong: custom/unpriced — 1500 context"
        );
        assert!(runtime::delegation_roster_text("x", None, &DelegationRoster::default()).is_none());

        // The prompt embeds it and switches to role-based guidance; the
        // legacy worker-model sentence goes away.
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let mut specs = tools::specs();
        specs.push(tools::spawn_agent_spec(&[], &roster));
        let prompt = agent_system_prompt(
            workspace,
            &specs,
            runtime::PromptSections {
                roster: Some(&text),
                ..runtime::PromptSections::default()
            },
            &instructions,
            None,
            None,
        );
        assert!(prompt.contains("Delegation:"));
        assert!(prompt.contains("Choose the sub-agent by spawn_agent's role argument"));
        assert!(prompt.contains("You are running as anthropic/current"));
        assert!(prompt.contains("- fast: openai/fast"));
        assert!(!prompt.contains("configured worker model"));
        assert!(prompt.contains("independent questions"));
    }

    /// A host serving many tools so the catalog is disclosed progressively.
    struct WideHost {
        count: usize,
        generation: Arc<std::sync::atomic::AtomicU64>,
        calls: Arc<Mutex<Vec<String>>>,
        failure: Option<HostCallError>,
        /// Advertised on every tool; must never change a policy decision.
        read_only_hint: bool,
        /// Whether the host declares configuration grants for its tools.
        granted: bool,
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
            if !self.granted {
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
            _cancelled: Arc<AtomicBool>,
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

    /// Scripts a sequence of turns, each a list of (tool name, arguments);
    /// an empty list completes with text.
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

    /// Regression for the `ext__` approval bypass: embedded-host tools once
    /// classified as `Unknown` and executed under every mode. Policy now
    /// classifies from the catalog effect, so an external call is denied
    /// under read-only and held under ask and supervised, and the host's
    /// `read_only` hint never changes the decision.
    #[tokio::test]
    async fn external_host_tools_obey_every_approval_mode_regardless_of_hints() {
        let directory = tempfile::tempdir().unwrap();
        for read_only_hint in [false, true] {
            for (mode, expected) in [
                (ApprovalMode::ReadOnly, Some(approval::POLICY_DENIED_RESULT)),
                (ApprovalMode::Ask, Some(approval::UNATTENDED_DENIED_RESULT)),
                (
                    ApprovalMode::Supervised,
                    Some(approval::UNATTENDED_DENIED_RESULT),
                ),
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
                        Arc::new(AtomicBool::new(false)),
                        Arc::new(StaticPolicyGate {
                            mode,
                            grants: approval::SessionGrants::default(),
                        }),
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
                    matches!(events.last(), Some(RuntimeEvent::Completed)),
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
                        r#"{"path":"note.txt","old_string":"before","new_string":"after"}"#
                            .to_owned(),
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
                Arc::new(AtomicBool::new(false)),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Full,
                    grants: approval::SessionGrants {
                        tools: ["edit_file".to_owned()].into_iter().collect(),
                        shell_prefixes: Vec::new(),
                    },
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed)),
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
                plan::AgentProfile::embedded(&runtime, workspace).with_exposed_tools(
                    (0..40).map(|i| format!("ext__wide__tool{i:02}")).collect(),
                ),
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
                        // Selected earlier in the same turn, so already usable.
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
        // Turn 1: static tools plus the selector, no external schema, and the
        // index in the system prompt.
        let first = tool_names(&requests[0]);
        assert!(first.contains(&catalog::SELECT_TOOLS_TOOL));
        assert!(!first.iter().any(|name| name.starts_with("ext__")));
        let system = requests[0].system().unwrap();
        assert!(system.contains("External tools (progressive)"));
        assert!(system.contains("ext__wide__tool07 — Deploy the service"));
        assert!(system.contains("host wide: 40 tools"));
        // An unpinned external call never reaches the host: the tool error
        // tells the model how to make it available.
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
        // Turn 2 selects, then calls the selected tool in the same turn.
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
        // Turn 3 carries the pinned schema.
        let third = tool_names(&requests[2]);
        assert!(third.contains(&"ext__wide__tool07"));
        assert_eq!(third.len(), first.len() + 1);
        // Turn 3 pins eight more; turn 4 sees them all, in pin order, and
        // nothing is duplicated.
        let fourth = tool_names(&requests[3]);
        assert_eq!(fourth.len(), first.len() + 9);
        let mut deduped = fourth.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), fourth.len());
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
                vec![ContentBlock::ToolCall {
                    id: "c1".to_owned(),
                    name: catalog::SELECT_TOOLS_TOOL.to_owned(),
                    arguments: serde_json::json!({"query": "x"}),
                }],
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
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
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
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
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
    async fn disclosed_skills_are_listed_and_loadable_only_for_guidance_capable_runs() {
        let directory = tempfile::tempdir().unwrap();
        for (path, content) in [
            (
                ".qq/skills/deploy/SKILL.md",
                "---\ndescription: How to deploy safely\n---\nRun the deploy checklist.\n",
            ),
            (".agents/skills/hidden/SKILL.md", "Compat only.\n"),
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
            assert!(!system.contains("hidden"), "compat roots are not disclosed");
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

        // A restricted run (no guidance) neither lists nor declares the loader.
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
        let restricted_plan =
            plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
                &restricted,
                std::fs::canonicalize(directory.path()).unwrap(),
            ))
            .unwrap();
        let events = restricted_plan
            .execute(
                vec![Message::user("summarize")],
                Arc::new(AtomicBool::new(false)),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::restricted(),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        let restricted_requests = restricted_requests.lock().unwrap();
        assert!(
            !restricted_requests[0]
                .system()
                .unwrap()
                .contains("Available skills")
        );
        assert!(!tool_names(&restricted_requests[0]).contains(&"load_skill"));
        assert_eq!(plan.descriptor().skills.disclosed, 1);
        assert_eq!(plan.descriptor().skills.indexed, 2);
    }
}
