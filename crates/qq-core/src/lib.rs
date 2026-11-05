#![forbid(unsafe_code)]

use std::{collections::HashMap, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

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
use thiserror::Error;

mod approval;
mod cancellation;
pub mod catalog;
pub mod context_source;
pub mod hosts;
mod input;
pub mod mentions;
pub mod output;
pub mod plan;
mod runtime;
mod sessions;
#[cfg(test)]
mod test_support;
mod tools;
pub mod undo;

#[cfg(windows)]
pub fn prepare_detached_spawn() -> std::io::Result<()> {
    tools::windows_job::root_job().map(|_| ())
}

#[doc(hidden)]
pub use tools::process_context::ProcessProducer;

pub use approval::bench_support as classify_bench;
#[doc(hidden)]
pub use sessions::bench_support as context_assembly_bench;
#[doc(hidden)]
pub use tools::bench_support as tool_bench;
pub use tools::network::{NetworkPolicy, host_grant_matches};
#[doc(hidden)]
pub use tools::output::bench_support as tool_output_bench;
mod workspace;

use runtime::{
    AGENT_PROMPT_VERSION, BUDGET_FINAL_RESPONSE_NOTICE, BudgetDecision, BudgetMeter, GateDecision,
    HistorySearcher, PendingToolCall, PreparedRequestWeight, PreparedStaticPrefix,
    ReadToolResultArgs, RuntimeEvent, RuntimeToolCall, SPAWN_UNAVAILABLE_RESULT, SearchHistoryArgs,
    SpawnAgentFuture, SpawnAgentOutcome, SpawnAgentSpend, SpawnRequest, SubagentSpawner, ToolGate,
    ToolGateFuture, TurnBlock, render_history_matches, render_tool_result,
};

pub use cancellation::RunCancellation;
pub use context_source::{
    ContextBudget, ContextBundle, ContextCache, ContextFetchFuture, ContextItem, ContextRequest,
    ContextSource, ContextSourceError, FailPolicy, MAX_CONTEXT_SOURCES,
};
pub use hosts::{
    EMBEDDED_TOOL_PREFIX, EmbeddedHostError, EmbeddedToolFuture, EmbeddedToolHandler,
    EmbeddedToolHost, EmbeddedToolHostBuilder, ExternalToolHost, HostCallError, HostCallFuture,
    HostCallObservation, HostCatalog, HostReadiness, HostShutdownFuture, HostTool, HostToolResult,
    MCP_TOOL_PREFIX, ObservedHostCallFuture, ObservedHostToolResult, ToolHints,
};
pub use runtime::{
    AUDIT_TOOL_CALL_THRESHOLD, ApprovedReadImage, ApprovedReadImages, AuditMode, AuditPolicy,
    AuditRequest, AuditVerdict, AuditedAction, BASE_ENV, BuiltinPreference, MAX_AUDIT_ACTION_BYTES,
    MAX_AUDIT_ANSWER_BYTES, MAX_AUDIT_CHILD_DURATION_MS, MAX_AUDIT_CHILD_TURNS,
    MAX_AUDIT_FINDING_BYTES, MAX_AUDIT_FINDINGS, MAX_PENDING_STEERING, ReadImageApprovalError,
    ReadImageContract, ReadImageFamily, ShellPolicy,
};
pub use sessions::{
    LoadedRuntime, MAX_CHILD_DEPTH, MAX_CHILD_DEPTH_CEILING, MAX_CONCURRENT_CHILDREN_PER_RUN,
    MAX_DELEGATION_ROSTER, MAX_DESCENDANTS_PER_ROOT, MAX_PENDING_PROMPTS, MAX_REPLAY_EVENTS,
    MAX_SPAWNED_CHILDREN_PER_RUN, PersistenceFault, PublishedEvent, PublishedEventStream,
    RunCompletion, RunResult, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadProgress,
    RuntimeLoadRequest, RuntimeLoadStage, RuntimeLoader, STORE_SCHEMA_VERSION, SessionEventStream,
    SessionRuntime, SessionRuntimeError, SessionRuntimeOptions, SideAnswer, SideQueryLimits,
    SlashCommandError, SpawnModelValidationFuture, WorkerRuntimeLoadFuture, run_cost,
};
pub use workspace::index::{
    IndexBudget, IndexDiff, IndexError, IndexOutcome, IndexStop, IndexedDirectory, IndexedFile,
    PartialIndex, WorkspaceIndex,
};
pub use workspace::skills::{MAX_INDEXED_SKILLS, MAX_SKILL_DESCRIPTION_BYTES};
pub use workspace::{SkillEntry, SkillIndex, SkillKind};

pub type RunStream = Pin<Box<dyn Stream<Item = RunEvent> + Send + 'static>>;
type RuntimeStream = Pin<Box<dyn Stream<Item = RuntimeEvent> + Send + 'static>>;

const MAX_TOOL_CALLS_PER_TURN: usize = 16;
const MAX_ADMITTED_TOOL_CALLS_PER_TURN: usize = 4 * MAX_TOOL_CALLS_PER_TURN;
const MAX_TOOL_CALLS_PER_SLICE: usize = 256;
pub(crate) const SLICE_CHECKPOINT_NOTICE: &str = "[QQ runtime notice; not a user instruction]\n\
This execution slice is at its safe tool-call boundary. Do not call tools in this reply. Write \
a short report: what is established (with path:line evidence), what is still unknown, and the \
one next action. QQ keeps this report and continues the same run with tools available again.";
const SLICE_CHECKPOINT_REJECTION: &str = "not executed: this reply was the slice checkpoint, \
which records progress without running tools; the run continues and tools are available on \
the next turn, so re-issue this call then";
pub(crate) const STALL_REPORT_NOTICE: &str = "[QQ runtime notice; not a user instruction]\n\
The last 64 tool calls changed nothing and produced no answer. Do not call tools in this reply. \
Write a short report: what is established (with path:line evidence), what is still unknown, and \
the one next action. QQ keeps this report and continues the same run with tools available \
again.";
pub(crate) const ROOT_STALL_REPORT_NOTICE: &str = "[QQ runtime notice; not a user instruction]\n\
The last 64 tool calls changed nothing. Do not call tools in this reply. In three lines: what \
you now know, the next action permitted by this task and run (path and what changes, or the \
finding for an analysis task), or the concrete blocker. Continue with that permitted action \
next turn.";
const STALL_REPORT_REJECTION: &str = "not executed: this reply was a progress report, which \
records what is established without running tools; the run continues and tools are available \
on the next turn, so re-issue this call then";
pub(crate) const SUBAGENT_FINAL_ANSWER_NOTICE: &str = "[QQ runtime notice; not a user \
instruction]\nYou have reported several times without new results, so this reply ends your \
run and is returned to the parent as your answer. Do not call tools; none will run. Answer the \
brief from what you have: the answer first, then the evidence as path:line, then what is still \
unknown.";
const SUBAGENT_FINAL_ANSWER_REJECTION: &str = "not executed: this reply was the sub-agent's \
final answer, which ends the run without running tools";
const SUMMARIZER_TOOL_REJECTION: &str = "not executed: this is a compaction request and tools \
are unavailable; reply with the summary only";
pub(crate) const SLICE_CONTINUATION_NOTICE: &str = "[QQ runtime notice; not a user \
instruction]\nContinue the task from the report above. Tools are available again. Do not stop \
at a progress summary: complete the user's request unless an explicit overall budget, \
cancellation, or genuine failure prevents it.";
const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_TOOL_CALL_ID_BYTES: usize = 1_024;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_RUN_MODEL_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUN_REASONING_BYTES: usize = 1024 * 1024;
const MAX_PARALLEL_READS: usize = 4;
const SHELL_OUTPUT_QUEUE_CAPACITY: usize = 16;
const CONTEXT_MESSAGE_FRAMING_BYTES: u64 = 16;
const CONTEXT_BLOCK_FRAMING_BYTES: u64 = 16;
const INTERRUPT_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe previous \
turn was interrupted by the user. Continue from where it stopped.";
const INTERRUPTED_TOOL_RESULT: &str =
    "Tool execution was interrupted before a durable result was recorded.";
pub const MAX_OUTPUT_CONTINUATIONS: u16 = 3;
pub const MAX_EMPTY_OUTPUT_RETRIES: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCeiling {
    pub tokens: u32,
    pub policy_bound: bool,
}
pub(crate) const OUTPUT_TRUNCATED_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe \
previous response was cut off at the output token limit. Continue exactly from where it \
stopped; do not repeat what was already written.";
pub(crate) const REASONING_TRUNCATED_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe \
previous response spent the whole output token limit on reasoning and produced no text or tool \
call. Reason less this time: take the next step now, or give the answer briefly.";
pub use qq_protocol::MAX_TURN_RETRIES;

pub use qq_provider::TurnRecoveryPolicy;

pub(crate) const TURN_RETRY_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe \
previous response was cut off by a transient provider error and QQ is retrying. Continue \
exactly from where it stopped; do not repeat what was already written.";
pub(crate) const EMPTY_TURN_PLACEHOLDER: &str = "[QQ runtime notice; not a user instruction]\nThe previous \
turn produced no model-visible text.";

enum StreamStep<T> {
    Interrupted,
    Event(Option<T>),
}

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

const SUBAGENT_DELIVERY_RETRY: std::time::Duration = std::time::Duration::from_millis(20);
const SUBAGENT_DELIVERY_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(1);

async fn steering_arrived(steering: &mut Option<runtime::SteeringReceiver>, handled: u64) {
    let Some(steering) = steering else {
        return std::future::pending().await;
    };
    if steering.peeked.is_some() || *steering.interrupts.borrow() > handled {
        return;
    }
    let runtime::SteeringReceiver {
        messages,
        interrupts,
        peeked,
    } = steering;
    tokio::select! {
        message = messages.recv() => match message {
            Some(message) => *peeked = Some(message),
            None => std::future::pending::<()>().await,
        },
        () = async {
            loop {
                if *interrupts.borrow() > handled {
                    return;
                }
                if interrupts.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        } => {}
    }
}

async fn deliver_children(
    spawner: &Arc<dyn SubagentSpawner>,
    boundary: Boundary,
    messages: &mut Vec<Message>,
    irreducible_message_bytes: &mut u64,
    budget: &mut BudgetMeter,
    stall: &mut runtime::StallScope,
) -> Result<Delivered, runtime::DeliveryError> {
    let delivered = spawner
        .deliver(boundary.turn_ordinal, boundary.reports)
        .await?;
    let mut count = Delivered::default();
    for child in &delivered {
        if child.interim {
            count.reports += 1;
        } else {
            count.answers += 1;
        }
        budget.charge_child(child.spend.usage, child.spend.cost_usd_nanos);
        budget.charge_tool_output(child.notice.len());
        if child.answered {
            stall.progress();
        }
        let notice = Message::user(child.notice.clone());
        *irreducible_message_bytes =
            irreducible_message_bytes.saturating_add(measure_message(&notice));
        messages.push(notice);
    }
    Ok(count)
}

#[derive(Debug, Clone, Copy)]
struct Boundary {
    turn_ordinal: u32,
    reports: runtime::ReportDelivery,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Delivered {
    answers: usize,
    reports: usize,
}

struct AppliedSteering {
    message_id: qq_protocol::MessageId,
    attachments: Vec<input::ResolvedAttachment>,
}

async fn apply_steering(
    stall: &mut runtime::StallScope,
    steering: &mut Option<runtime::SteeringReceiver>,
    messages: &mut Vec<Message>,
    irreducible_message_bytes: &mut u64,
    workspace: &workspace::Workspace,
    file_state: &Arc<workspace::FileState>,
) -> Option<Vec<AppliedSteering>> {
    let steering = steering.as_mut()?;
    let mut applied = Vec::new();
    while let Some(message) = steering
        .peeked
        .take()
        .or_else(|| steering.messages.try_recv().ok())
    {
        let has_files = message
            .input
            .iter()
            .any(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. }));
        let (text, attachments) = if has_files {
            let workspace = workspace.clone();
            let file_state = Arc::clone(file_state);
            let parts = message.input;
            let resolved = tokio::task::spawn_blocking(move || {
                input::resolve_blocking(&parts, &workspace, &file_state)
                    .map_err(|error| (input::render_text(&parts), error))
            })
            .await;
            match resolved {
                Ok(Ok(resolved)) => (resolved.text, resolved.attachments),
                Ok(Err((placeholder, error))) => (
                    format!(
                        "{}\n\n[QQ runtime notice; not a user instruction]\nAn attached file \
                         could not be read: {error}",
                        placeholder.trim()
                    ),
                    Vec::new(),
                ),
                Err(_) => (
                    "[QQ runtime notice; not a user instruction]\nA steering message's \
                     attachments could not be resolved."
                        .to_owned(),
                    Vec::new(),
                ),
            }
        } else {
            (
                input::render_text(&message.input).trim().to_owned(),
                Vec::new(),
            )
        };
        let user = Message::user(text);
        *irreducible_message_bytes =
            irreducible_message_bytes.saturating_add(measure_message(&user));
        messages.push(user);
        applied.push(AppliedSteering {
            message_id: message.message_id,
            attachments,
        });
    }
    if applied.is_empty() {
        return None;
    }
    stall.progress();
    Some(applied)
}

fn cite_spill(
    mut result: tools::ToolOutput,
    tool: &str,
    call: ToolCallId,
    stored: bool,
) -> tools::ToolOutput {
    match (&result.spill, stored) {
        (Some(spill), true) => {
            let handle = spill.handle(tool, call);
            tools::finalize_spill_marker(
                &mut result.model_text,
                Some(&handle),
                Some(spill.omitted_from_line),
            );
        }
        (Some(_), false) => result.spill = None,
        (None, _) => {}
    }
    result
}

fn note_audited_action(
    triggers: &mut runtime::AuditTriggers,
    actions: &mut Vec<runtime::AuditedAction>,
    keep_actions: bool,
    call: &RuntimeToolCall,
    result: &tools::ToolOutput,
) {
    triggers.tool_calls = triggers.tool_calls.saturating_add(1);
    if result
        .execution_context
        .as_ref()
        .is_some_and(|receipt| receipt.is_work())
    {
        triggers.non_read_shell = true;
    }
    match approval::classify(
        call.effect,
        &call.name,
        &call.arguments,
        &tools::network::NetworkPolicy::default(),
    ) {
        approval::ToolClass::Mutating if !result.is_error => triggers.mutated_files = true,
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
    parent_effort: Option<qq_provider::ReasoningEffort>,
) -> Result<(Option<String>, Option<qq_provider::ReasoningEffort>), String> {
    if delegation.roster.is_empty() {
        if role.is_some() {
            return Err(
                "no delegation roster is configured, so role cannot be used; omit role \
                        (and model) to use the configured worker model"
                    .to_owned(),
            );
        }
        return Ok((model, None));
    }
    if let Some(model) = model {
        let Some(entry) = delegation.entry_for_route(&model) else {
            return Err(format!(
                "model {model:?} is not on the delegation roster; choose a role instead or use \
                 one of the listed routes"
            ));
        };
        let effort = qq_protocol::child_reasoning_effort(entry, parent_effort);
        return Ok((Some(model), effort));
    }
    let role = role.unwrap_or(delegation.default_role);
    match delegation.entry_for_role(role) {
        Some(entry) => Ok((
            Some(entry.route.clone()),
            qq_protocol::child_reasoning_effort(entry, parent_effort),
        )),
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
    filter: &catalog::StaticFilter,
) -> (tools::ToolOutput, bool) {
    let arguments = match serde_json::from_str::<catalog::SelectToolsArgs>(arguments) {
        Ok(arguments) if arguments.query.trim().is_empty() => {
            return (tools::ToolOutput::refused("query must not be empty"), false);
        }
        Ok(arguments) => arguments,
        Err(error) => {
            return (
                tools::ToolOutput::refused(format!("invalid arguments: {error}")),
                false,
            );
        }
    };
    if catalog.exposure() != catalog::Exposure::Full && catalog.external_len() == 0 {
        return (
            tools::ToolOutput::refused("no external tools are available"),
            false,
        );
    }
    let limit = arguments.limit.clamp(1, catalog::MAX_SELECT_MATCHES);
    let matches = catalog.rank(&arguments.query, pins, limit, filter);
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
                if !catalog
                    .lookup(name)
                    .is_some_and(|entry| filter.allows(entry))
                {
                    return false;
                }
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

fn recover_pins(
    messages: &[Message],
    catalog: &catalog::ToolCatalog,
    pins: &mut catalog::PinSet,
    filter: &catalog::StaticFilter,
) {
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
                                    && filter.allows(entry)
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

struct CancelOnDrop(RunCancellation);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) struct RunCapabilities {
    spawner: Option<Arc<dyn SubagentSpawner>>,
    allow_guidance: bool,
    slash_is_literal: bool,
    allow_tools: bool,
    read_only: bool,
    unattended: bool,
    max_output_tokens: Option<u32>,
    limits: RunLimits,
    execution_started: Option<tokio::time::Instant>,
    pricing: Option<ModelPricing>,
    history: Option<Arc<dyn HistorySearcher>>,
    spills: Option<Arc<dyn runtime::SpillReader>>,
    steering: Option<runtime::SteeringReceiver>,
    compactor: Option<Arc<dyn runtime::InRunCompactor>>,
    audit_hook: Option<Arc<dyn runtime::AuditHook>>,
    tool_tasks: Option<tools::ToolTasks>,
    output: Option<Arc<output::CompiledOutputSchema>>,
    subagent: Option<runtime::SubagentAuthority>,
    stall_exempt: bool,
    summarizer: Option<plan::PromptPrefixKey>,
    inherited_effects: Vec<Option<catalog::EffectClass>>,
    cache_key: Option<Arc<str>>,
    mode: ApprovalMode,
    snapshots: bool,
}

impl RunCapabilities {
    pub(crate) fn user(spawner: Option<Arc<dyn SubagentSpawner>>) -> Self {
        Self {
            spawner,
            allow_guidance: true,
            slash_is_literal: false,
            allow_tools: true,
            read_only: false,
            unattended: false,
            max_output_tokens: None,
            limits: RunLimits::default(),
            execution_started: None,
            pricing: None,
            history: None,
            spills: None,
            steering: None,
            compactor: None,
            audit_hook: None,
            tool_tasks: None,
            output: None,
            subagent: None,
            stall_exempt: false,
            summarizer: None,
            inherited_effects: Vec::new(),
            cache_key: None,
            mode: ApprovalMode::Auto,
            snapshots: false,
        }
    }

    pub(crate) fn with_literal_slash(mut self, literal: bool) -> Self {
        self.slash_is_literal = literal;
        self
    }

    #[cfg(test)]
    pub(crate) fn without_tools(mut self) -> Self {
        self.allow_tools = false;
        self
    }

    pub(crate) fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub(crate) fn in_session(mut self, mode: ApprovalMode) -> Self {
        self.mode = mode;
        self.snapshots = true;
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

    pub(crate) fn with_cache_key(mut self, key: impl Into<Arc<str>>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    pub(crate) fn with_execution_started(mut self, started: tokio::time::Instant) -> Self {
        self.execution_started = Some(started);
        self
    }

    pub(crate) fn with_history(mut self, history: Arc<dyn HistorySearcher>) -> Self {
        self.history = Some(history);
        self
    }

    pub(crate) fn with_spills(mut self, spills: Arc<dyn runtime::SpillReader>) -> Self {
        self.spills = Some(spills);
        self
    }

    pub(crate) fn with_steering(mut self, steering: runtime::SteeringReceiver) -> Self {
        self.steering = Some(steering);
        self
    }

    pub(crate) fn with_compactor(mut self, compactor: Arc<dyn runtime::InRunCompactor>) -> Self {
        self.compactor = Some(compactor);
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

    pub(crate) fn with_output(mut self, output: Option<Arc<output::CompiledOutputSchema>>) -> Self {
        self.output = output;
        self
    }

    pub(crate) fn for_subagent(mut self, authority: runtime::SubagentAuthority) -> Self {
        self.subagent = Some(authority);
        self
    }

    pub(crate) fn stall_exempt(mut self) -> Self {
        self.stall_exempt = true;
        self
    }

    pub(crate) fn with_inherited_effects(
        mut self,
        effects: Vec<Option<catalog::EffectClass>>,
    ) -> Self {
        self.inherited_effects = effects;
        self
    }

    pub(crate) fn summarizer(mut self, key: plan::PromptPrefixKey) -> Self {
        self.summarizer = Some(key);
        self.output = None;
        self
    }

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
            unattended: false,
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
            execution_started: None,
            pricing: None,
            history: None,
            spills: None,
            steering: None,
            compactor: None,
            audit_hook: None,
            tool_tasks: None,
            output: None,
            subagent: None,
            stall_exempt: false,
            summarizer: None,
            inherited_effects: Vec::new(),
            cache_key: None,
            mode: ApprovalMode::Auto,
            snapshots: false,
        }
    }
}

struct StaticPolicyGate {
    mode: ApprovalMode,
    tools: std::collections::HashSet<String>,
    network: Arc<tools::network::NetworkPolicy>,
}

impl ToolGate for StaticPolicyGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let class = approval::classify(call.effect, &call.name, &call.arguments, &self.network);
        let decision = match approval::evaluate(self.mode, &class) {
            approval::PolicyDecision::Execute => GateDecision::Execute,
            approval::PolicyDecision::Deny { reason } => GateDecision::Deny {
                message: approval::deny_result(&reason),
            },
            approval::PolicyDecision::RequireApproval => GateDecision::Deny {
                message: approval::UNATTENDED_DENIED_RESULT.to_owned(),
            },
            approval::PolicyDecision::AskUser { .. } => GateDecision::Deny {
                message: approval::UNATTENDED_QUESTION_RESULT.to_owned(),
            },
        };
        Box::pin(std::future::ready(decision))
    }
}

struct ConfiguredToolGate(StaticPolicyGate);

impl ToolGate for ConfiguredToolGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        if !matches!(
            call.effect,
            catalog::EffectClass::ReadOnly | catalog::EffectClass::Interactive
        ) && !self.0.tools.contains(&call.name)
        {
            return Box::pin(std::future::ready(GateDecision::Deny {
                message: approval::UNATTENDED_DENIED_RESULT.to_owned(),
            }));
        }
        self.0.resolve(call)
    }
}

#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    model: Arc<str>,
    max_output_tokens: u32,
    output_ceiling: Option<OutputCeiling>,
    context_window: Option<u32>,
    reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub(crate) hosts: Arc<[Arc<dyn ExternalToolHost>]>,
    pub(crate) context_sources: Arc<[context_source::RegisteredSource]>,
    pub(crate) context_cache: Arc<ContextCache>,
    spawn_model_routes: Arc<[String]>,
    pub(crate) delegation: Arc<DelegationRoster>,
    pub(crate) audit: runtime::AuditPolicy,
    pub(crate) shell: Arc<runtime::ShellPolicy>,
    pub(crate) approved_read_images: Arc<runtime::ApprovedReadImages>,
    pub(crate) process_producer: Arc<ProcessProducer>,
    pub(crate) network: Arc<tools::network::NetworkPolicy>,
    pub(crate) turn_recovery: TurnRecoveryPolicy,
    pub(crate) tool_calls_per_slice: usize,
}

impl Runtime {
    #[must_use]
    pub const fn with_reasoning_effort(mut self, effort: qq_provider::ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    pub fn new(
        provider: impl Provider + 'static,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        Self::with_provider(Arc::new(provider), model, max_output_tokens)
    }

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
            output_ceiling: None,
            context_window: None,
            hosts: Arc::from([]),
            context_sources: Arc::from([]),
            context_cache: Arc::new(ContextCache::default()),
            spawn_model_routes: Arc::from([]),
            delegation: Arc::new(DelegationRoster::default()),
            audit: runtime::AuditPolicy::default(),
            reasoning_effort: None,
            shell: Arc::new(runtime::ShellPolicy::default()),
            approved_read_images: Arc::new(runtime::ApprovedReadImages::default()),
            process_producer: Arc::default(),
            network: Arc::new(tools::network::NetworkPolicy::default()),
            turn_recovery: TurnRecoveryPolicy::default(),
            tool_calls_per_slice: MAX_TOOL_CALLS_PER_SLICE,
        })
    }

    #[must_use]
    pub const fn with_turn_recovery(mut self, policy: TurnRecoveryPolicy) -> Self {
        self.turn_recovery = policy;
        self
    }

    #[cfg(test)]
    pub(crate) const fn with_tool_calls_per_slice(mut self, calls: usize) -> Self {
        self.tool_calls_per_slice = calls;
        self
    }

    pub(crate) fn summarize(
        &self,
        messages: Vec<Message>,
        system: Arc<str>,
        tools: Arc<[ToolSpec]>,
        cache_key: Option<Arc<str>>,
        max_output_tokens: u32,
        retry_owner: qq_provider::RetryOwner,
    ) -> impl Stream<Item = Result<runtime::SummarizeStep, runtime::SummarizeError>> + Send + '_
    {
        stream! {
        let mut messages = messages;
        let mut summary = String::new();
        let mut emitted_text_bytes = 0_usize;
        let mut total_usage = Some(TokenUsage::default());
        let mut ordinal = 0_u32;
        let mut continuations: u16 = 0;
        let mut fold = runtime::SummaryFold::new(messages.len().saturating_sub(1));
        let mut rejected_call_turn = false;
        'requests: loop {
            yield Ok(runtime::SummarizeStep::RequestStarting);
            let request =
                ModelRequest::new(Arc::clone(&self.model), messages.clone(), max_output_tokens)
                    .with_system(Arc::clone(&system))
                    .with_retry_owner(retry_owner);
            let request = match &cache_key {
                Some(key) => request.with_cache_key(Arc::clone(key)),
                None => request,
            };
            let request = if tools.is_empty() {
                request
            } else {
                request.with_tools(Arc::clone(&tools))
            };
            let request = match self.reasoning_effort {
                Some(effort) => request.with_reasoning_effort(effort),
                None => request,
            };
            let mut events = self.provider.stream(request);
            let mut text = String::new();
            let mut reasoning_seen = false;
            let mut truncated = None;
            let usage;
            struct SummarizerCall {
                id: String,
                name: String,
                arguments: String,
            }
            let mut calls: Vec<SummarizerCall> = Vec::new();
            let mut replay = None;
            loop {
                let Some(event) = events.next().await else {
                    yield Err(runtime::SummarizeError::Transient {
                        kind: RunFailureKind::ProviderTransport,
                        message: "summarizer stream ended without completing".to_owned(),
                        retry_after: None,
                    }); return;
                };
                match event {
                    Ok(ProviderEvent::Replay { data }) => replay = Some(data),
                    Ok(ProviderEvent::OutputTextDelta { text: delta }) => {
                        emitted_text_bytes = emitted_text_bytes.saturating_add(delta.len());
                        if emitted_text_bytes > MAX_RUN_MODEL_TEXT_BYTES
                        {
                            yield Err(runtime::SummarizeError::Rejected(
                                "summarizer output exceeded the run text bound".to_owned(),
                            )); return;
                        }
                        text.push_str(&delta);
                    }
                    Ok(
                        ProviderEvent::ReasoningStarted { .. }
                        | ProviderEvent::ReasoningDelta { .. }
                        | ProviderEvent::ReasoningCompleted { .. },
                    ) => reasoning_seen = true,
                    Ok(ProviderEvent::RefusalDelta { .. }) => {
                        yield Err(runtime::SummarizeError::Rejected(
                            "summarizer refused".to_owned(),
                        )); return;
                    }
                    Ok(ProviderEvent::ToolCallStarted { id, name }) => {
                        if rejected_call_turn {
                            yield Err(runtime::SummarizeError::Rejected(
                                "summarizer called a tool on two turns".to_owned(),
                            )); return;
                        }
                        if calls.len() >= MAX_ADMITTED_TOOL_CALLS_PER_TURN
                            || id.is_empty()
                            || id.len() > MAX_TOOL_CALL_ID_BYTES
                            || name.is_empty()
                            || name.len() > MAX_TOOL_NAME_BYTES
                            || calls.iter().any(|call| call.id == id)
                        {
                            yield Err(runtime::SummarizeError::Rejected(
                                "summarizer streamed a malformed tool call".to_owned(),
                            )); return;
                        }
                        calls.push(SummarizerCall {
                            id,
                            name,
                            arguments: String::new(),
                        });
                    }
                    Ok(ProviderEvent::ToolCallArgumentsDelta { id, json }) => {
                        let Some(call) = calls.iter_mut().find(|call| call.id == id) else {
                            yield Err(runtime::SummarizeError::Rejected(
                                "summarizer streamed arguments for an unknown call".to_owned(),
                            )); return;
                        };
                        if call.arguments.len().saturating_add(json.len()) > MAX_TOOL_ARGUMENT_BYTES
                        {
                            yield Err(runtime::SummarizeError::Rejected(
                                "summarizer tool arguments exceeded their bound".to_owned(),
                            )); return;
                        }
                        call.arguments.push_str(&json);
                    }
                    Ok(ProviderEvent::ToolCallCompleted { .. }) => {}
                    Ok(ProviderEvent::Completed { usage: reported }) => {
                        usage = reported.map(provider_usage);
                        break;
                    }
                    Ok(ProviderEvent::Incomplete { reason, usage: reported }) => {
                        usage = reported.map(provider_usage);
                        truncated = Some(reason);
                        break;
                    }
                    Err(error) => {
                        let rejected_before_work = matches!(&error,
                            qq_provider::ProviderError::Api { .. } | qq_provider::ProviderError::RetryAfter { .. } | qq_provider::ProviderError::ResponseFailed { kind: qq_provider::ProviderErrorKind::ContextExceeded, .. } | qq_provider::ProviderError::Configuration(_)
                            | qq_provider::ProviderError::CredentialsUnavailable(_))
                            && text.is_empty() && calls.is_empty() && replay.is_none()
                            && !reasoning_seen;
                        let kind = run_failure_kind(error.kind());
                        let message = error.to_string();
                        if rejected_before_work
                            && kind == RunFailureKind::ProviderContextExceeded
                            && fold.split(&mut messages)
                        {
                            ordinal += 1;
                            yield Ok(runtime::SummarizeStep::TurnCompleted {
                                ordinal,
                                message: Message::assistant(String::new()),
                                usage: Some(TokenUsage::default()),
                                truncated: false,
                            });
                            summary.clear();
                            continuations = 0;
                            continue 'requests;
                        }
                        if rejected_before_work {
                            yield Err(runtime::SummarizeError::RejectedBeforeWork { kind, message, retry_after: error.retry_after() });
                            return;
                        }
                        yield Err(if recoverable_turn_fault(kind) {
                            runtime::SummarizeError::Transient { kind, message, retry_after: error.retry_after() }
                        } else {
                            runtime::SummarizeError::Rejected(message)
                        }); return;
                    }
                }
            }
            ordinal += 1;
            let mut completed_blocks = Vec::with_capacity(calls.len() + 1);
            if !text.is_empty() {
                completed_blocks.push(ContentBlock::Text { text: text.clone() });
            }
            for call in &calls {
                let encoded = if call.arguments.trim().is_empty() { "{}" } else { &call.arguments };
                match serde_json::value::RawValue::from_string(encoded.to_owned()) {
                    Ok(arguments) => completed_blocks.push(ContentBlock::ToolCall {
                        id: call.id.clone(), name: call.name.clone(), arguments,
                    }),
                    Err(_) => completed_blocks.push(ContentBlock::Text {
                        text: format!("[Malformed summarizer call {} {}: {}]", call.id, call.name, call.arguments),
                    }),
                }
            }
            let completed = Message::new(Role::Assistant, completed_blocks);
            let completed = match &replay {
                Some(data) => completed.with_replay(data.clone()),
                None => completed,
            };
            yield Ok(runtime::SummarizeStep::TurnCompleted {
                ordinal, message: completed, usage, truncated: truncated.is_some(),
            });
            total_usage = match (total_usage, usage) {
                (Some(total), Some(turn)) => match sessions::add_usage(total, turn) {
                    Some(sum) => Some(sum),
                    None => {
                        yield Err(runtime::SummarizeError::Rejected(
                            "summarizer usage overflowed".to_owned(),
                        )); return;
                    }
                },
                (Some(_), None) | (None, Some(_)) | (None, None) => None,
            };
            if !calls.is_empty() {
                if truncated.is_some() {
                    yield Err(runtime::SummarizeError::Rejected(
                        "summarizer called a tool and was cut off".to_owned(),
                    )); return;
                }
                rejected_call_turn = true;
                summary.clear();
                continuations = 0;
                let mut blocks = Vec::with_capacity(calls.len() + 1);
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text { text });
                }
                let mut results = Vec::with_capacity(calls.len());
                for SummarizerCall {
                    id,
                    name,
                    arguments,
                } in calls
                {
                    let arguments = if arguments.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        arguments
                    };
                    let Ok(arguments) = serde_json::value::RawValue::from_string(arguments) else {
                        yield Err(runtime::SummarizeError::Rejected(
                            "summarizer streamed tool arguments that are not JSON".to_owned(),
                        )); return;
                    };
                    blocks.push(ContentBlock::ToolCall {
                        id: id.clone(),
                        name,
                        arguments,
                    });
                    results.push(ContentBlock::ToolResult {
                        call_id: id,
                        content: SUMMARIZER_TOOL_REJECTION.to_owned(),
                        is_error: true,
                    });
                }
                let assistant = Message::new(Role::Assistant, blocks);
                messages.push(match replay {
                    Some(data) => assistant.with_replay(data),
                    None => assistant,
                });
                messages.push(Message::tool_results(results));
                continue;
            }
            summary.push_str(&text);
            let Some(reason) = truncated else {
                match fold.advance(&mut summary) {
                    Ok(Some(next)) => {
                        messages = next;
                        continuations = 0;
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => { yield Err(error); return; }
                }
                yield Ok(runtime::SummarizeStep::Summary(summary)); return;
            };
            if text.trim().is_empty() && reason == qq_provider::IncompleteReason::OutputTokens {
                yield Err(runtime::SummarizeError::Rejected(format!(
                    "summarizer output was cut off at the output token limit ({max_output_tokens} tokens) without producing any visible text; the limit was spent on reasoning. Lower `reasoning_effort` or raise `max_output_tokens`"
                ))); return;
            }
            if continuations >= MAX_OUTPUT_CONTINUATIONS {
                yield Err(runtime::SummarizeError::Rejected(format!(
                    "summarizer output was cut off at the output token limit ({max_output_tokens} tokens) on {} consecutive turns",
                    u32::from(MAX_OUTPUT_CONTINUATIONS) + 1
                ))); return;
            }
            continuations += 1;
            messages.push(Message::assistant(text));
            messages.push(Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE));
        }
        }
    }

    #[must_use]
    pub fn with_context_window(mut self, context_window: Option<u32>) -> Self {
        self.context_window = context_window;
        self
    }

    #[must_use]
    pub const fn with_output_ceiling(mut self, output_ceiling: Option<OutputCeiling>) -> Self {
        self.output_ceiling = output_ceiling;
        self
    }

    #[must_use]
    pub fn resolved_model(&self) -> qq_protocol::ResolvedModel {
        self.embedded_resolved_model()
    }

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

    #[must_use]
    pub fn with_tool_host(mut self, host: Arc<dyn ExternalToolHost>) -> Self {
        let mut hosts = self.hosts.to_vec();
        hosts.push(host);
        self.hosts = hosts.into();
        self
    }

    #[must_use]
    pub fn with_context_source(mut self, source: Arc<dyn ContextSource>) -> Self {
        let mut sources = self.context_sources.to_vec();
        sources.push(context_source::RegisteredSource::new(source));
        self.context_sources = sources.into();
        self
    }

    #[must_use]
    pub fn with_context_cache(mut self, cache: Arc<ContextCache>) -> Self {
        self.context_cache = cache;
        self
    }

    #[must_use]
    pub fn with_spawn_model_routes(mut self, mut routes: Vec<String>) -> Self {
        routes.sort();
        routes.dedup();
        routes.retain(|route| !route.trim().is_empty());
        self.spawn_model_routes = routes.into();
        self
    }

    #[must_use]
    pub fn with_delegation(mut self, delegation: DelegationRoster) -> Self {
        self.delegation = Arc::new(delegation);
        self
    }

    #[must_use]
    pub const fn with_audit(mut self, audit: runtime::AuditPolicy) -> Self {
        self.audit = audit;
        self
    }

    #[must_use]
    pub fn with_process_producer(mut self, producer: Arc<ProcessProducer>) -> Self {
        self.process_producer = producer;
        self
    }

    #[must_use]
    pub fn with_approved_read_images(mut self, images: runtime::ApprovedReadImages) -> Self {
        self.approved_read_images = Arc::new(images);
        self
    }

    #[must_use]
    pub fn with_shell_policy(mut self, shell: runtime::ShellPolicy) -> Self {
        self.shell = Arc::new(shell);
        self
    }

    #[must_use]
    pub fn with_network_policy(mut self, network: NetworkPolicy) -> Self {
        self.network = Arc::new(network);
        self
    }

    pub fn run(&self, command: RunCommand) -> RunStream {
        self.run_in_workspace(
            command,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    pub fn run_in_workspace(&self, command: RunCommand, workspace: PathBuf) -> RunStream {
        public_run_stream(
            self.run_messages_in_workspace(
                vec![Message::user(input::render_text(command.input()))],
                workspace,
            ),
            self.context_window,
        )
    }

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
            RunCancellation::new(),
        )
    }

    fn run_messages_in_workspace_with_cancellation(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: RunCancellation,
    ) -> RuntimeStream {
        let mut capabilities = RunCapabilities::user(None);
        capabilities.unattended = true;
        self.run_loop_with_spawner(
            messages,
            workspace,
            cancelled,
            Arc::new(ConfiguredToolGate(StaticPolicyGate {
                mode: ApprovalMode::Auto,
                tools: std::collections::HashSet::new(),
                network: Arc::clone(&self.network),
            })),
            Arc::new(workspace::FileState::default()),
            capabilities,
        )
    }

    #[cfg(test)]
    pub(crate) fn run_loop(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: RunCancellation,
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
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: RunCancellation,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<workspace::FileState>,
        mut capabilities: RunCapabilities,
    ) -> RuntimeStream {
        let runtime = self.clone();
        Box::pin(stream! {
            let started = capabilities.execution_started.unwrap_or_else(tokio::time::Instant::now);
            capabilities.execution_started = Some(started);
            let deadline = runtime::RunDeadline::new(capabilities.limits, started);
            let _cancel_on_drop = CancelOnDrop(cancelled.clone());
            yield RuntimeEvent::Started;
            let preparation = workspace::prepare_workspace(
                workspace,
                cancelled.clone(),
            );
            tokio::pin!(preparation);
            let prepared = tokio::select! {
                biased;
                () = runtime::RunDeadline::wait(deadline) => {
                    cancelled.cancel();
                    let _ = preparation.await;
                    yield RuntimeEvent::BudgetExhausted {
                        exhaustion: deadline.expect("only a finite deadline wakes").exhaustion(),
                    };
                    return;
                }
                result = &mut preparation => result,
            };
            let (opened, _instructions) = match prepared {
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
            let profile_runtime = runtime.clone();
            let workspace_path = opened.path().to_owned();
            let mut compilation = tokio::task::spawn_blocking(move || {
                let profile = plan::AgentProfile::embedded(&profile_runtime, workspace_path);
                plan::CompiledAgentPlan::compile_blocking(profile)
            });
            let compiled = tokio::select! {
                biased;
                () = runtime::RunDeadline::wait(deadline) => {
                    cancelled.cancel();
                    let _ = compilation.await;
                    yield RuntimeEvent::BudgetExhausted {
                        exhaustion: deadline.expect("only a finite deadline wakes").exhaustion(),
                    };
                    return;
                }
                result = &mut compilation => result,
            };
            let compiled = match compiled {
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
            let gate = if capabilities.unattended {
                Arc::new(ConfiguredToolGate(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    tools: compiled.descriptor().tools.config_grants.iter().cloned().collect(),
                    network: Arc::clone(&compiled.runtime.network),
                })) as Arc<dyn ToolGate>
            } else { gate };
            let mut events = compiled.execute(messages, cancelled, gate, file_state, capabilities);
            while let Some(event) = events.next().await {
                match event {
                    RuntimeEvent::Started => {}
                    event => yield event,
                }
            }
        })
    }
}

impl plan::CompiledAgentPlan {
    pub fn run(self: &Arc<Self>, command: RunCommand) -> RunStream {
        let mut capabilities = RunCapabilities::user(None);
        capabilities.unattended = true;
        let tools = self
            .descriptor()
            .tools
            .config_grants
            .iter()
            .cloned()
            .collect();
        public_run_stream(
            self.execute(
                vec![Message::user(input::render_text(command.input()))],
                RunCancellation::new(),
                Arc::new(ConfiguredToolGate(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    tools,
                    network: Arc::clone(&self.runtime.network),
                })),
                Arc::new(workspace::FileState::default()),
                capabilities,
            ),
            self.runtime.context_window,
        )
    }
}

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
                        compaction: sessions::context::CompactionDisposition::Unsupported,
                    });
                    if let Some(message) = sessions::context::rejection_message(plan) {
                        drop(events);
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
                | RuntimeEvent::InRunCompacted { .. }
                | RuntimeEvent::ContextPruned { .. }
                | RuntimeEvent::ProviderOverflow { .. }
                | RuntimeEvent::ToolCallStarted { .. }
                | RuntimeEvent::ToolCallDenied { .. }
                | RuntimeEvent::ToolCallUnexecuted { .. }
                | RuntimeEvent::ToolCallAnswered { .. }
                | RuntimeEvent::ToolCallOutputDelta { .. }
                | RuntimeEvent::ToolCallFinished { .. }
                | RuntimeEvent::ToolResultsProjected { .. }
                | RuntimeEvent::SteeringApplied { .. }
                | RuntimeEvent::Interrupted { .. }
                | RuntimeEvent::Audited { .. }
                | RuntimeEvent::OutputTruncated { .. }
                | RuntimeEvent::TurnRetrying { .. }
                | RuntimeEvent::OutputRepairRequested { .. } => {}
                RuntimeEvent::Completed { .. } => {
                    drop(events);
                    yield RunEvent::Completed;
                    return;
                }
                RuntimeEvent::Failed { kind, message } => {
                    drop(events);
                    yield RunEvent::Failed { kind, message };
                    return;
                }
                RuntimeEvent::Paused { pause } => {
                    drop(events);
                    let (kind, message) = match pause.as_ref() {
                        qq_protocol::RunPause::ProviderRetry { kind, message, attempts, .. } =>
                            (*kind, format!("{message} (paused after {attempts} turn retries)")),
                        qq_protocol::RunPause::NoProgress { .. } =>
                            (RunFailureKind::Policy, pause.to_string()),
                    };
                    yield RunEvent::Failed { kind, message };
                    return;
                }
                RuntimeEvent::BudgetExhausted { exhaustion } => {
                    drop(events);
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

fn usable_conversation(messages: Vec<Message>) -> Vec<Message> {
    let mut conversation = Vec::with_capacity(messages.len());
    let mut skipped_empty = false;
    for message in messages {
        if !message.has_content() {
            skipped_empty = true;
            continue;
        }
        if skipped_empty && conversation.last().map(Message::role) == Some(message.role()) {
            conversation.push(match message.role() {
                Role::User => Message::assistant(EMPTY_TURN_PLACEHOLDER),
                Role::Assistant => Message::user(EMPTY_TURN_PLACEHOLDER),
            });
        }
        skipped_empty = false;
        conversation.push(message);
    }
    conversation
}

pub(crate) fn measure_messages(messages: &[Message]) -> u64 {
    messages.iter().fold(0_u64, |total, message| {
        total.saturating_add(measure_message(message))
    })
}

#[derive(Debug, Clone)]
struct RetainedResult {
    disposition: tools::ResultDisposition,
    result_digest: [u8; 32],
    human_input: bool,
    process_work: bool,
    model_text: String,
    is_error: bool,
    spill_handle: Option<String>,
    instruction_notice: bool,
}

impl RetainedResult {
    fn retain(output: &tools::ToolOutput, tool: &str, call: ToolCallId) -> Self {
        Self {
            disposition: output.disposition,
            result_digest: output.result_digest,
            human_input: false,
            process_work: output
                .execution_context
                .as_ref()
                .is_some_and(|receipt| receipt.is_work()),
            model_text: output.model_text.clone(),
            is_error: output.is_error,
            spill_handle: output.spill.as_ref().map(|spill| spill.handle(tool, call)),
            instruction_notice: false,
        }
    }

    const fn error(message: String) -> Self {
        Self {
            disposition: tools::ResultDisposition::Refused,
            result_digest: [0; 32],
            human_input: false,
            process_work: false,
            model_text: message,
            is_error: true,
            spill_handle: None,
            instruction_notice: false,
        }
    }

    fn instruction_notice(message: String) -> Self {
        let mut result = Self::error(message);
        result.instruction_notice = true;
        result
    }

    fn interrupted(message: String) -> Self {
        let mut result = Self::error(message);
        result.disposition = tools::ResultDisposition::Interrupted;
        result
    }

    const fn answered(result: String) -> Self {
        Self {
            disposition: tools::ResultDisposition::Refused,
            result_digest: [0; 32],
            human_input: true,
            process_work: false,
            model_text: result,
            is_error: false,
            spill_handle: None,
            instruction_notice: false,
        }
    }
}

pub(crate) fn measure_message(message: &Message) -> u64 {
    message.content().iter().fold(
        CONTEXT_MESSAGE_FRAMING_BYTES
            .saturating_add(message.replay().map_or(0, |replay| replay.len() as u64)),
        |total, block| {
            let content = match block {
                ContentBlock::Text { text } => u64::try_from(text.len()).unwrap_or(u64::MAX),
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => u64::try_from(id.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(name.len()).unwrap_or(u64::MAX))
                    .saturating_add(u64::try_from(arguments.get().len()).unwrap_or(u64::MAX)),
                ContentBlock::ToolResult {
                    call_id, content, ..
                } => u64::try_from(call_id.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(content.len()).unwrap_or(u64::MAX)),
            };
            total
                .saturating_add(CONTEXT_BLOCK_FRAMING_BYTES)
                .saturating_add(content)
        },
    )
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

const fn recoverable_turn_fault(kind: RunFailureKind) -> bool {
    matches!(
        kind,
        RunFailureKind::ProviderUnavailable
            | RunFailureKind::ProviderRateLimited
            | RunFailureKind::ProviderTransport
    )
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeConfigError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("maximum output tokens must be greater than zero")]
    ZeroMaxOutputTokens,
}

#[cfg(test)]
mod tests;
