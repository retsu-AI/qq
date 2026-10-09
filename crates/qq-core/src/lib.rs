//! Agent runtime, session behavior, tools, and persistence.
//!
//! # Embedding lifecycle
//!
//! Construct a [`Runtime`] from a [`qq_provider::Provider`], attach external
//! tools with [`Runtime::with_tool_host`], then asynchronously compile it with
//! [`LoadedRuntime::from_runtime`]. A [`RuntimeLoader`] supplies this immutable
//! plan to [`SessionRuntime::open`]. The host need not depend on a configuration
//! crate; [`Runtime::resolved_model`] records only capabilities it can establish.
//!
//! Resolve the workspace and create a session with [`qq_protocol::SessionCommand`],
//! subscribe before submitting a prompt, and consume committed [`qq_protocol::SessionEvent`]
//! values. In ask mode, respond to tool approvals through the same command lane.
//! A `RunFinished` event records the typed outcome; silence is not completion.
//! Call [`SessionRuntime::shutdown`] to cancel and drain owned work before closing
//! the store. The credential-free `examples/embed.rs` runs this entire lifecycle.
//! Compilation performs blocking filesystem/catalog work on bounded Tokio
//! blocking tasks. Cancellation of a compilation future does not stop work that
//! has already started; compilation is not a run and produces no tool side effects.

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
use sha2::{Digest, Sha256};
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
mod tools;

#[doc(hidden)]
pub use approval::bench_support as classify_bench;
#[doc(hidden)]
pub use sessions::bench_support as context_assembly_bench;
/// Entry points for the `tool_output` bench. Not a public API.
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

pub use approval::{ApprovalDelegate, shell_prefix_matches};
pub use cancellation::RunCancellation;
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
    BASE_ENV, BuiltinPreference, CheckpointFuture, CheckpointOutcome, CheckpointPhase,
    CheckpointRequest, CheckpointReviewer, CheckpointVerdict, MAX_AUDIT_ACTION_BYTES,
    MAX_AUDIT_ANSWER_BYTES, MAX_AUDIT_CHILD_DURATION_MS, MAX_AUDIT_CHILD_TURNS,
    MAX_AUDIT_FINDING_BYTES, MAX_AUDIT_FINDINGS, MAX_PENDING_STEERING, MAX_SHELL_ENV_ALLOWLIST,
    MAX_SHELL_ENV_NAMES, ShellPolicy, valid_env_name,
};
pub use sessions::{
    ApprovalDelegateSelection, ApprovalReviewer, CheckpointSelection, DelegateIdentity,
    GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, MAX_CHILD_DEPTH, MAX_CHILD_DEPTH_CEILING,
    MAX_CONCURRENT_CHILDREN_PER_RUN, MAX_DELEGATION_ROSTER, MAX_DESCENDANTS_PER_ROOT,
    MAX_GRANT_BYTES, MAX_PENDING_PROMPTS, MAX_REPLAY_EVENTS, MAX_REVIEW_ARGUMENT_BYTES,
    MAX_REVIEW_BRIEF_BYTES, MAX_REVIEW_RECENT_ACTIONS, MAX_SPAWNED_CHILDREN_PER_RUN,
    PersistenceFault, PublishedEvent, PublishedEventStream, RecentAction, ReviewDecision,
    ReviewFuture, ReviewOrigin, ReviewRequest, ReviewSpend, ReviewVerdict, RoutingSelection,
    RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadProgress, RuntimeLoadRequest, RuntimeLoadStage,
    RuntimeLoader, STORE_SCHEMA_VERSION, SessionEventStream, SessionRuntime, SessionRuntimeError,
    SessionRuntimeOptions, SideAnswer, SideQueryLimits, SlashCommandError,
    SpawnModelValidationFuture, TaskRouter, TaskRoutingFuture, WorkerRuntimeLoadFuture,
    WorkspaceGrantAuthority, WorkspaceGrantSeed, run_cost,
};
/// Merkle index over the workspace tree the tools see: the change-detection
/// primitive for run-snapshot checkpoints (`docs/plans/run-snapshots.md`).
pub use workspace::index::{
    IndexBudget, IndexDiff, IndexError, IndexOutcome, IndexStop, IndexedDirectory, IndexedFile,
    PartialIndex, WorkspaceIndex,
};
pub use workspace::skills::{MAX_INDEXED_SKILLS, MAX_SKILL_DESCRIPTION_BYTES};
pub use workspace::{SkillEntry, SkillIndex, SkillKind};

pub type RunStream = Pin<Box<dyn Stream<Item = RunEvent> + Send + 'static>>;
type RuntimeStream = Pin<Box<dyn Stream<Item = RuntimeEvent> + Send + 'static>>;

/// Tool calls one model turn may execute. Calls past this cap are admitted
/// into the transcript with a not-executed error result so the model can
/// re-issue them, and the run continues.
const MAX_TOOL_CALLS_PER_TURN: usize = 16;
/// Tool calls one model turn may name at all; beyond this the provider stream
/// is treated as a protocol violation.
const MAX_ADMITTED_TOOL_CALLS_PER_TURN: usize = 4 * MAX_TOOL_CALLS_PER_TURN;
// A runaway-loop backstop for one internal execution slice, not a task
// completion limit. Before a new model turn can exceed this ceiling, QQ asks
// for a checkpoint reply, persists it, resets the counter, and continues the
// same run. Tools stay declared on that turn: a call the model makes anyway is
// admitted with a not-executed result instead of failing the run, because the
// persisted turn is the boundary, not the model's obedience. An empty reply is
// a missed report, not a failure (ADR-0054 § 2).
const MAX_TOOL_CALLS_PER_SLICE: usize = 256;
// The checkpoint and continuation notices are messages in the conversation,
// not system-prompt text, so the cached prefix survives the seam. They are
// replayed from `model_turns.notice` (`runtime::TurnNotice`); their wording is
// part of every stored run that carried them.
pub(crate) const SLICE_CHECKPOINT_NOTICE: &str = "[QQ runtime notice; not a user instruction]\n\
This execution slice is at its safe tool-call boundary. Do not call tools in this reply. Write \
a short report: what is established (with path:line evidence), what is still unknown, and the \
one next action. QQ keeps this report and continues the same run with tools available again.";
const SLICE_CHECKPOINT_REJECTION: &str = "not executed: this reply was the slice checkpoint, \
which records progress without running tools; the run continues and tools are available on \
the next turn, so re-issue this call then";
/// The stall report (ADR-0054 § 2) is the same kind of turn as the slice
/// checkpoint, under its own wording: it fires after calls that changed
/// nothing, not at a fixed slice boundary. It asks for the same report.
pub(crate) const STALL_REPORT_NOTICE: &str = "[QQ runtime notice; not a user instruction]\n\
The last 64 tool calls changed nothing and produced no answer. Do not call tools in this reply. \
Write a short report: what is established (with path:line evidence), what is still unknown, and \
the one next action. QQ keeps this report and continues the same run with tools available \
again.";
const STALL_REPORT_REJECTION: &str = "not executed: this reply was a progress report, which \
records what is established without running tools; the run continues and tools are available \
on the next turn, so re-issue this call then";
/// A sub-agent's last turn (ADR-0054 § 3). Its tools stay declared with
/// `ToolChoice::None`; the turn ends the run whatever it returns.
pub(crate) const SUBAGENT_FINAL_ANSWER_NOTICE: &str = "[QQ runtime notice; not a user \
instruction]\nYou have reported several times without new results, so this reply ends your \
run and is returned to the parent as your answer. Do not call tools; none will run. Answer the \
brief from what you have: the answer first, then the evidence as path:line, then what is still \
unknown.";
const SUBAGENT_FINAL_ANSWER_REJECTION: &str = "not executed: this reply was the sub-agent's \
final answer, which ends the run without running tools";
/// A compaction summarizer declares the session's tools only so its request
/// shares the provider cache; it never runs one (ADR-0056 § 5).
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
/// Most times one run raises its output cap after a turn hit the limit with
/// nothing visible streamed. Such a turn was spent entirely on hidden
/// reasoning; resending the same request would only spend it again, so the
/// retry doubles the cap (bounded by the model's ceiling) and a second empty
/// turn settles the run with the cause named.
pub const MAX_EMPTY_OUTPUT_RETRIES: u16 = 1;

/// The highest output cap the empty-truncation recovery may raise a turn to:
/// the model's catalog limit, lowered to a managed `policy.max_output_tokens`
/// when one is set below it. `policy_bound` says which one binds, so the
/// terminal diagnostic names a remedy that can work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCeiling {
    pub tokens: u32,
    pub policy_bound: bool,
}
/// Sent after a truncated turn is committed so the model resumes rather than
/// restarts. Assistant/user alternation is preserved because the partial
/// assistant message precedes it.
pub(crate) const OUTPUT_TRUNCATED_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe \
previous response was cut off at the output token limit. Continue exactly from where it \
stopped; do not repeat what was already written.";
/// Most retries one turn may spend on a transient provider fault after the
/// stream has started (the provider owns retries before that, ADR-0005). The
/// count resets when a turn completes, so a long run survives many isolated
/// blips while a provider that is down settles the run `paused` in minutes.
pub use qq_protocol::MAX_TURN_RETRIES;

/// Backoff between turn retries. The defaults are minute-scale because the
/// provider's own sub-minute ledger has already been spent on anything that
/// reaches the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRecoveryPolicy {
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for TurnRecoveryPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl TurnRecoveryPolicy {
    /// Exponential backoff from `base_delay`, doubling per retry, capped at
    /// `max_delay` (clamped to at least the base).
    #[must_use]
    pub fn new(base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            base_delay,
            max_delay: max_delay.max(base_delay),
        }
    }

    /// The sleep before retry `attempt` (1-based).
    #[must_use]
    pub fn delay(self, attempt: u16) -> Duration {
        let doublings = u32::from(attempt.saturating_sub(1)).min(31);
        self.base_delay
            .saturating_mul(1_u32 << doublings)
            .min(self.max_delay)
    }
}
/// Sent after a partial turn is committed when the provider fault cut the
/// model off mid-reply. Alternation holds because the partial assistant
/// message precedes it.
pub(crate) const TURN_RETRY_CONTINUE_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nThe \
previous response was cut off by a transient provider error and QQ is retrying. Continue \
exactly from where it stopped; do not repeat what was already written.";
/// Fills a skipped empty assistant turn so the follow-up user message does
/// not sit next to the previous user message. Providers require alternation.
pub(crate) const EMPTY_TURN_PLACEHOLDER: &str = "[QQ runtime notice; not a user instruction]\nThe previous \
turn produced no model-visible text.";

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

/// How long a parent waiting for answers first pauses before retrying a
/// settled child whose spend is not yet readable (its descendants are
/// settling); the pause doubles up to `SUBAGENT_DELIVERY_RETRY_MAX`.
const SUBAGENT_DELIVERY_RETRY: std::time::Duration = std::time::Duration::from_millis(20);
const SUBAGENT_DELIVERY_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// Resolves when steering arrives (the message is kept for the next
/// boundary) or an interrupt newer than `handled` is requested; pending
/// forever for runs without steering.
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

/// Commits every settled detached child's answer for the parent's turn
/// `turn_ordinal` and appends each as a runtime notice, after the boundary's
/// steering (ADR-0054 § 4). The store commits before the message joins
/// context; each answer is charged once here. Only a child that answered is
/// progress (ADR-0054 § 1): a failed or cancelled child's notice is evidence
/// for the parent, not output, exactly as a blocking spawn's error result is
/// not progress, and neither is an interim report. Returns how many answers
/// and how many interim reports were delivered.
async fn deliver_children(
    spawner: &Arc<dyn SubagentSpawner>,
    boundary: Boundary,
    messages: &mut Vec<Message>,
    irreducible_message_bytes: &mut u64,
    budget: &mut BudgetMeter,
    stall: &mut runtime::StallScope,
    checkpoint: Option<&mut runtime::CheckpointContext>,
) -> Result<Delivered, runtime::DeliveryError> {
    let delivered = spawner
        .deliver(boundary.turn_ordinal, boundary.reports)
        .await?;
    let mut checkpoint = checkpoint;
    let mut count = Delivered::default();
    for child in &delivered {
        if child.interim {
            count.reports += 1;
        } else {
            count.answers += 1;
        }
        budget.charge_child(child.spend.usage, child.spend.cost_usd_nanos);
        // Answers are output the parent asked for, like tool results: they
        // count against its tool-output bound. The store bounded each
        // boundary's notices together.
        budget.charge_tool_output(child.notice.len());
        if child.answered {
            stall.progress();
        }
        // A delivered answer is evidence the final review weighs, exactly
        // as a blocking spawn's result was.
        if let Some(context) = checkpoint.as_deref_mut() {
            context.record(child.notice.clone());
        }
        let notice = Message::user(child.notice.clone());
        *irreducible_message_bytes =
            irreducible_message_bytes.saturating_add(measure_message(&notice));
        messages.push(notice);
    }
    Ok(count)
}

/// The boundary a delivery is for: the parent turn whose request carries
/// it, and whether running children's reports come with it.
#[derive(Debug, Clone, Copy)]
struct Boundary {
    turn_ordinal: u32,
    reports: runtime::ReportDelivery,
}

/// What one boundary delivery added to the parent's context.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Delivered {
    answers: usize,
    reports: usize,
}

/// One steering message the loop has injected: its id and the files it read.
struct AppliedSteering {
    message_id: qq_protocol::MessageId,
    attachments: Vec<input::ResolvedAttachment>,
}

/// Drains every steering message that is ready and appends each as a user
/// message; applying any restarts the stall count. Returns what was
/// applied, in order, or `None` when nothing was pending. Never waits for
/// more steering: messages that arrive after the drain wait for the next
/// boundary. A message with file parts reads them
/// here — off the executor, through the plan's workspace — so the model sees
/// the bytes as they are at the boundary and the store can keep them as
/// this message's attachments. A file that cannot be read is reported to the
/// model in place of the attachment rather than failing the run: the user's
/// text still lands, and the message names what was missing.
async fn apply_steering(
    stall: &mut runtime::StallScope,
    steering: &mut Option<runtime::SteeringReceiver>,
    messages: &mut Vec<Message>,
    irreducible_message_bytes: &mut u64,
    mut checkpoint: Option<&mut runtime::CheckpointContext>,
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
        if let Some(context) = checkpoint.as_deref_mut() {
            context.steer(&text);
        }
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
    // An applied steer is a progress event: the user just gave the run
    // something new to act on (ADR-0054 § 1).
    stall.progress();
    Some(applied)
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
/// Names the spill in the result's marker when a session store will keep
/// it, and drops the spill otherwise so the marker keeps saying `not
/// stored`. The handle's digest is of the complete text, known here.
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
    // Classification here only asks "was it a mutation or a non-read shell";
    // the network policy is irrelevant to that, so the default is enough.
    match approval::classify(
        call.effect,
        &call.name,
        &call.arguments,
        &tools::network::NetworkPolicy::default(),
    ) {
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
) -> (tools::ToolOutput, bool) {
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

struct CancelOnDrop(RunCancellation);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
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
    routing_spend: Option<qq_protocol::CheckpointSpend>,
    execution_started: Option<tokio::time::Instant>,
    pricing: Option<ModelPricing>,
    /// Full-transcript recall for `search_history`. Session runs install one;
    /// direct runs have no durable history to search.
    history: Option<Arc<dyn HistorySearcher>>,
    /// Stored complete tool outputs for `read_tool_result`. Session runs
    /// install one and keep spills; direct runs keep none, so their markers
    /// say `not stored`.
    spills: Option<Arc<dyn runtime::SpillReader>>,
    /// Steering input from the session layer. Direct runs have none.
    steering: Option<runtime::SteeringReceiver>,
    /// Summarizes this run's own earlier turns when a later turn would not
    /// fit the window even after stubbing. Session prompt runs install one;
    /// direct runs, children of the summarizer, and internal runs have none.
    compactor: Option<Arc<dyn runtime::InRunCompactor>>,
    /// Audits the candidate final answer of a root run. Session roots install
    /// one; children, internal runs, and direct runs have none.
    audit_hook: Option<Arc<dyn runtime::AuditHook>>,
    tool_tasks: Option<tools::ToolTasks>,
    /// The typed-output contract of a prompt submitted with one. The final
    /// answer is validated at the completion boundary and repaired within
    /// the contract's turn allowance. Compaction and children have none.
    output: Option<Arc<output::CompiledOutputSchema>>,
    /// Set for a model-spawned child task: its prompt says a parent is
    /// waiting on it (ADR-0054 § 5).
    subagent: Option<runtime::SubagentAuthority>,
    /// Audit children keep no stall count: they are already bounded at a
    /// few turns (ADR-0054 § 1).
    stall_exempt: bool,
    /// Set for a compaction summarizer: the prompt prefix of the session's
    /// prompt runs. The request declares that tool list under that system
    /// prompt, so it reads the provider cache those runs wrote; every call is
    /// rejected unexecuted (ADR-0056 § 5).
    summarizer: Option<plan::PromptPrefixKey>,
    /// The stored effect of each tool result in the messages the run starts
    /// with, in block order (`None` for rows that predate the effect
    /// column). The live overflow prune classifies inherited results by
    /// these, exactly as assembly does, so a prune seam replays byte for
    /// byte. Direct runs have none and fall back to the built-in names.
    inherited_effects: Vec<Option<catalog::EffectClass>>,
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
            routing_spend: None,
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
        }
    }

    /// Marks a leading slash as already normalized from the durable `//`
    /// escape. The message is provider-ready and must not be reinterpreted as
    /// a guidance invocation.
    pub(crate) fn with_literal_slash(mut self, literal: bool) -> Self {
        self.slash_is_literal = literal;
        self
    }

    #[cfg(test)]
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

    /// Marks a model-spawned child task, so its prompt carries the sub-agent
    /// section for its authority.
    pub(crate) fn for_subagent(mut self, authority: runtime::SubagentAuthority) -> Self {
        self.subagent = Some(authority);
        self
    }

    /// Exempts the run from stall reports: an audit child, bounded already.
    pub(crate) fn stall_exempt(mut self) -> Self {
        self.stall_exempt = true;
        self
    }

    /// Makes the run a compaction summarizer whose request carries the
    /// prompt prefix `key`: the same system prompt and tool list as the
    /// session's prompt runs. Context sources and an output contract are
    /// never applied to it, it keeps no stall count, and every call it makes
    /// is rejected unexecuted; a second turn that calls a tool fails the run.
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
            routing_spend: None,
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
        }
    }
}

struct StaticPolicyGate {
    mode: ApprovalMode,
    /// Workspace-configured grants (today: MCP allowlist entries by exact
    /// namespaced name). Mode still wins: read-only denies granted tools.
    grants: approval::SessionGrants,
    network: Arc<tools::network::NetworkPolicy>,
}

impl ToolGate for StaticPolicyGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let class = approval::classify(call.effect, &call.name, &call.arguments, &self.network);
        let decision = match approval::evaluate(self.mode, &call.name, &class, &self.grants) {
            approval::PolicyDecision::Execute => GateDecision::Execute,
            approval::PolicyDecision::Deny { reason } => GateDecision::Deny {
                message: approval::deny_result(&reason),
            },
            approval::PolicyDecision::Forbidden { rules } => GateDecision::Deny {
                message: approval::forbidden_result(&rules),
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

/// Runs protocol commands against a configured model provider.
#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    model: Arc<str>,
    max_output_tokens: u32,
    /// The most the empty-truncation recovery may raise a turn's cap to: the
    /// model's catalog limit, bounded by policy. `None` (no catalog limit, or
    /// an embedded runtime) means the cap cannot be raised past
    /// `max_output_tokens`.
    output_ceiling: Option<OutputCeiling>,
    context_window: Option<u32>,
    reasoning_effort: Option<qq_provider::ReasoningEffort>,
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
    /// Mandatory, non-recursive post-result and final-candidate reviewer.
    pub(crate) checkpoint: Option<Arc<dyn runtime::CheckpointReviewer>>,
    pub(crate) checkpoint_identity: Option<Arc<str>>,
    pub(crate) task_router: Option<Arc<dyn sessions::TaskRouter>>,
    /// Environment allowlist and built-in preference for `shell` calls.
    pub(crate) shell: Arc<runtime::ShellPolicy>,
    pub(crate) network: Arc<tools::network::NetworkPolicy>,
    pub(crate) turn_recovery: TurnRecoveryPolicy,
    /// Who settles the calls the session's approval mode holds.
    pub(crate) approval_delegate: approval::ApprovalDelegate,
    /// Identity of the first approval delegate (Jev) for this runtime's held
    /// calls; `None` means it is off.
    pub(crate) approval_delegate_identity: Option<Arc<str>>,
}

impl Runtime {
    /// Pins effort for every provider request, including continuation turns.
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
            output_ceiling: None,
            context_window: None,
            hosts: Arc::from([]),
            context_sources: Arc::from([]),
            context_cache: Arc::new(ContextCache::default()),
            spawn_model_routes: Arc::from([]),
            delegation: Arc::new(DelegationRoster::default()),
            audit: runtime::AuditPolicy::default(),
            checkpoint: None,
            checkpoint_identity: None,
            task_router: None,
            reasoning_effort: None,
            shell: Arc::new(runtime::ShellPolicy::default()),
            network: Arc::new(tools::network::NetworkPolicy::default()),
            turn_recovery: TurnRecoveryPolicy::default(),
            approval_delegate: approval::ApprovalDelegate::default(),
            approval_delegate_identity: None,
        })
    }

    /// Sets the backoff between turn retries after a transient provider
    /// fault. The retry count itself is fixed at [`MAX_TURN_RETRIES`].
    #[must_use]
    pub const fn with_turn_recovery(mut self, policy: TurnRecoveryPolicy) -> Self {
        self.turn_recovery = policy;
        self
    }

    /// Sets who settles held approvals: the configured reviewer under the
    /// modes that consult it, or a human for everything. Inert without a
    /// reviewer installed on the session runtime.
    #[must_use]
    pub const fn with_approval_delegate(mut self, delegate: approval::ApprovalDelegate) -> Self {
        self.approval_delegate = delegate;
        self
    }

    /// Names the first approval delegate for held calls (Jev), recorded in
    /// the plan descriptor; `ReviewRequest::jev_approval` is set when it is
    /// present. Inert unless the installed reviewer composes that delegate.
    #[must_use]
    pub fn with_approval_delegate_identity(mut self, identity: Option<Arc<str>>) -> Self {
        self.approval_delegate_identity = identity;
        self
    }

    /// The summarizer request of an in-run compaction, under the run's own
    /// system prompt and tools (ADR-0056 § 5). It is continued up to
    /// `MAX_OUTPUT_CONTINUATIONS` times when the reply is cut at the output
    /// limit, exactly as the run loop and the between-run path do; a cut turn
    /// resumes mid-token, so its continuation is appended verbatim. A turn
    /// that calls tools gets a rejection result for each call and is asked
    /// again once, dropping what it wrote; a second such turn, a refusal, a
    /// protocol violation, or a transport failure is an error naming it, and
    /// the caller settles the compaction run failed. Returns the joined text
    /// and the summed usage. The provider owns retries as for any request.
    pub(crate) async fn summarize(
        &self,
        messages: Vec<Message>,
        system: Arc<str>,
        tools: Arc<[ToolSpec]>,
        max_output_tokens: u32,
    ) -> Result<(String, Option<TokenUsage>), String> {
        let mut messages = messages;
        let mut summary = String::new();
        let mut total_usage: Option<TokenUsage> = None;
        let mut continuations: u16 = 0;
        // One turn of calls is answered with rejections so the model can
        // still write the summary; a second fails the step.
        let mut rejected_call_turn = false;
        loop {
            // The run's own system prompt and tools, so this request shares
            // the run's provider cache (ADR-0056 § 5). Calls are never run.
            let request =
                ModelRequest::new(Arc::clone(&self.model), messages.clone(), max_output_tokens)
                    .with_system(Arc::clone(&system));
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
            let mut truncated = None;
            let mut usage = None;
            // Calls in this turn, in order, and the turn's replay data: a
            // rejected-call turn is sent back exactly as it was produced.
            struct SummarizerCall {
                id: String,
                name: String,
                arguments: String,
            }
            let mut calls: Vec<SummarizerCall> = Vec::new();
            let mut replay = None;
            loop {
                let Some(event) = events.next().await else {
                    return Err("summarizer stream ended without completing".to_owned());
                };
                match event {
                    Ok(ProviderEvent::Replay { data }) => replay = Some(data),
                    Ok(ProviderEvent::OutputTextDelta { text: delta }) => {
                        if summary
                            .len()
                            .saturating_add(text.len())
                            .saturating_add(delta.len())
                            > MAX_RUN_MODEL_TEXT_BYTES
                        {
                            return Err("summarizer output exceeded the run text bound".to_owned());
                        }
                        text.push_str(&delta);
                    }
                    Ok(
                        ProviderEvent::ReasoningStarted { .. }
                        | ProviderEvent::ReasoningDelta { .. }
                        | ProviderEvent::ReasoningCompleted { .. },
                    ) => {}
                    Ok(ProviderEvent::RefusalDelta { .. }) => {
                        return Err("summarizer refused".to_owned());
                    }
                    Ok(ProviderEvent::ToolCallStarted { id, name }) => {
                        if rejected_call_turn {
                            return Err("summarizer called a tool on two turns".to_owned());
                        }
                        if calls.len() >= MAX_ADMITTED_TOOL_CALLS_PER_TURN
                            || id.is_empty()
                            || id.len() > MAX_TOOL_CALL_ID_BYTES
                            || name.is_empty()
                            || name.len() > MAX_TOOL_NAME_BYTES
                            || calls.iter().any(|call| call.id == id)
                        {
                            return Err("summarizer streamed a malformed tool call".to_owned());
                        }
                        calls.push(SummarizerCall {
                            id,
                            name,
                            arguments: String::new(),
                        });
                    }
                    Ok(ProviderEvent::ToolCallArgumentsDelta { id, json }) => {
                        let Some(call) = calls.iter_mut().find(|call| call.id == id) else {
                            return Err(
                                "summarizer streamed arguments for an unknown call".to_owned()
                            );
                        };
                        if call.arguments.len().saturating_add(json.len()) > MAX_TOOL_ARGUMENT_BYTES
                        {
                            return Err("summarizer tool arguments exceeded their bound".to_owned());
                        }
                        call.arguments.push_str(&json);
                    }
                    Ok(ProviderEvent::ToolCallCompleted { .. }) => {}
                    Ok(ProviderEvent::Completed { usage: reported }) => {
                        usage = reported.map(provider_usage);
                        break;
                    }
                    Ok(ProviderEvent::Incomplete { reason, .. }) => {
                        truncated = Some(reason);
                        break;
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            // Overflowing the sum is a provider protocol fault; fail the
            // compaction rather than persist an understated total.
            total_usage = match (total_usage, usage) {
                (Some(total), Some(turn)) => match sessions::add_usage(total, turn) {
                    Some(sum) => Some(sum),
                    None => return Err("summarizer usage overflowed".to_owned()),
                },
                (Some(total), None) | (None, Some(total)) => Some(total),
                (None, None) => None,
            };
            if !calls.is_empty() {
                // Answer each call with a rejection and ask again. The model
                // abandoned its reply, so nothing written so far is kept. A
                // cut turn's calls are incomplete and cannot be answered.
                if truncated.is_some() {
                    return Err("summarizer called a tool and was cut off".to_owned());
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
                        return Err(
                            "summarizer streamed tool arguments that are not JSON".to_owned()
                        );
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
                // Reasoning providers (Anthropic thinking) require the turn's
                // replay data alongside its tool calls.
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
                return Ok((summary, total_usage));
            };
            if text.trim().is_empty() && reason == qq_provider::IncompleteReason::OutputTokens {
                // Nothing visible: the cap went to hidden reasoning. A
                // continuation would resend the same request. (A provider
                // pause with no text is resent as the provider requires.)
                return Err(format!(
                    "summarizer output was cut off at the output token limit ({max_output_tokens} tokens) without producing any visible text; the limit was spent on reasoning. Lower `reasoning_effort` or raise `max_output_tokens`"
                ));
            }
            if continuations >= MAX_OUTPUT_CONTINUATIONS {
                return Err(format!(
                    "summarizer output was cut off at the output token limit ({max_output_tokens} tokens) on {} consecutive turns",
                    u32::from(MAX_OUTPUT_CONTINUATIONS) + 1
                ));
            }
            continuations += 1;
            messages.push(Message::assistant(text));
            messages.push(Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE));
        }
    }

    pub fn with_task_router(mut self, router: Arc<dyn sessions::TaskRouter>) -> Self {
        self.task_router = Some(router);
        self
    }

    /// Installs the typed reviewer for its selected tool/final boundaries.
    #[must_use]
    pub fn with_checkpoint_reviewer(
        mut self,
        reviewer: Arc<dyn runtime::CheckpointReviewer>,
    ) -> Self {
        self.checkpoint_identity = Some(Arc::from(reviewer.identity()));
        self.checkpoint = Some(reviewer);
        self
    }

    /// Supplies the effective model context window for provider-neutral
    /// request planning. `None` retains the independent storage backstop.
    #[must_use]
    pub fn with_context_window(mut self, context_window: Option<u32>) -> Self {
        self.context_window = context_window;
        self
    }

    /// Supplies the highest output cap a turn may be raised to when the whole
    /// cap went to hidden reasoning (see [`MAX_EMPTY_OUTPUT_RETRIES`]). A
    /// value at or below `max_output_tokens` disables the raise.
    #[must_use]
    pub const fn with_output_ceiling(mut self, output_ceiling: Option<OutputCeiling>) -> Self {
        self.output_ceiling = output_ceiling;
        self
    }

    /// The resolved-model account of a runtime constructed without
    /// configuration: identity comes from the runtime itself, and every
    /// capability the embedder did not declare is recorded as unsupported or
    /// unknown rather than guessed.
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
    /// once per run, concurrently, before the first provider request. At
    /// most [`MAX_CONTEXT_SOURCES`] may be registered; plan compilation,
    /// which every run path goes through, rejects more with
    /// [`plan::PlanCompileError::TooManyContextSources`] before any
    /// provider work.
    #[must_use]
    pub fn with_context_source(mut self, source: Arc<dyn ContextSource>) -> Self {
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

    /// Sets the shell environment allowlist and built-in preference.
    #[must_use]
    pub fn with_shell_policy(mut self, shell: runtime::ShellPolicy) -> Self {
        self.shell = Arc::new(shell);
        self
    }

    /// Sets the managed host denies `fetch` refuses under every mode.
    #[must_use]
    pub fn with_network_policy(mut self, network: NetworkPolicy) -> Self {
        self.network = Arc::new(network);
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
            RunCancellation::new(),
        )
    }

    fn run_messages_in_workspace_with_cancellation(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: RunCancellation,
    ) -> RuntimeStream {
        // Configuration allowlists are the only grants a gate-less run has;
        // read-only mode still denies them inside `evaluate`.
        let grants = approval::SessionGrants {
            tools: self.config_grants(),
            shell_prefixes: Vec::new(),
            hosts: Vec::new(),
            delegate: approval::DelegateGrants::default(),
        };
        self.run_loop(
            messages,
            workspace,
            cancelled,
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::Ask,
                grants,
                network: Arc::clone(&self.network),
            }),
            Arc::new(workspace::FileState::default()),
        )
    }

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

    /// Runs the loop from a runtime that was not compiled into a plan: the
    /// workspace is canonicalized and opened, and instructions are read, for
    /// this run only. Durable sessions compile once and call
    /// [`CompiledAgentPlan::execute`] directly.
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
            // Capturing the profile calls every host's blocking catalog and
            // compilation re-opens the workspace and re-reads instructions, so
            // both run on the blocking thread; the direct path pays this once
            // per run, the same filesystem work it always did.
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
            hosts: Vec::new(),
            delegate: approval::DelegateGrants::default(),
        };
        public_run_stream(
            self.execute(
                vec![Message::user(input::render_text(command.input()))],
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants,
                    network: Arc::clone(&self.runtime.network),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None),
            ),
            self.runtime.context_window,
        )
    }
}

fn checkpoint_protocol_outcome(
    outcome: runtime::CheckpointOutcome,
) -> qq_protocol::CheckpointOutcome {
    match outcome {
        runtime::CheckpointOutcome::Supported => qq_protocol::CheckpointOutcome::Supported,
        runtime::CheckpointOutcome::PartiallySupported => {
            qq_protocol::CheckpointOutcome::PartiallySupported
        }
        runtime::CheckpointOutcome::Contradicted => qq_protocol::CheckpointOutcome::Contradicted,
        runtime::CheckpointOutcome::InsufficientEvidence => {
            qq_protocol::CheckpointOutcome::InsufficientEvidence
        }
        runtime::CheckpointOutcome::Unavailable => qq_protocol::CheckpointOutcome::Unavailable,
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
                // Direct runs have no compactor, so these never fire.
                | RuntimeEvent::InRunCompacted { .. }
                // Direct runs keep no session history to replay.
                | RuntimeEvent::ContextPruned { .. }
                | RuntimeEvent::ProviderOverflow { .. }
                | RuntimeEvent::ToolCallStarted { .. }
                | RuntimeEvent::ToolCallDenied { .. }
                | RuntimeEvent::ToolCallAnswered { .. }
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
                | RuntimeEvent::OutputTruncated { .. }
                // Turn recovery is transparent too; `Paused` below names
                // exhaustion.
                | RuntimeEvent::TurnRetrying { .. }
                // Direct runs carry no output contract.
                | RuntimeEvent::OutputRepairRequested { .. } => {}
                RuntimeEvent::CheckpointStarted { correlation, phase, tool_call_id } => {
                    yield RunEvent::CheckpointStarted { correlation, phase, tool_call_id };
                }
                RuntimeEvent::CheckpointReviewed {
                    correlation,
                    phase,
                    tool_call_id,
                    outcome,
                    confidence,
                    feedback,
                    spend,
                } => {
                    yield RunEvent::CheckpointReviewed {
                        correlation,
                        phase,
                        tool_call_id,
                        outcome,
                        confidence_basis_points: confidence.map(|value| {
                            (value.clamp(0.0, 1.0) * 10_000.0).round() as u16
                        }),
                        feedback,
                        spend,
                    };
                }
                RuntimeEvent::Completed { .. } => {
                    yield RunEvent::Completed;
                    return;
                }
                RuntimeEvent::Failed { kind, message } => {
                    yield RunEvent::Failed { kind, message };
                    return;
                }
                // The direct stream has no session to resume from, so the
                // pause surfaces as the last attempt's failure with the
                // retries it spent.
                RuntimeEvent::Paused { pause } => {
                    yield RunEvent::Failed {
                        kind: pause.kind,
                        message: format!(
                            "{} (paused after {} turn retries)",
                            pause.message, pause.attempts
                        ),
                    };
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

/// What the run loop keeps of a tool result between its finished event and
/// the turn's result message: the bounded text, the error flag, and the spill
/// handle. The full output (file states, display payload, and the spilled
/// text, which can be as large as the store keeps) moves into the event.
#[derive(Debug, Clone)]
struct RetainedResult {
    model_text: String,
    is_error: bool,
    spill_handle: Option<String>,
}

impl RetainedResult {
    fn retain(output: &tools::ToolOutput, tool: &str, call: ToolCallId) -> Self {
        Self {
            model_text: output.model_text.clone(),
            is_error: output.is_error,
            spill_handle: output.spill.as_ref().map(|spill| spill.handle(tool, call)),
        }
    }

    const fn error(message: String) -> Self {
        Self {
            model_text: message,
            is_error: true,
            spill_handle: None,
        }
    }

    /// An `ask_user` result the session layer already persisted: kept as-is
    /// so the model reads exactly what the store holds.
    const fn answered(result: String) -> Self {
        Self {
            model_text: result,
            is_error: false,
            spill_handle: None,
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

/// Whether a provider fault that ended a started stream is worth re-issuing
/// the turn for. Overload, rate limiting, and transport loss are the
/// provider's moment, not the request's; everything else (auth, invalid
/// request, malformed stream) would fail the same way again.
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
mod tests {
    mod progress;

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
            let mut runtime =
                Runtime::new(EffortProvider(Arc::clone(&captured)), "test", 128).unwrap();
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

    /// D5: the run shares its transcript with each turn's request and, once
    /// the provider stream is dropped, appends in place. The provider records
    /// only the allocation's address (a `Weak` would itself force
    /// `Arc::make_mut` to reallocate); turn two's request must then point at
    /// the very allocation turn one saw, grown by two messages.
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
        // Exactly two holders at request time: the run and the request.
        assert_eq!((*first_holders, *second_holders), (2, 2));
        // Same allocation both turns: the append copied nothing.
        assert_eq!(
            first, second,
            "the transcript must be grown in place between turns"
        );
    }

    /// D5: the run's `system_prompt_hash` is the SHA-256 of the full prompt
    /// even though the plan-constant prefix was hashed once at compile and
    /// only the per-run suffix (guidance, output contract) per run. The full
    /// text must also equal what the one-shot builder produces, across the
    /// capability sets that vary the prefix, so persisted identities are
    /// unchanged by the split.
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
            plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
                &runtime, workspace,
            ))
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

        // (capabilities, prompt) pairs: the default set with a skill and a
        // contract (both suffix), a read-only guidance-less child, a tool-less
        // compaction-style run, and the default set with no suffix at all.
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
                    Arc::new(StaticPolicyGate {
                        mode: ApprovalMode::ReadOnly,
                        grants: approval::SessionGrants::default(),
                        network: Arc::default(),
                    }),
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
        // The suffix landed where the one-shot builder would have put it.
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
        // Byte-for-byte parity with the single-pass builder for the plain case.
        let expected = runtime::agent_system_prompt(
            plan.workspace.path(),
            &plan.catalog.base_specs(&catalog::StaticFilter {
                spawn_agent: false,
                search_history: false,
                read_tool_result: false,
                load_skill: true,
                read_only: false,
            }),
            runtime::PromptSections {
                tool_index: plan.catalog.index_text().map(Arc::as_ref),
                roster: plan.roster_text.as_deref(),
                skill_index: plan.skills.disclosure_text(),
                subagent: None,
            },
            &plan.instructions,
            plan.persona.as_deref(),
            None,
        );
        assert_eq!(systems[3].as_ref(), expected.as_str());
        // Distinct capability sets produced distinct prefixes; the two
        // default-set runs shared one.
        assert_ne!(systems[1], systems[2]);
        assert_ne!(systems[1], systems[3]);
    }

    /// Scripted turns for `Runtime::summarize`: each request pops the next
    /// event list and is recorded.
    struct SummarizeScript {
        turns: Mutex<std::collections::VecDeque<Vec<Result<ProviderEvent, ProviderError>>>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for SummarizeScript {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let turn = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted turn");
            Box::pin(stream::iter(turn))
        }
    }

    fn summarize_call(id: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
        vec![
            Ok(ProviderEvent::ToolCallStarted {
                id: id.to_owned(),
                name: "read_file".to_owned(),
            }),
            Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: id.to_owned(),
                json: r#"{"path":"x"}"#.to_owned(),
            }),
            Ok(ProviderEvent::ToolCallCompleted { id: id.to_owned() }),
        ]
    }

    async fn run_summarize(
        turns: Vec<Vec<Result<ProviderEvent, ProviderError>>>,
    ) -> (Result<String, String>, Vec<ModelRequest>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            SummarizeScript {
                turns: Mutex::new(turns.into()),
                requests: Arc::clone(&requests),
            },
            "test-model",
            256,
        )
        .unwrap();
        let tools: Arc<[ToolSpec]> = Arc::from([ToolSpec::new(
            "read_file",
            "read",
            serde_json::json!({"type": "object"}),
        )]);
        let result = runtime
            .summarize(
                vec![Message::user("summarize")],
                Arc::from("system"),
                tools,
                256,
            )
            .await
            .map(|(summary, _)| summary);
        let requests = requests.lock().unwrap().clone();
        (result, requests)
    }

    #[tokio::test]
    async fn in_run_summarize_answers_one_call_turn_with_rejections_and_keeps_only_the_new_reply() {
        // Turn one is cut mid-reply; turn two continues it but calls a tool,
        // abandoning the reply; turn three writes the summary. Only turn
        // three is the summary: no fragment of the abandoned reply survives.
        let mut call_turn = vec![
            Ok(ProviderEvent::Replay {
                data: Arc::from("thinking-signature"),
            }),
            Ok(ProviderEvent::OutputTextDelta {
                text: "let me check".to_owned(),
            }),
        ];
        call_turn.extend(summarize_call("call_1"));
        call_turn.push(Ok(ProviderEvent::Completed { usage: None }));
        let (result, requests) = run_summarize(vec![
            vec![
                Ok(ProviderEvent::OutputTextDelta {
                    text: "1. Intent: half".to_owned(),
                }),
                Ok(ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                }),
            ],
            call_turn,
            vec![
                Ok(ProviderEvent::OutputTextDelta {
                    text: "the summary".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ],
        ])
        .await;
        assert_eq!(result.as_deref(), Ok("the summary"));
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request.system(), Some("system"));
            assert_eq!(request.tools().len(), 1);
        }
        // The retry carries the call with its replay data, then a rejection.
        let messages = requests[2].messages();
        let (call, result) = (&messages[messages.len() - 2], &messages[messages.len() - 1]);
        assert_eq!(call.replay(), Some("thinking-signature"));
        assert!(call.content().iter().any(|block| matches!(
            block,
            ContentBlock::ToolCall { id, name, .. } if id == "call_1" && name == "read_file"
        )));
        assert!(matches!(
            result.content(),
            [ContentBlock::ToolResult { call_id, is_error: true, content }]
                if call_id == "call_1" && content == SUMMARIZER_TOOL_REJECTION
        ));
    }

    #[tokio::test]
    async fn in_run_summarize_fails_on_a_second_call_turn_and_on_a_cut_call_turn() {
        let mut first = summarize_call("call_1");
        first.push(Ok(ProviderEvent::Completed { usage: None }));
        let mut second = summarize_call("call_2");
        second.push(Ok(ProviderEvent::Completed { usage: None }));
        let (result, requests) = run_summarize(vec![first, second]).await;
        assert_eq!(
            result,
            Err("summarizer called a tool on two turns".to_owned())
        );
        assert_eq!(requests.len(), 2);

        let mut cut = summarize_call("call_1");
        cut.push(Ok(ProviderEvent::Incomplete {
            usage: None,
            reason: qq_provider::IncompleteReason::OutputTokens,
        }));
        let (result, _) = run_summarize(vec![cut]).await;
        assert_eq!(
            result,
            Err("summarizer called a tool and was cut off".to_owned())
        );
    }

    /// ADR-0056 § 5: a summarizer run carries the system prompt and tools
    /// of the prompt runs whose prefix key it is given, fetches no context
    /// sources, and sends no output-contract notice, so its request shares
    /// the cached prefix of those runs.
    #[tokio::test]
    async fn a_summarizer_run_sends_the_prompt_runs_system_prompt_and_tools_without_context_sources()
     {
        struct Capture(Arc<Mutex<Vec<ModelRequest>>>);

        impl Provider for Capture {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.0.lock().unwrap().push(request);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "{\"ok\":true}".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        struct CountingSource(Arc<AtomicUsize>);

        impl ContextSource for CountingSource {
            fn name(&self) -> &str {
                "memory"
            }
            fn version(&self) -> &str {
                "1"
            }
            fn cache_key(&self, _request: &context_source::ContextRequest) -> Option<[u8; 32]> {
                None
            }
            fn fetch(
                &self,
                _request: context_source::ContextRequest,
                _cancelled: RunCancellation,
            ) -> context_source::ContextFetchFuture {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(context_source::ContextBundle {
                        items: vec![context_source::ContextItem {
                            provenance: "memory:0".to_owned(),
                            content: "remembered".to_owned(),
                        }],
                    })
                })
            }
            fn fail_policy(&self) -> context_source::FailPolicy {
                context_source::FailPolicy::Open
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(Capture(Arc::clone(&requests)), "test-model", 256)
            .unwrap()
            .with_context_source(Arc::new(CountingSource(Arc::clone(&fetches))));
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let plan = tokio::task::spawn_blocking(move || {
            plan::CompiledAgentPlan::compile_blocking(plan::AgentProfile::embedded(
                &runtime, workspace,
            ))
            .unwrap()
        })
        .await
        .unwrap();
        let contract = output::CompiledOutputSchema::compile(&qq_protocol::OutputContract {
            schema: serde_json::json!({"type": "object"}),
            repair_turns: 0,
        })
        .unwrap();
        let user_key = plan::PromptPrefixKey {
            tools: Some(catalog::StaticFilter {
                spawn_agent: false,
                search_history: false,
                read_tool_result: false,
                load_skill: true,
                read_only: false,
            }),
            guidance: true,
            subagent: None,
        };
        for capabilities in [
            RunCapabilities::user(None),
            RunCapabilities::restricted()
                .with_output(Some(contract))
                .summarizer(user_key),
        ] {
            let events = plan
                .execute(
                    vec![Message::user("hello")],
                    RunCancellation::new(),
                    Arc::new(StaticPolicyGate {
                        mode: ApprovalMode::ReadOnly,
                        grants: approval::SessionGrants::default(),
                        network: Arc::default(),
                    }),
                    Arc::new(workspace::FileState::default()),
                    capabilities,
                )
                .collect::<Vec<_>>()
                .await;
            assert!(
                matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
                "{events:?}"
            );
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "only the prompt run fetches context"
        );
        let requests = requests.lock().unwrap();
        let (prompt, summarizer) = (&requests[0], &requests[1]);
        let prompt_system = prompt.system().unwrap();
        let summarizer_system = summarizer.system().unwrap();
        assert!(prompt_system.contains("[memory:0]"));
        assert!(!summarizer_system.contains("[memory:0]"));
        assert!(!summarizer_system.contains("## Output contract"));
        // The plan-constant prefix is shared byte for byte; only the prompt
        // run's per-run suffix (here, the context block) follows it.
        assert!(prompt_system.starts_with(summarizer_system));
        assert_eq!(summarizer.tools(), prompt.tools());
        assert!(!summarizer.tools().is_empty());
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
                    0 => tool_turn("list-src", "tree", r#"{"path":"src","depth":1}"#),
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
                                && content.ends_with("\n1\tFollow src fallback.\n")
                        ));
                        tool_turn(
                            "list-feature",
                            "tree",
                            r#"{"path":"src/feature","depth":1}"#,
                        )
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
                                && content.ends_with("\n1\tFollow feature policy.\n")
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
                            }]) if call_id == "read-before" && content.ends_with("\n1\tbefore\n")
                        ));
                        tool_turn(
                            "edit",
                            "edit_file",
                            r#"{"edits":[{"path":"src/feature/note.txt","old":"before\n","new":"after\n"}]}"#,
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
                            }]) if call_id == "read-after" && content.ends_with("\n1\tafter\n")
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
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
        let cancelled = RunCancellation::new();
        let run_cancelled = cancelled.clone();
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
        cancelled.cancel();
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
    async fn review_regression_final_checkpoint_preserves_steering() {
        struct FinalProvider;
        impl Provider for FinalProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".into(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct SteeringReviewer {
            sender: runtime::SteeringSender,
            sent: std::sync::atomic::AtomicBool,
            message_id: qq_protocol::MessageId,
        }
        impl CheckpointReviewer for SteeringReviewer {
            fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
                let send = !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst);
                if !send {
                    assert!(
                        request.task.contains("Also explain the result"),
                        "review must use the steered task"
                    );
                }
                let sender = self.sender.clone();
                let message_id = self.message_id;
                Box::pin(async move {
                    // Deliver steering after the final checkpoint has started,
                    // before its successful result is returned to the run loop.
                    if send {
                        sender
                            .messages
                            .send(runtime::SteeringMessage::text(
                                message_id,
                                "Also explain the result",
                            ))
                            .await
                            .unwrap();
                    }
                    CheckpointVerdict {
                        spend: qq_protocol::CheckpointSpend::default(),
                        outcome: CheckpointOutcome::Supported,
                        confidence: Some(1.0),
                        feedback: "supported".into(),
                    }
                })
            }
        }
        let (sender, receiver) = runtime::steering_channel();
        let message_id = qq_protocol::MessageId::from_bytes([42; 16]);
        let runtime = Runtime::new(FinalProvider, "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(SteeringReviewer {
                sender,
                sent: std::sync::atomic::AtomicBool::new(false),
                message_id,
            }));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("answer directly")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_steering(receiver),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            events.iter().any(|event| matches!(event,
                RuntimeEvent::SteeringApplied { message_id: applied, .. } if *applied == message_id
            )),
            "steering accepted during final review must be applied before completion: {events:?}"
        );
    }

    #[tokio::test]
    async fn interrupting_steering_stops_final_checkpoint_review() {
        struct FinalProvider;
        impl Provider for FinalProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".into(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct SteeringReviewer {
            sender: runtime::SteeringSender,
            sent: std::sync::atomic::AtomicBool,
            message_id: qq_protocol::MessageId,
        }
        impl CheckpointReviewer for SteeringReviewer {
            fn review(&self, _: CheckpointRequest) -> CheckpointFuture {
                let send = !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst);
                let sender = self.sender.clone();
                let message_id = self.message_id;
                Box::pin(async move {
                    // Deliver steering after the final checkpoint has started,
                    // before its successful result is returned to the run loop.
                    if send {
                        sender
                            .messages
                            .send(runtime::SteeringMessage::text(
                                message_id,
                                "Also explain the result",
                            ))
                            .await
                            .unwrap();
                    }
                    if send {
                        sender.interrupt();
                        std::future::pending::<()>().await;
                    }
                    CheckpointVerdict {
                        spend: qq_protocol::CheckpointSpend::default(),
                        outcome: CheckpointOutcome::Supported,
                        confidence: Some(1.0),
                        feedback: "supported".into(),
                    }
                })
            }
        }
        let (sender, receiver) = runtime::steering_channel();
        let message_id = qq_protocol::MessageId::from_bytes([42; 16]);
        let runtime = Runtime::new(FinalProvider, "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(SteeringReviewer {
                sender,
                sent: std::sync::atomic::AtomicBool::new(false),
                message_id,
            }));
        let directory = tempfile::tempdir().unwrap();
        let running = runtime
            .run_loop_with_spawner(
                vec![Message::user("answer directly")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_steering(receiver),
            )
            .collect::<Vec<_>>();
        let events = tokio::time::timeout(std::time::Duration::from_millis(100), running)
            .await
            .expect("interrupting steering must stop the held final reviewer");
        assert!(
            events.iter().any(|event| matches!(event,
                RuntimeEvent::SteeringApplied { message_id: applied, .. } if *applied == message_id
            )),
            "steering accepted during final review must be applied before completion: {events:?}"
        );
    }

    #[tokio::test]
    async fn direct_run_exposes_checkpoint_review_events() {
        struct FinalProvider;
        impl Provider for FinalProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct Supports;
        impl CheckpointReviewer for Supports {
            fn review(&self, _request: CheckpointRequest) -> CheckpointFuture {
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(0.99),
                    feedback: "final evidence is supported".to_owned(),
                }))
            }
        }
        let runtime = Runtime::new(FinalProvider, "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(Supports));

        let events = runtime
            .run(RunCommand::new("answer directly"))
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::CheckpointReviewed {
                phase: qq_protocol::CheckpointPhase::FinalCandidate,
                outcome: qq_protocol::CheckpointOutcome::Supported,
                confidence_basis_points: Some(9_900),
                feedback,
                ..
            } if feedback == "final evidence is supported"
        )));
        assert!(matches!(events.last(), Some(RunEvent::Completed)));
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
                            name: "tree".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "list".to_owned(),
                            json: r#"{"path":".","depth":1}"#.to_owned(),
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

        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
        assert_eq!(
            requests[0].tools().len(),
            8 + usize::from(cfg!(feature = "tool-fetch"))
        );
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
                && content.starts_with("read note.txt L1/1 h:")
                && content.ends_with("\n1\tcontents\n")
                && second_id == "list"
                && second_content == "tree . depth=1 entries=1/1 files=1 dirs=0\nnote.txt 9\n"
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
        let cancelled = RunCancellation::new();
        let started = tools::test_executions_started();
        let execution = tokio::spawn(tools::execute(
            workspace,
            Arc::new(workspace::FileState::default()),
            "__test_delay".to_owned(),
            r#"{"delay_ms":500,"result":"late"}"#.to_owned(),
            cancelled.clone(),
            None,
            tools::ToolTasks::default(),
            Arc::new(runtime::ShellPolicy::default()),
            Arc::default(),
        ));
        while tools::test_executions_started() == started {
            tokio::task::yield_now().await;
        }
        cancelled.cancel();
        let result = execution.await.unwrap();
        assert!(result.is_error);
        assert!(result.model_text.contains("cancelled"));
    }

    #[tokio::test]
    async fn a_turns_tool_output_is_capped_and_persisted_results_stay_whole() {
        // Four reads of ~28 KiB each: per call every one fits its own 32 KiB
        // bound, together they exceed the 96 KiB turn budget. The fourth
        // result the model sees is re-bounded to the remainder; the events
        // (what the store persists) keep each call's full bounded text.
        struct ThreeReadsProvider {
            turn: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ThreeReadsProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request);
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current != 0 {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let mut events = Vec::new();
                for index in 0..4 {
                    let id = format!("read-{index}");
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "read_file".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: format!(r#"{{"path":"big-{index}.txt","limit":2000}}"#),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let line = format!("{}\n", "z".repeat(63));
        for index in 0..4 {
            std::fs::write(
                directory.path().join(format!("big-{index}.txt")),
                line.repeat(420),
            )
            .unwrap();
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            ThreeReadsProvider {
                turn: Mutex::new(0),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("read")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));

        let persisted = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished { result, .. } => Some(result.len()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(persisted.len(), 4);
        for length in &persisted {
            assert!((27 * 1024..=29 * 1024).contains(length), "{length}");
        }

        let requests = requests.lock().unwrap();
        let in_context = requests[1].messages()[2]
            .content()
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { content, .. } => content.as_str(),
                other => panic!("unexpected block {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(in_context[0].len(), persisted[0]);
        assert_eq!(in_context[1].len(), persisted[1]);
        assert_eq!(in_context[2].len(), persisted[2]);
        assert!(in_context[3].len() <= 16 * 1024, "{}", in_context[3].len());
        assert!(
            in_context[3].contains("turn budget reached"),
            "{}",
            in_context[3]
        );
        assert!(
            in_context[3].starts_with("read big-3.txt L1-420/420 h:"),
            "the header survives"
        );
        assert!(
            in_context[3].ends_with(&format!("420\t{line}")),
            "the tail survives"
        );
        let total: usize = in_context.iter().map(|content| content.len()).sum();
        assert!(
            total <= tools::output::MAX_TURN_TOOL_OUTPUT_BYTES,
            "{total}"
        );
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
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn enforced_checkpoint_rejects_multi_call_turn_before_any_tool_executes() {
        struct TwoCalls {
            turn: Mutex<u8>,
        }
        impl Provider for TwoCalls {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "a".into(),
                            name: "__test_read".into(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "a".into(),
                            json: r#"{"delay_ms":0,"result":"a"}"#.into(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted { id: "a".into() }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "b".into(),
                            name: "__test_read".into(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "b".into(),
                            json: r#"{"delay_ms":0,"result":"b"}"#.into(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted { id: "b".into() }),
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
        struct Supports;
        impl CheckpointReviewer for Supports {
            fn review(&self, _request: CheckpointRequest) -> CheckpointFuture {
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(1.0),
                    feedback: "supported".into(),
                }))
            }
        }
        let before = tools::test_executions_started();
        let runtime = Runtime::new(
            TwoCalls {
                turn: Mutex::new(0),
            },
            "test",
            256,
        )
        .unwrap()
        .with_checkpoint_reviewer(Arc::new(Supports));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("two reads")], directory.path().into())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(tools::test_executions_started(), before);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::CheckpointReviewed {
                        phase: qq_protocol::CheckpointPhase::ToolResult,
                        ..
                    }
                ))
                .count(),
            2
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AssistantTurnCompleted { calls, .. } if calls.len() == 2
        )));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn final_checkpoint_uses_continued_history_all_task_blocks_and_masks_secrets() {
        struct FinalProvider;
        impl Provider for FinalProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "API_KEY=abcdefgh12345".into(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct Reviewer(Arc<Mutex<Vec<CheckpointRequest>>>);
        impl CheckpointReviewer for Reviewer {
            fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
                self.0.lock().unwrap().push(request);
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(1.0),
                    feedback: "supported".into(),
                }))
            }
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(FinalProvider, "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(Reviewer(Arc::clone(&requests))));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![
                    Message::user("Earlier requirement: retain attribution"),
                    Message::tool_results(vec![ContentBlock::ToolResult {
                        call_id: "prior-call".into(),
                        content: "Prior source API_KEY=abcdefgh12345".into(),
                        is_error: false,
                    }]),
                    Message::new(
                        Role::User,
                        vec![
                            ContentBlock::Text {
                                text: "Summarize API_KEY=abcdefgh12345".into(),
                            },
                            ContentBlock::Text {
                                text: "Include uncertainties".into(),
                            },
                        ],
                    ),
                ],
                directory.path().into(),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].task.contains("Include uncertainties"));
        assert!(requests[0].evidence.contains("Earlier requirement"));
        assert!(requests[0].evidence.contains("prior-call"));
        assert!(!requests[0].task.contains("abcdefgh12345"));
        assert!(!requests[0].evidence.contains("abcdefgh12345"));
    }

    #[tokio::test]
    async fn final_checkpoint_spend_exhausts_the_shared_run_budget() {
        struct FinalProvider;
        impl Provider for FinalProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".into(),
                    }),
                    Ok(ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 1,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_tokens: None,
                        }),
                    }),
                ]))
            }
        }
        struct Reviewer;
        impl CheckpointReviewer for Reviewer {
            fn review(&self, _: CheckpointRequest) -> CheckpointFuture {
                Box::pin(std::future::ready(CheckpointVerdict {
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(1.0),
                    feedback: "supported".into(),
                    spend: qq_protocol::CheckpointSpend {
                        usage: Some(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 2,
                            ..TokenUsage::default()
                        }),
                        estimated_cost_usd_nanos: Some(840),
                    },
                }))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(FinalProvider, "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(Reviewer));
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("finish")],
                directory.path().into(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::ReadOnly,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_limits(
                    RunLimits {
                        max_total_tokens: Some(10),
                        ..RunLimits::default()
                    },
                    None,
                ),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(event, RuntimeEvent::CheckpointReviewed { spend: Some(spend), .. } if spend.usage.unwrap().input_tokens == 20)));
        assert!(
            matches!(events.last(), Some(RuntimeEvent::BudgetExhausted { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn checkpoint_request_and_repair_limits_stop_repeated_tool_loops() {
        struct RepeatingProvider;
        impl Provider for RepeatingProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "read".into(),
                        name: "read_file".into(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "read".into(),
                        json: r#"{"path":"note"}"#.into(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id: "read".into() }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct Reviewer(CheckpointOutcome, Arc<AtomicUsize>);
        impl CheckpointReviewer for Reviewer {
            fn review(&self, _: CheckpointRequest) -> CheckpointFuture {
                self.1.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::ready(CheckpointVerdict {
                    outcome: self.0,
                    confidence: Some(1.0),
                    feedback: "criterion: test evidence".into(),
                    spend: qq_protocol::CheckpointSpend::default(),
                }))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note"), "evidence").unwrap();
        // A GREEN reviewer is asked 32 times and then the per-run review
        // limit fails the run: that is a harness bound, not a verdict.
        {
            let calls = Arc::new(AtomicUsize::new(0));
            let runtime = Runtime::new(RepeatingProvider, "test", 256)
                .unwrap()
                .with_checkpoint_reviewer(Arc::new(Reviewer(
                    CheckpointOutcome::Supported,
                    Arc::clone(&calls),
                )));
            let events = runtime
                .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().into())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(calls.load(Ordering::SeqCst), 32);
            assert!(
                matches!(events.last(), Some(RuntimeEvent::Failed { message, .. }) if message.contains("32 review requests")),
                "{events:?}"
            );
        }
        // A RED reviewer redirects the run twice. After that its verdicts
        // are recorded on each result and the model proceeds; the run is not
        // failed for disagreeing with the reviewer (RR3). The provider here
        // never stops calling tools, so the review limit is what ends it,
        // with every one of the 32 verdicts durable in the event stream.
        {
            let calls = Arc::new(AtomicUsize::new(0));
            let runtime = Runtime::new(RepeatingProvider, "test", 256)
                .unwrap()
                .with_checkpoint_reviewer(Arc::new(Reviewer(
                    CheckpointOutcome::Contradicted,
                    Arc::clone(&calls),
                )));
            let events = runtime
                .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().into())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(calls.load(Ordering::SeqCst), 32);
            let red = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        RuntimeEvent::CheckpointReviewed {
                            outcome: qq_protocol::CheckpointOutcome::Contradicted,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(red, 32);
            assert!(
                !events.iter().any(|event| matches!(event, RuntimeEvent::Failed { message, .. } if message.contains("correction attempts"))),
                "{events:?}"
            );
            assert!(
                matches!(events.last(), Some(RuntimeEvent::Failed { message, .. }) if message.contains("32 review requests")),
                "{events:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_final_candidate_rejected_twice_completes_with_the_verdicts_on_record() {
        // Regression (RR3): 9 real runs on 2026-09-19/20 ended as `Failed`
        // with "Jev exhausted its two correction attempts" after the model
        // had produced an answer. The reviewer's disagreement is now evidence
        // in the transcript: two RED verdicts redirect, the third completes.
        struct AnswerProvider(AtomicUsize);
        impl Provider for AnswerProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let attempt = self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: format!("answer {attempt}"),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct RedReviewer(Arc<AtomicUsize>);
        impl CheckpointReviewer for RedReviewer {
            fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
                assert_eq!(request.phase, runtime::CheckpointPhase::FinalCandidate);
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::ready(CheckpointVerdict {
                    outcome: CheckpointOutcome::InsufficientEvidence,
                    confidence: Some(0.9),
                    feedback: "C1: no direct evidence".into(),
                    spend: qq_protocol::CheckpointSpend::default(),
                }))
            }
        }
        let reviews = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(AnswerProvider(AtomicUsize::new(0)), "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(RedReviewer(Arc::clone(&reviews))));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("answer")], directory.path().into())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(reviews.load(Ordering::SeqCst), 3);
        let verdicts = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::CheckpointReviewed {
                        outcome: qq_protocol::CheckpointOutcome::InsufficientEvidence,
                        phase: qq_protocol::CheckpointPhase::FinalCandidate,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(verdicts, 3);
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{:?}",
            events.last()
        );
        // The third candidate is the one that stands.
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::OutputTextDelta { text } if text == "answer 2"
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn final_checkpoint_timeout_records_unknown_spend_and_completes() {
        struct Reviewer;
        impl CheckpointReviewer for Reviewer {
            fn review(&self, _: CheckpointRequest) -> CheckpointFuture {
                Box::pin(std::future::pending())
            }
        }
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: false,
            },
            "test",
            256,
        )
        .unwrap()
        .with_checkpoint_reviewer(Arc::new(Reviewer));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("answer")], directory.path().into())
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(event, RuntimeEvent::CheckpointReviewed { outcome: qq_protocol::CheckpointOutcome::Unavailable, spend: Some(spend), feedback, .. } if spend.usage.is_none() && feedback.contains("five seconds"))));
        // A reviewer outage is recorded, not turned into a failed run (RR3).
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{:?}",
            events.last()
        );
    }

    #[tokio::test]
    async fn checkpoint_cost_admission_refuses_unknown_or_unaffordable_charge_before_dispatch() {
        struct Reviewer(Option<u64>);
        impl CheckpointReviewer for Reviewer {
            fn max_cost_usd_nanos(&self) -> Option<u64> {
                self.0
            }
            fn review(&self, _: CheckpointRequest) -> CheckpointFuture {
                panic!("over-budget reviewer must not dispatch");
            }
        }
        let directory = tempfile::tempdir().unwrap();
        for (maximum, expected) in [
            (None, qq_protocol::BudgetLimitKind::CostUnknown),
            (Some(2), qq_protocol::BudgetLimitKind::Cost),
        ] {
            let runtime = Runtime::new(
                ScriptedProvider {
                    request: Arc::new(Mutex::new(None)),
                    fails: false,
                },
                "test",
                256,
            )
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(Reviewer(maximum)));
            let events = runtime
                .run_loop_with_spawner(
                    vec![Message::user("finish")],
                    directory.path().into(),
                    RunCancellation::new(),
                    Arc::new(StaticPolicyGate {
                        mode: ApprovalMode::ReadOnly,
                        grants: approval::SessionGrants::default(),
                        network: Arc::default(),
                    }),
                    Arc::new(workspace::FileState::default()),
                    RunCapabilities::user(None).with_limits(
                        RunLimits {
                            max_cost_usd_nanos: Some(1),
                            ..RunLimits::default()
                        },
                        Some(qq_protocol::ModelPricing {
                            input_usd_nanos_per_token: 0,
                            output_usd_nanos_per_token: 0,
                            cache_read_usd_nanos_per_token: Some(0),
                            cache_write_usd_nanos_per_token: Some(0),
                            context_tier: None,
                            provenance: "fixture".into(),
                        }),
                    ),
                )
                .collect::<Vec<_>>()
                .await;
            assert!(
                matches!(events.last(), Some(RuntimeEvent::BudgetExhausted { exhaustion }) if exhaustion.limit == expected),
                "{events:?}"
            );
        }
    }

    #[tokio::test]
    async fn final_checkpoint_can_correct_a_claim_without_an_extra_tool_call() {
        struct ProviderWithCorrection(AtomicUsize);
        impl Provider for ProviderWithCorrection {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let text = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    "deployed"
                } else {
                    "built locally; deployment unverified"
                };
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta { text: text.into() }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct Reviewer(AtomicUsize);
        impl CheckpointReviewer for Reviewer {
            fn reviews_tools(&self) -> bool {
                false
            }
            fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
                let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
                if !first {
                    assert!(request.evidence.contains("deployment unverified"));
                }
                Box::pin(std::future::ready(CheckpointVerdict {
                    outcome: if first {
                        CheckpointOutcome::Contradicted
                    } else {
                        CheckpointOutcome::Supported
                    },
                    confidence: Some(1.0),
                    feedback: "consistency: do not infer deployment from a local build".into(),
                    spend: qq_protocol::CheckpointSpend::default(),
                }))
            }
        }
        let runtime = Runtime::new(ProviderWithCorrection(AtomicUsize::new(0)), "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(Reviewer(AtomicUsize::new(0))));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user(
                    "Summarize: a local build passed; deployment was not performed",
                )],
                directory.path().into(),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::CheckpointReviewed { .. }))
                .count(),
            2
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        );
    }

    #[tokio::test]
    async fn selective_checkpoint_preserves_batching_and_finishes_large_evidence_run() {
        struct BatchProvider(AtomicUsize);
        impl Provider for BatchProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let mut events = Vec::new();
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    for i in 0..8 {
                        let id = format!("read-{i}");
                        events.extend([
                            Ok(ProviderEvent::ToolCallStarted {
                                id: id.clone(),
                                name: "read_file".into(),
                            }),
                            Ok(ProviderEvent::ToolCallArgumentsDelta {
                                id: id.clone(),
                                json: format!(r#"{{"path":"file-{i}"}}"#),
                            }),
                            Ok(ProviderEvent::ToolCallCompleted { id }),
                        ]);
                    }
                } else {
                    events.push(Ok(ProviderEvent::OutputTextDelta {
                        text: "Read all eight files.".into(),
                    }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }
        struct FinalReviewer(Arc<Mutex<Vec<CheckpointRequest>>>);
        impl CheckpointReviewer for FinalReviewer {
            fn reviews_tools(&self) -> bool {
                false
            }
            fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
                self.0.lock().unwrap().push(request);
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(1.0),
                    feedback: "supported".into(),
                }))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        for i in 0..8 {
            std::fs::write(
                directory.path().join(format!("file-{i}")),
                "evidence line\n".repeat(400),
            )
            .unwrap();
        }
        let reviews = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(BatchProvider(AtomicUsize::new(0)), "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(FinalReviewer(Arc::clone(&reviews))));
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("Read all eight files")],
                directory.path().into(),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::ToolCallFinished {
                        is_error: false,
                        ..
                    }
                ))
                .count(),
            8
        );
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let requests = reviews.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].phase, CheckpointPhase::FinalCandidate);
        assert!(requests[0].evidence.len() <= runtime::MAX_CHECKPOINT_TEXT_BYTES);
        assert!(requests[0].evidence.contains("file-7"));
        assert!(requests[0].evidence.contains("omitted"));
    }

    #[tokio::test]
    async fn enforced_checkpoint_does_not_assess_an_oversized_original_task() {
        struct FinalProvider;
        impl Provider for FinalProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".into(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct CountingReviewer(Arc<AtomicUsize>);
        impl CheckpointReviewer for CountingReviewer {
            fn review(&self, _request: CheckpointRequest) -> CheckpointFuture {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(1.0),
                    feedback: "must not be used".into(),
                }))
            }
        }

        let reviews = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(FinalProvider, "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(CountingReviewer(Arc::clone(&reviews))));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user(
                    "x".repeat(runtime::MAX_CHECKPOINT_TEXT_BYTES + 1),
                )],
                directory.path().into(),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(reviews.load(Ordering::SeqCst), 0);
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CheckpointReviewed {
                phase: qq_protocol::CheckpointPhase::FinalCandidate,
                outcome: qq_protocol::CheckpointOutcome::Unavailable,
                feedback,
                ..
            } if feedback.contains("original task exceeded")
        )));
        // Unreviewable is recorded, not fatal (RR3): the answer stands.
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{:?}",
            events.last()
        );
    }

    #[tokio::test]
    async fn enforced_checkpoint_does_not_assess_oversized_tool_evidence() {
        struct ToolProvider(AtomicUsize);
        impl Provider for ToolProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".into(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let arguments = serde_json::json!({
                    "delay_ms": 0,
                    "result": "x".repeat(runtime::MAX_CHECKPOINT_TEXT_BYTES)
                })
                .to_string();
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "large".into(),
                        name: "__test_read".into(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "large".into(),
                        json: arguments,
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id: "large".into() }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct CountingReviewer(Arc<AtomicUsize>);
        impl CheckpointReviewer for CountingReviewer {
            fn review(&self, _request: CheckpointRequest) -> CheckpointFuture {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome: CheckpointOutcome::Supported,
                    confidence: Some(1.0),
                    feedback: "must not be used".into(),
                }))
            }
        }

        let reviews = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::new(ToolProvider(AtomicUsize::new(0)), "test", 256)
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(CountingReviewer(Arc::clone(&reviews))));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().into())
            .collect::<Vec<_>>()
            .await;

        // The final candidate is reviewed as usual; only the oversized tool
        // result was skipped.
        assert_eq!(reviews.load(Ordering::SeqCst), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CheckpointReviewed {
                phase: qq_protocol::CheckpointPhase::ToolResult,
                outcome: qq_protocol::CheckpointOutcome::Unavailable,
                feedback,
                ..
            } if feedback.contains("tool arguments and result exceeded")
        )));
        // Unreviewed is recorded, not fatal (RR3): the run completes.
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{:?}",
            events.last()
        );
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
                RunCancellation::new(),
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
                        if result.contains("streamed-hello") && result.starts_with("shell exit=0 ")
                )
            })
            .expect("the bounded result must follow the streamed output");
        assert!(started < delta && delta < finished);
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn invalid_tool_argument_json_yields_a_tool_error_and_continues_the_run() {
        struct RecordingReviewer {
            requests: Arc<Mutex<Vec<CheckpointRequest>>>,
            outcome: CheckpointOutcome,
        }
        impl CheckpointReviewer for RecordingReviewer {
            fn identity(&self) -> &'static str {
                "test/checkpoint/enforce"
            }
            fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
                self.requests.lock().unwrap().push(request);
                let outcome = self.outcome;
                Box::pin(std::future::ready(CheckpointVerdict {
                    spend: qq_protocol::CheckpointSpend::default(),
                    outcome,
                    confidence: Some(1.0),
                    feedback: outcome.label().to_owned(),
                }))
            }
        }
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
        let reviews = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MalformedArgumentsProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_checkpoint_reviewer(Arc::new(RecordingReviewer {
            requests: Arc::clone(&reviews),
            outcome: CheckpointOutcome::Supported,
        }));

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
            }] if call_id == "bad" && content.contains("not valid JSON") && content.contains("JEV GREEN supported")
        ));
        drop(requests);
        let recorded = reviews.lock().unwrap();
        assert_eq!(
            recorded.len(),
            2,
            "malformed result and final candidate are reviewed"
        );
        assert_eq!(recorded[0].phase, CheckpointPhase::ToolResult);
        assert!(recorded[0].evidence.contains("not valid JSON"));
        assert_eq!(recorded[1].phase, CheckpointPhase::FinalCandidate);
        assert!(recorded[1].evidence.contains("not valid JSON"));
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
                RunCancellation::new(),
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
        /// Truncated turns stream nothing visible (all hidden reasoning).
        empty: bool,
        /// Truncated turns from this ordinal onward stream nothing visible.
        empty_from: usize,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    /// Streams one complete tool call, then stops at the output limit with no
    /// text; every later turn answers `done`.
    struct CallThenCutProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
        /// After the call-then-cut turn, every later turn is cut with nothing
        /// visible (all reasoning) instead of answering.
        then_empty: bool,
    }

    impl Provider for CallThenCutProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call".to_owned(),
                        json: r#"{"path":"AGENTS.md"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call".to_owned(),
                    }),
                    Ok(ProviderEvent::Incomplete {
                        usage: None,
                        reason: qq_provider::IncompleteReason::OutputTokens,
                    }),
                ]))
            } else if self.then_empty {
                Box::pin(stream::iter([Ok(ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::OutputTokens,
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
                let mut events = if self.empty || turn >= self.empty_from {
                    Vec::new()
                } else {
                    vec![Ok(ProviderEvent::OutputTextDelta {
                        text: format!("part{turn} "),
                    })]
                };
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
                empty: false,
                empty_from: usize::MAX,
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
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );
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
                empty: false,
                empty_from: usize::MAX,
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
    async fn an_empty_truncated_turn_raises_the_cap_once_then_completes() {
        // RR8.1 regression: a turn cut at the limit with nothing visible was
        // resent byte-for-byte up to the continuation cap (four identical
        // 16 384-token reasoning turns in the audited runs). The retry must
        // change the request: the cap doubles toward the model ceiling.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: 1,
                cut_tool_call: false,
                empty: true,
                empty_from: usize::MAX,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            1024,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("think hard")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::ReadOnly,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_max_output_tokens(256),
            )
            .collect::<Vec<_>>()
            .await;

        let caps = requests
            .lock()
            .unwrap()
            .iter()
            .map(ModelRequest::max_output_tokens)
            .collect::<Vec<_>>();
        assert_eq!(caps, vec![256, 512], "the retry carries a larger cap");
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );
        // The retry is announced as the first (1-based) continuation.
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::OutputTruncated {
                continuation: 1,
                ..
            }
        )));
        // The empty partial turn is durable and flagged; no continuation
        // notice was appended because there was nothing to continue.
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AssistantTurnCompleted {
                truncated: true,
                ..
            }
        )));
        let requests = requests.lock().unwrap();
        assert_eq!(requests[1].messages().len(), 1);
    }

    #[tokio::test]
    async fn an_empty_truncation_after_the_continuation_cap_does_not_spend_another_turn() {
        // Three visible truncations exhaust the shared continuation cap; an
        // empty one after them must settle, not emit `continuation: 4` (past
        // the advertised `max_output_continuations`) and spend a fifth turn.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                empty: false,
                empty_from: usize::from(MAX_OUTPUT_CONTINUATIONS),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            1024,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("write a long answer")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::ReadOnly,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_max_output_tokens(256),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            requests.lock().unwrap().len(),
            usize::from(MAX_OUTPUT_CONTINUATIONS) + 1
        );
        assert!(events.iter().all(|event| !matches!(
            event,
            RuntimeEvent::OutputTruncated { continuation, .. }
                if *continuation > MAX_OUTPUT_CONTINUATIONS
        )));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Failed {
                kind: RunFailureKind::ProviderOutputTruncated,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_tool_call_cut_mid_arguments_with_no_text_is_an_empty_truncation() {
        // A call cut before it completed is dropped from the resend, so a
        // turn holding only that is as empty as one with nothing at all: at
        // the ceiling it fails at once rather than resending three times.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: true,
                empty: true,
                empty_from: usize::MAX,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("think hard")],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Failed {
                kind: RunFailureKind::ProviderOutputTruncated,
                message,
            }) if message.contains("without producing any visible output")
        ));
    }

    #[tokio::test]
    async fn an_empty_truncated_turn_at_the_ceiling_fails_at_once_naming_the_cause() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                empty: true,
                empty_from: usize::MAX,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("think hard")],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;

        // Already at the model ceiling: no retry can change the request, so
        // exactly one provider call is spent, not MAX_OUTPUT_CONTINUATIONS + 1.
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Failed {
                kind: RunFailureKind::ProviderOutputTruncated,
                message,
            }) if message.contains("without producing any visible output")
                && message.contains("reasoning_effort")
                && message.contains("model ceiling 256")
        ));
    }
    #[tokio::test]
    async fn a_truncated_turn_that_streamed_a_tool_call_is_continued_not_failed() {
        // The model streamed a whole tool call (visible work) and then hit the
        // cap with no text. The call cannot execute, but the turn was not
        // spent on hidden reasoning: it is continued like any truncation
        // rather than failed as an empty one.
        let requests = Arc::new(Mutex::new(Vec::new()));
        // Already at the model ceiling: an empty truncation would fail here.
        let runtime = Runtime::new(
            CallThenCutProvider {
                requests: Arc::clone(&requests),
                then_empty: false,
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

        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::OutputTruncated {
                continuation: 1,
                ..
            }
        )));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_session_shaped_run_raises_an_empty_truncated_cap_toward_the_catalog_ceiling() {
        // ENG-973: session runs start at the configured cap, which was also
        // the raise limit, so the RR8.1 raise never fired and the error named
        // the configured cap as the "model ceiling". With a catalog ceiling
        // above it the first empty truncation doubles the cap.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: 1,
                cut_tool_call: false,
                empty: true,
                empty_from: usize::MAX,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_output_ceiling(Some(OutputCeiling {
            tokens: 4096,
            policy_bound: false,
        }));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("think hard")],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;
        let caps = requests
            .lock()
            .unwrap()
            .iter()
            .map(ModelRequest::max_output_tokens)
            .collect::<Vec<_>>();
        assert_eq!(caps, vec![256, 512]);
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );

        // When the raise is spent the error names the catalog ceiling.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                empty: true,
                empty_from: usize::MAX,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_output_ceiling(Some(OutputCeiling {
            tokens: 4096,
            policy_bound: false,
        }));
        let events = runtime
            .run_messages_in_workspace(
                vec![Message::user("think hard")],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(requests.lock().unwrap().len(), 2, "one raise, then settle");
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Failed {
                kind: RunFailureKind::ProviderOutputTruncated,
                message,
            }) if message.contains("(512 tokens)") && message.contains("model ceiling 4096")
        ));
    }

    #[tokio::test]
    async fn a_tool_call_then_cut_turn_changes_the_resend_when_it_can() {
        // ENG-973: a turn that streamed a complete call and then hit the cap
        // drops the call, so a plain continuation resends the identical
        // request. With room under the ceiling the resend carries a raised
        // cap instead; the continuation counter still bounds the run.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            CallThenCutProvider {
                requests: Arc::clone(&requests),
                then_empty: false,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_output_ceiling(Some(OutputCeiling {
            tokens: 4096,
            policy_bound: false,
        }));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].max_output_tokens(), 256);
        assert_eq!(requests[1].max_output_tokens(), 512, "the resend differs");
        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );
    }

    #[tokio::test]
    async fn the_reasoning_diagnostic_counts_only_turns_that_produced_nothing() {
        // Review (#216): a call-then-cut turn takes the one raise, then the
        // retried turn is a genuine all-reasoning cut at the ceiling. The
        // failure must count one empty turn, not attribute the earlier turn
        // (which streamed a complete call) to reasoning too.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            CallThenCutProvider {
                requests: Arc::clone(&requests),
                then_empty: true,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_output_ceiling(Some(OutputCeiling {
            tokens: 512,
            policy_bound: false,
        }));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(requests.lock().unwrap().len(), 2);
        let Some(RuntimeEvent::Failed { kind, message }) = events.last() else {
            panic!("expected a failure, got {:?}", events.last());
        };
        assert_eq!(*kind, RunFailureKind::ProviderOutputTruncated);
        assert!(message.contains("on 1 consecutive turn;"), "{message}");
        assert!(message.contains("(512 tokens)"), "{message}");
    }

    /// Turn 0: nothing visible, cut. Turn 1: a complete `read_file` call that
    /// finishes normally. Turn 2 onward: nothing visible, cut.
    struct EmptyThenToolThenEmptyProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for EmptyThenToolThenEmptyProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            let cut = || {
                Ok(ProviderEvent::Incomplete {
                    usage: None,
                    reason: qq_provider::IncompleteReason::OutputTokens,
                })
            };
            if turn == 1 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "read".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "read".to_owned(),
                        json: r#"{"path":"notes.txt"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "read".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([cut()]))
            }
        }
    }

    #[tokio::test]
    async fn a_completed_tool_turn_resets_the_reasoning_only_streak() {
        // Review (#216): empty (takes the raise) -> a completed tool turn ->
        // empty at the ceiling reported two consecutive reasoning-only turns;
        // the tool turn in between was visible work.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            EmptyThenToolThenEmptyProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_output_ceiling(Some(OutputCeiling {
            tokens: 512,
            policy_bound: false,
        }));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("notes.txt"), "hello").unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(requests.lock().unwrap().len(), 3);
        let Some(RuntimeEvent::Failed { kind, message }) = events.last() else {
            panic!("expected a failure, got {:?}", events.last());
        };
        assert_eq!(*kind, RunFailureKind::ProviderOutputTruncated);
        assert!(message.contains("on 1 consecutive turn;"), "{message}");
    }

    #[tokio::test]
    async fn a_policy_bound_ceiling_names_the_policy_not_max_output_tokens() {
        // Review (#216): when managed policy, not the model, set the raise
        // ceiling, "raise `max_output_tokens`" cannot help and the number is
        // not the model's.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                empty: true,
                empty_from: usize::MAX,
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_output_ceiling(Some(OutputCeiling {
            tokens: 512,
            policy_bound: true,
        }));
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        let Some(RuntimeEvent::Failed { message, .. }) = events.last() else {
            panic!("expected a failure, got {:?}", events.last());
        };
        assert!(
            message.contains("Managed policy caps output at 512"),
            "{message}"
        );
        assert!(message.contains("policy.max_output_tokens"), "{message}");
        assert!(!message.contains("model ceiling"), "{message}");
    }

    #[tokio::test]
    async fn completed_limited_stream_stays_finished_when_polled_after_its_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: false,
            },
            "test-model",
            256,
        )
        .unwrap();
        let mut events = runtime.run_loop_with_spawner(
            vec![Message::user("finish")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::ReadOnly,
                grants: approval::SessionGrants::default(),
                network: Arc::default(),
            }),
            Arc::new(workspace::FileState::default()),
            RunCapabilities::user(None).with_limits(
                RunLimits {
                    max_duration_ms: Some(500),
                    ..RunLimits::default()
                },
                None,
            ),
        );
        loop {
            match events.next().await.expect("completion") {
                RuntimeEvent::Completed { .. } => break,
                RuntimeEvent::Failed { message, .. } => panic!("{message}"),
                RuntimeEvent::BudgetExhausted { .. } => panic!("expired before completion"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            events.next().await.is_none(),
            "a finished stream must not produce a second terminal"
        );
    }

    /// Bedrock Converse cannot ask for no tool calls, so a model may call a
    /// tool on the budget-final turn anyway. The call never runs and the run
    /// settles as exhausted without a final response, never as a provider
    /// failure.
    #[tokio::test]
    async fn a_tool_call_on_the_budget_final_turn_settles_without_running() {
        struct AlwaysCalls {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for AlwaysCalls {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let id = format!("call-{}", requests.len());
                requests.push(request);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "__test_mutate".to_owned(),
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

        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            AlwaysCalls {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("work")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_limits(
                    RunLimits {
                        max_model_turns: Some(2),
                        ..RunLimits::default()
                    },
                    None,
                ),
            )
            .collect::<Vec<_>>()
            .await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].tool_choice(), qq_provider::ToolChoice::None);
        assert_eq!(requests[1].tools(), requests[0].tools());
        let started = events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .count();
        assert_eq!(started, 1, "only the first turn's call runs: {events:?}");
        let turns = events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::AssistantTurnCompleted { .. }))
            .count();
        assert_eq!(turns, 1, "the final turn's call is never committed");
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::BudgetExhausted { exhaustion })
                if exhaustion.limit == BudgetLimitKind::ModelTurns && !exhaustion.final_response
        ));
    }

    #[tokio::test]
    async fn a_truncated_budget_final_turn_settles_as_exhausted_not_continued() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TruncatingProvider {
                truncations: usize::MAX,
                cut_tool_call: false,
                empty: false,
                empty_from: usize::MAX,
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
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
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

        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );
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
    async fn prior_empty_assistant_turns_do_not_block_a_follow_up() {
        struct CompletingProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for CompletingProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                assert!(
                    request.messages().iter().all(Message::has_content),
                    "empty reconstructed turns must not reach the provider: {:?}",
                    request.messages()
                );
                self.requests.lock().unwrap().push(request);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "ok".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            CompletingProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_messages_in_workspace(
                vec![
                    Message::user("hello"),
                    Message::assistant(""),
                    Message::user("continue"),
                ],
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            events.last(),
            Some(&RuntimeEvent::Completed { final_output: None })
        );
        assert!(events.iter().all(|event| !matches!(
            event,
            RuntimeEvent::Failed { message, .. }
                if message.contains("must not be empty")
        )));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].messages(),
            [
                Message::user("hello"),
                Message::assistant(EMPTY_TURN_PLACEHOLDER),
                Message::user("continue"),
            ]
        );
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
                RunCancellation::new(),
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

    /// Whether a recorded request is the slice-checkpoint turn: tools stay
    /// declared there and the system prompt is unchanged, so the report
    /// notice as the last message is the marker.
    fn is_checkpoint_request(request: &ModelRequest) -> bool {
        last_user_text(request) == Some(SLICE_CHECKPOINT_NOTICE)
    }

    /// The text of the request's last message when it is a user text, such
    /// as a runtime notice.
    fn last_user_text(request: &ModelRequest) -> Option<&str> {
        let message = request.messages().last()?;
        if message.role() != Role::User {
            return None;
        }
        match message.content() {
            [ContentBlock::Text { text }] => Some(text.as_str()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn the_measured_token_chain_survives_the_slice_checkpoint_and_continuation() {
        // Every turn reports usage. The checkpoint and continuation turns add
        // a notice message each; the system prompt and the static prefix stay
        // the run's own (ADR-0054 § 2). Each request must still carry a
        // measurement-derived estimate, adjusted by the byte deltas.
        struct MeasuredCheckpoint {
            emitted: Mutex<usize>,
        }

        impl Provider for MeasuredCheckpoint {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let usage = Some(qq_provider::ProviderUsage {
                    input_tokens: 1_000,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 1,
                    reasoning_tokens: None,
                });
                if is_checkpoint_request(&request) {
                    assert!(
                        !request.tools().is_empty(),
                        "tools stay declared on the checkpoint turn"
                    );
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "slice checkpoint".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage }),
                    ]));
                }
                let mut emitted = self.emitted.lock().unwrap();
                // Two turns past the checkpoint: continuation, then one more.
                if *emitted >= MAX_TOOL_CALLS_PER_SLICE + 2 {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "task complete".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage }),
                    ]));
                }
                let first = *emitted;
                let count = if first < MAX_TOOL_CALLS_PER_SLICE {
                    (MAX_TOOL_CALLS_PER_SLICE - first).min(MAX_TOOL_CALLS_PER_TURN - 1)
                } else {
                    1
                };
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
                events.push(Ok(ProviderEvent::Completed { usage }));
                Box::pin(stream::iter(events))
            }
        }

        let runtime = Runtime::new(
            MeasuredCheckpoint {
                emitted: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let events = runtime
            .run_loop(
                vec![Message::user("finish a long task")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let prepared: Vec<(u32, Option<u64>)> = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::Prepared {
                    turn_ordinal,
                    weight,
                    ..
                } => Some((*turn_ordinal, weight.compatible_input_tokens)),
                _ => None,
            })
            .collect();
        // One static prefix for the whole run: the seam no longer changes it,
        // so occupancy reuse and the provider cache hold across it.
        let prefixes = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::Prepared { static_prefix, .. } => Some(*static_prefix),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(prefixes.len() > 3);
        assert!(
            prefixes.iter().all(|prefix| *prefix == prefixes[0]),
            "{prefixes:?}"
        );
        // The first request has nothing to inherit; every later one does,
        // including the checkpoint, the continuation, and the turn after.
        assert_eq!(prepared[0].1, None);
        let unmeasured: Vec<u32> = prepared[1..]
            .iter()
            .filter(|(_, tokens)| tokens.is_none())
            .map(|(turn, _)| *turn)
            .collect();
        assert!(
            unmeasured.is_empty(),
            "turns without a measured chain: {unmeasured:?}"
        );
        // Every measured turn reports 1,000 tokens, so each request after the
        // first estimates 1,000 plus the byte delta of one turn's calls and
        // results (~300 tokens here), never the raw byte count of the whole
        // transcript. The checkpoint and continuation requests are charged
        // only their notice message on top of the measurement.
        for (turn, tokens) in &prepared[1..] {
            let tokens = tokens.expect("measured");
            assert!((900..=1_400).contains(&tokens), "turn {turn}: {tokens}");
        }
        let raw_bytes = sessions::context::estimate_tokens(MAX_TOOL_CALLS_PER_SLICE as u64 * 64);
        assert!(
            prepared[1..]
                .iter()
                .all(|(_, tokens)| tokens.is_some_and(|tokens| tokens < raw_bytes)),
            "{prepared:?}"
        );
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
                if is_checkpoint_request(&request) {
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
        let checkpoint_index = requests.iter().position(is_checkpoint_request).unwrap();
        assert!(!requests[checkpoint_index].tools().is_empty());
        assert!(!requests[checkpoint_index + 1].tools().is_empty());
        assert!(!is_checkpoint_request(&requests[checkpoint_index + 1]));
    }

    #[tokio::test]
    async fn a_tool_call_on_the_checkpoint_turn_settles_as_a_rejection_and_the_run_continues() {
        // Regression: models over OpenAI-compatible routes keep emitting
        // tool calls on the checkpoint turn when the transcript is dense with
        // them. That used to be a `ProviderProtocol` failure that discarded
        // the whole run (5 runs, 120 minutes in the 2026-09-21 audit). The
        // checkpoint keeps tools declared; a call there is admitted with a
        // not-executed result and the run continues into the next slice.
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        struct CallsOnCheckpoint {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
            emitted: Mutex<usize>,
            checkpoint_seen: AtomicBool,
        }

        impl Provider for CallsOnCheckpoint {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                let read = |id: String| {
                    [
                        Ok(ProviderEvent::ToolCallStarted {
                            id: id.clone(),
                            name: "read_file".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: id.clone(),
                            json: r#"{"path":"note.txt"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted { id }),
                    ]
                };
                if is_checkpoint_request(&request) {
                    self.checkpoint_seen.store(true, Ordering::SeqCst);
                    let mut events = Vec::from(read("checkpoint-call".to_owned()));
                    events.push(Ok(ProviderEvent::Completed { usage: None }));
                    return Box::pin(stream::iter(events));
                }
                if self.checkpoint_seen.load(Ordering::SeqCst) {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "task complete".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                // Fifteen per turn parks the first slice at 255 so the next
                // turn is the checkpoint.
                let mut emitted = self.emitted.lock().unwrap();
                let first = *emitted;
                let count = (MAX_TOOL_CALLS_PER_SLICE - 1 - first).min(MAX_TOOL_CALLS_PER_TURN - 1);
                *emitted += count;
                drop(emitted);
                let mut events = Vec::with_capacity(count * 3 + 1);
                for index in first..first + count {
                    if index == first {
                        // One write per turn is progress, so the run reaches
                        // the slice checkpoint rather than a stall report.
                        let id = format!("call-{index}");
                        events.extend([
                            Ok(ProviderEvent::ToolCallStarted {
                                id: id.clone(),
                                name: "write_file".to_owned(),
                            }),
                            Ok(ProviderEvent::ToolCallArgumentsDelta {
                                id: id.clone(),
                                json: r#"{"path":"progress.txt","content":"x"}"#.to_owned(),
                            }),
                            Ok(ProviderEvent::ToolCallCompleted { id }),
                        ]);
                    } else {
                        events.extend(read(format!("call-{index}")));
                    }
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            CallsOnCheckpoint {
                requests: Arc::clone(&requests),
                emitted: Mutex::new(0),
                checkpoint_seen: AtomicBool::new(false),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop(
                vec![Message::user("finish a long task")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{:?}",
            events.last()
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Failed { .. })),
        );
        let executed = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::ToolCallFinished {
                        is_error: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(executed, MAX_TOOL_CALLS_PER_SLICE - 1);
        let rejected: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished {
                    is_error: true,
                    result,
                    ..
                } => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(rejected.len(), 1, "{rejected:?}");
        assert_eq!(rejected[0], SLICE_CHECKPOINT_REJECTION);

        let requests = requests.lock().unwrap();
        let checkpoint_index = requests.iter().position(is_checkpoint_request).unwrap();
        assert!(!requests[checkpoint_index].tools().is_empty());
        let continuation = &requests[checkpoint_index + 1];
        assert!(!continuation.tools().is_empty());
        // Neither seam touches the system prompt: the notices are messages,
        // so the cached prefix holds across the checkpoint (ADR-0054 § 2).
        assert_eq!(requests[checkpoint_index].system(), requests[0].system());
        assert_eq!(continuation.system(), requests[0].system());
        assert_eq!(
            last_user_text(continuation),
            Some(SLICE_CONTINUATION_NOTICE)
        );
        // The rejected call has exactly one result, just before the
        // continuation notice, so the model can re-issue it.
        let messages = continuation.messages();
        assert!(matches!(
            messages[messages.len() - 2].content(),
            [ContentBlock::ToolResult { call_id, content, is_error: true }]
                if call_id == "checkpoint-call" && content == SLICE_CHECKPOINT_REJECTION
        ));
        assert_eq!(requests.len(), checkpoint_index + 2);
    }

    #[tokio::test]
    async fn calls_past_the_per_turn_cap_settle_as_tool_errors_and_the_run_continues() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        struct TwentyReadsThenAnswer {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for TwentyReadsThenAnswer {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 1 {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let mut events = Vec::new();
                for index in 0..MAX_TOOL_CALLS_PER_TURN + 4 {
                    let id = format!("call-{index}");
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

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            TwentyReadsThenAnswer {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop(
                vec![Message::user("read it twenty times")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let results: Vec<(&str, bool)> = requests[1]
            .messages()
            .last()
            .unwrap()
            .content()
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => (content.as_str(), *is_error),
                _ => panic!("tool-result message"),
            })
            .collect();
        assert_eq!(results.len(), MAX_TOOL_CALLS_PER_TURN + 4);
        for (content, is_error) in &results[..MAX_TOOL_CALLS_PER_TURN] {
            assert!(!is_error, "{content}");
            assert!(content.contains("hello"));
        }
        for (content, is_error) in &results[MAX_TOOL_CALLS_PER_TURN..] {
            assert!(is_error);
            assert!(content.contains("not executed"), "{content}");
            assert!(content.contains("call this again next turn"), "{content}");
        }
    }

    #[tokio::test]
    async fn a_mixed_turn_overlaps_its_leading_reads_then_runs_the_rest_in_order() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        // Three reads, one edit, one read: the leading reads overlap and see
        // the original file; the trailing read runs after the edit and sees
        // the new text. Results still land in call order.
        struct MixedTurn {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MixedTurn {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 1 {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let calls = [
                    ("r1", "read_file", r#"{"path":"note.txt"}"#),
                    ("r2", "read_file", r#"{"path":"note.txt"}"#),
                    ("r3", "read_file", r#"{"path":"note.txt"}"#),
                    (
                        "e1",
                        "edit_file",
                        r#"{"edits":[{"path":"note.txt","old":"before\n","new":"after\n"}]}"#,
                    ),
                    ("r4", "read_file", r#"{"path":"note.txt"}"#),
                ];
                let mut events = Vec::new();
                for (id, name, json) in calls {
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.to_owned(),
                        name: name.to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.to_owned(),
                        json: json.to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id: id.to_owned() }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "before\n").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MixedTurn {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop(
                vec![Message::user("read, edit, read")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        let requests = requests.lock().unwrap();
        let results: Vec<(&str, &str, bool)> = requests[1]
            .messages()
            .last()
            .unwrap()
            .content()
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => (call_id.as_str(), content.as_str(), *is_error),
                _ => panic!("tool-result message"),
            })
            .collect();
        assert_eq!(
            results.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            ["r1", "r2", "r3", "e1", "r4"]
        );
        for (id, content, is_error) in &results[..3] {
            assert!(!is_error, "{id}: {content}");
            assert!(content.contains("before"), "{id}: {content}");
        }
        assert!(!results[3].2, "{}", results[3].1);
        assert!(results[4].1.contains("after"), "{}", results[4].1);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn enforces_the_admitted_call_limit_and_checkpoint_request_contract() {
        struct TooManyInOneTurn;

        impl Provider for TooManyInOneTurn {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let events = (0..=MAX_ADMITTED_TOOL_CALLS_PER_TURN)
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

        // An empty checkpoint is a missed report, not a failure: the slice
        // still resets and the run finishes (ADR-0054 § 2). The live context
        // fills the empty turn with the placeholder assembly would insert.
        struct EmptyCheckpoint {
            turn: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for EmptyCheckpoint {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                if last_user_text(&request) == Some(SLICE_CONTINUATION_NOTICE) {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "finished after the missed report".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                if is_checkpoint_request(&request) {
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

        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            EmptyCheckpoint {
                turn: Mutex::new(0),
                requests: Arc::clone(&requests),
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
        assert!(
            matches!(events.last(), Some(RunEvent::Completed)),
            "{:?}",
            events.last()
        );
        let requests = requests.lock().unwrap();
        let continuation = requests.last().unwrap();
        let messages = continuation.messages();
        assert_eq!(
            messages[messages.len() - 3..],
            [
                Message::user(SLICE_CHECKPOINT_NOTICE),
                Message::assistant(EMPTY_TURN_PLACEHOLDER),
                Message::user(SLICE_CONTINUATION_NOTICE),
            ]
        );
    }

    /// When the turn after a slice report is the budget-final turn, it asks
    /// for no tool calls, so it is not told that tools are available again: it carries
    /// only the budget-final notice, and its turn records no continuation.
    #[tokio::test]
    async fn a_budget_final_turn_after_a_report_gets_no_continuation_notice() {
        struct ReportThenFinal {
            calls: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ReportThenFinal {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                if is_checkpoint_request(&request)
                    || request.tool_choice() == qq_provider::ToolChoice::None
                {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "report".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let mut calls = self.calls.lock().unwrap();
                let first = *calls;
                *calls += MAX_TOOL_CALLS_PER_TURN;
                let mut events = Vec::new();
                for index in first..first + MAX_TOOL_CALLS_PER_TURN {
                    let id = format!("call-{index}");
                    // One write per turn is progress, so the run reaches the
                    // slice checkpoint, not a stall report.
                    let (name, json) = if index == first {
                        ("write_file", r#"{"path":"progress.txt","content":"x"}"#)
                    } else {
                        ("read_file", r#"{"path":"note.txt"}"#)
                    };
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: name.to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: json.to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        // Sixteen-call turns fill the slice exactly: after sixteen of them
        // (256 calls) the next turn could pass the ceiling, so turn 17 is the
        // report. With 18 turns allowed, turn 18 is the reserved tool-free
        // final response, which is also the first turn of the next slice.
        let report_turn =
            u32::try_from(MAX_TOOL_CALLS_PER_SLICE / MAX_TOOL_CALLS_PER_TURN).unwrap() + 1;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            ReportThenFinal {
                calls: Mutex::new(0),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("long task")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_limits(
                    RunLimits {
                        max_model_turns: Some(report_turn + 1),
                        ..RunLimits::default()
                    },
                    None,
                ),
            )
            .collect::<Vec<_>>()
            .await;
        let requests = requests.lock().unwrap();
        let report_at = requests.iter().position(is_checkpoint_request).unwrap();
        let final_request = &requests[report_at + 1];
        assert_eq!(
            final_request.tool_choice(),
            qq_provider::ToolChoice::None,
            "the final response asks for no tool calls"
        );
        assert!(
            !final_request.tools().is_empty(),
            "the final response keeps its tools declared"
        );
        assert!(
            final_request
                .system()
                .is_some_and(|system| system.contains(BUDGET_FINAL_RESPONSE_NOTICE))
        );
        assert!(
            !final_request
                .messages()
                .contains(&Message::user(SLICE_CONTINUATION_NOTICE)),
            "a tool-free turn is never told tools are available again"
        );
        let notices = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { notice, .. } => Some(*notice),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notices.last(),
            Some(&None),
            "the final turn records no continuation: {notices:?}"
        );
        assert!(notices.contains(&Some(runtime::TurnNotice::Report)));
    }

    /// A run under Jev review can never reach a slice report: Jev admits one
    /// tool call per turn and at most 32 reviews per run, both far below the
    /// 241 executed calls that trigger a checkpoint, so the report turn can
    /// never become a Jev final candidate. If either bound is raised past the
    /// slice, report turns need an explicit Jev rule (ADR-0054 § 2).
    #[test]
    fn a_jev_run_cannot_reach_a_slice_report() {
        let reviews = usize::from(runtime::MAX_CHECKPOINT_REVIEWS_PER_RUN);
        assert!(
            reviews < MAX_TOOL_CALLS_PER_SLICE - MAX_TOOL_CALLS_PER_TURN,
            "Jev's review cap now reaches the slice checkpoint"
        );
    }

    /// A checkpoint reply with no content *and no usage* after fresh tool
    /// results is a gateway that swallowed a failure, not a missed report:
    /// it is retried like any transient fault. The report notice is placed
    /// once and the retry's report is the one that stands. (A metered empty
    /// reply is a missed report; see the test above.)
    #[tokio::test(start_paused = true)]
    async fn an_unmetered_empty_checkpoint_is_retried_as_a_transient_fault() {
        struct SwallowedCheckpoint {
            calls: Mutex<usize>,
            checkpoint_attempts: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for SwallowedCheckpoint {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                if is_checkpoint_request(&request) {
                    let mut attempts = self.checkpoint_attempts.lock().unwrap();
                    *attempts += 1;
                    if *attempts == 1 {
                        return Box::pin(stream::iter([Ok(ProviderEvent::Completed {
                            usage: None,
                        })]));
                    }
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "report after retry".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                if last_user_text(&request) == Some(SLICE_CONTINUATION_NOTICE) {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let mut calls = self.calls.lock().unwrap();
                let first = *calls;
                *calls += MAX_TOOL_CALLS_PER_TURN;
                let mut events = Vec::new();
                for index in first..first + MAX_TOOL_CALLS_PER_TURN {
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
        let runtime = Runtime::new(
            SwallowedCheckpoint {
                calls: Mutex::new(0),
                checkpoint_attempts: Mutex::new(0),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RunEvent::Completed)),
            "{:?}",
            events.last()
        );
        let requests = requests.lock().unwrap();
        // The swallowed reply was re-issued: two checkpoint requests.
        assert_eq!(
            requests.iter().filter(|r| is_checkpoint_request(r)).count(),
            2
        );
        let last = requests.last().unwrap().messages();
        assert_eq!(
            last.iter()
                .filter(|message| **message == Message::user(SLICE_CHECKPOINT_NOTICE))
                .count(),
            1
        );
        assert!(last.contains(&Message::assistant("report after retry")));
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

    /// A gateway that swallows an upstream failure: turn one asks for a
    /// read, then every reply to the result is `prelude` (a gateway that
    /// flushes a trailing newline before `[DONE]`) followed by exactly one
    /// `Completed` carrying the given usage.
    struct EmptyCompletionProvider {
        calls: Arc<std::sync::atomic::AtomicU32>,
        usage: Option<qq_provider::ProviderUsage>,
        prelude: Option<&'static str>,
    }

    impl Provider for EmptyCompletionProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let turn = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if turn == 0 {
                return Box::pin(stream::iter([
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
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let usage = self.usage;
            let mut events = Vec::with_capacity(2);
            if let Some(text) = self.prelude {
                events.push(Ok(ProviderEvent::OutputTextDelta {
                    text: text.to_owned(),
                }));
            }
            events.push(Ok(ProviderEvent::Completed { usage }));
            Box::pin(stream::iter(events))
        }
    }

    /// ENG-952: two production runs settled `completed` after the gateway
    /// returned a bare `[DONE]` — no text, no calls, no usage — in reply to
    /// fresh tool results. Silence is not an answer: it is the transient
    /// fault a cut stream is, and takes the same bounded retry then pause.
    #[tokio::test(start_paused = true)]
    async fn an_empty_completion_without_usage_is_a_transient_fault_not_an_answer() {
        let fast = TurnRecoveryPolicy::new(Duration::from_millis(1), Duration::from_millis(1));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "note\n").unwrap();
        // Whitespace before the terminal event is still nothing: the
        // transcript would drop it (`has_content` trims), so the fault
        // predicate measures emptiness the same way.
        for prelude in [None, Some("\n"), Some(" \n\t")] {
            let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let runtime = Runtime::new(
                EmptyCompletionProvider {
                    calls: Arc::clone(&calls),
                    usage: None,
                    prelude,
                },
                "gpt-test",
                256,
            )
            .unwrap()
            .with_turn_recovery(fast);
            let events = runtime
                .run_messages_in_workspace(
                    vec![Message::user("read it")],
                    directory.path().to_owned(),
                )
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                u32::from(MAX_TURN_RETRIES) + 2,
                "prelude {prelude:?}: the read turn, one empty reply, and every retry of it"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RuntimeEvent::Completed { .. })),
                "prelude {prelude:?}: an empty reply must never settle completed: {events:?}"
            );
            assert!(
                matches!(
                    events.last(),
                    Some(RuntimeEvent::Paused { pause })
                        if pause.kind == RunFailureKind::ProviderTransport
                            && pause.message.contains("no content and no usage")
                ),
                "prelude {prelude:?}: {events:?}"
            );
        }

        // Real text without usage is an answer, exactly as every
        // `Completed { usage: None }` provider in this module is.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            EmptyCompletionProvider {
                calls: Arc::clone(&calls),
                usage: None,
                prelude: Some("ok"),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("read it")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );

        // A provider that measured the request and genuinely returned an
        // empty answer is a real (if useless) completion, not a fault: the
        // run settles once and never resends.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            EmptyCompletionProvider {
                calls: Arc::clone(&calls),
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 0,
                    reasoning_tokens: None,
                }),
                prelude: None,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("read it")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn credential_load_timeout_does_not_retry_the_turn() {
        for message in [
            "credential loading timed out",
            "credential loading capacity is exhausted",
        ] {
            let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
            struct CredentialFailure {
                calls: Arc<std::sync::atomic::AtomicU32>,
                message: &'static str,
            }
            impl Provider for CredentialFailure {
                fn stream(&self, _: ModelRequest) -> ProviderStream {
                    self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let message = self.message.to_owned();
                    Box::pin(stream::once(async move {
                        Err(ProviderError::ResponseFailed {
                            kind: qq_provider::ProviderErrorKind::Authentication,
                            message,
                        })
                    }))
                }
            }
            let runtime = Runtime::new(
                CredentialFailure {
                    calls: Arc::clone(&calls),
                    message,
                },
                "gpt-test",
                256,
            )
            .unwrap()
            .with_turn_recovery(TurnRecoveryPolicy::new(
                Duration::from_millis(1),
                Duration::from_millis(1),
            ));
            let events = runtime
                .run(RunCommand::new("hello"))
                .collect::<Vec<_>>()
                .await;
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(
                matches!(
                    events.last(),
                    Some(RunEvent::Failed {
                        kind: RunFailureKind::ProviderAuthentication,
                        message: got,
                    }) if got.contains(message) && !got.contains("paused after")
                ),
                "{events:?}"
            );
        }
    }

    /// Steering accepted while tools ran joins the request as a user message
    /// after the results (`apply_steering` at the turn boundary), so the
    /// last message is no longer the results. The empty completion that
    /// follows is still a fault: the results are what the model was answering.
    #[tokio::test(start_paused = true)]
    async fn an_empty_completion_after_tool_results_and_steering_is_still_a_fault() {
        let fast = TurnRecoveryPolicy::new(Duration::from_millis(1), Duration::from_millis(1));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "note\n").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            EmptyCompletionProvider {
                calls: Arc::clone(&calls),
                usage: None,
                prelude: None,
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
        let (sender, receiver) = runtime::steering_channel();
        let message_id = qq_protocol::MessageId::from_bytes([7; 16]);
        // Queued before the run starts: the first boundary after the read's
        // result drains it, so the empty turn's request ends with this
        // user message, not with the tool result.
        sender
            .messages
            .send(runtime::SteeringMessage::text(message_id, "also summarize"))
            .await
            .unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("read it")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None).with_steering(receiver),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            events.iter().any(|event| matches!(event,
                RuntimeEvent::SteeringApplied { message_id: applied, .. } if *applied == message_id
            )),
            "steering must join after the tool result: {events:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            u32::from(MAX_TURN_RETRIES) + 2,
            "the read turn, one empty reply, and every retry of it"
        );
        assert!(
            matches!(
                events.last(),
                Some(RuntimeEvent::Paused { pause })
                    if pause.kind == RunFailureKind::ProviderTransport
                        && pause.message.contains("no content and no usage")
            ),
            "{events:?}"
        );
    }

    /// Two-phase retry ownership (ADR-0040). The provider owns resends while
    /// nothing has streamed; the run owns recovery of the *turn*: a transient
    /// fault commits whatever arrived, re-issues the turn up to
    /// `MAX_TURN_RETRIES` times, and then settles `paused` rather than failed.
    /// Faults that would recur (auth, invalid request, malformed stream) still
    /// fail at once with no resend.
    #[tokio::test(start_paused = true)]
    async fn transient_faults_retry_the_turn_and_exhaustion_pauses() {
        let fast = TurnRecoveryPolicy::new(Duration::from_millis(1), Duration::from_millis(1));
        type Case = (fn() -> Option<ProviderError>, RunFailureKind, &'static str);
        let transient: [Case; 3] = [
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
                RunFailureKind::ProviderTransport,
                "ended without a terminal event",
            ),
        ];
        for (failure, kind, needle) in transient {
            let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let runtime = Runtime::new(
                FailingProvider {
                    calls: Arc::clone(&calls),
                    failure,
                },
                "gpt-test",
                256,
            )
            .unwrap()
            .with_turn_recovery(fast);
            let events = runtime
                .run(RunCommand::new("hello"))
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                u32::from(MAX_TURN_RETRIES) + 1,
                "{kind:?}: one send plus every retry"
            );
            // The direct path has no session to resume from, so the pause
            // surfaces as the last fault with the retries it spent.
            assert!(
                matches!(
                    events.last(),
                    Some(RunEvent::Failed { kind: got, message })
                        if *got == kind
                            && message.contains(needle)
                            && message.contains("paused after 5 turn retries")
                ),
                "{kind:?}: {events:?}"
            );
        }

        // A fault the retry cannot fix fails at once with no resend.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            FailingProvider {
                calls: Arc::clone(&calls),
                failure: || {
                    Some(ProviderError::Api {
                        status: 401,
                        message: "bad key".to_owned(),
                    })
                },
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
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

        // After visible output a fault commits the partial turn and the
        // retry continues it: the answer is the two halves.
        struct RecoversMidStream {
            calls: Arc<std::sync::atomic::AtomicU32>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for RecoversMidStream {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.requests.lock().unwrap().push(request);
                match call {
                    // 529 twice mid-stream, then a clean finish.
                    0 | 1 => Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: format!("part{call} "),
                        }),
                        Err(ProviderError::Api {
                            status: 529,
                            message: "overloaded_error".to_owned(),
                        }),
                    ])),
                    _ => Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ])),
                }
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            RecoversMidStream {
                calls: Arc::clone(&calls),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(fast);
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::OutputTextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "part0 part1 done");
        assert!(matches!(events.last(), Some(RunEvent::Completed)));
        // Each retry carried the partial turn and the continue notice, so
        // the model resumes rather than restarts.
        let requests = requests.lock().unwrap();
        let third = requests[2].messages();
        assert!(matches!(
            third[third.len() - 1].content(),
            [ContentBlock::Text { text }] if text == TURN_RETRY_CONTINUE_NOTICE
        ));
        assert!(matches!(
            third[third.len() - 2].content(),
            [ContentBlock::Text { text }] if text == "part1 "
        ));
    }

    /// The retry count is per turn: a run that completes a turn between
    /// faults never pauses, however many isolated blips it meets.
    #[tokio::test(start_paused = true)]
    async fn the_turn_retry_allowance_resets_on_a_completed_turn() {
        struct BlipEveryTurn {
            calls: Arc<std::sync::atomic::AtomicU32>,
        }

        impl Provider for BlipEveryTurn {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let tool_results = request
                    .messages()
                    .iter()
                    .flat_map(|message| message.content())
                    .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
                    .count();
                // Odd calls fault mid-stream; even calls complete. Eight
                // completed tool turns, each preceded by one fault, is more
                // faults than one turn may spend.
                if call.is_multiple_of(2) {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "thinking".to_owned(),
                        }),
                        Err(ProviderError::Transport("blip".to_owned())),
                    ]));
                }
                if tool_results >= 8 {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "finished".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let id = format!("call-{tool_results}");
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: r#"{"path":"note.txt"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let runtime = Runtime::new(
            BlipEveryTurn {
                calls: Arc::clone(&calls),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_turn_recovery(TurnRecoveryPolicy::new(
            Duration::from_millis(1),
            Duration::from_millis(1),
        ));
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("read a lot")], directory.path().into())
            .collect::<Vec<_>>()
            .await;
        // 9 faults (one per turn) + 9 completions; well past MAX_TURN_RETRIES
        // in total, never more than one per turn.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 18);
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{:?}",
            events.last()
        );
        let retries = events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::TurnRetrying { attempt: 1, .. }))
            .count();
        assert_eq!(retries, 9);
        assert!(!events.iter().any(|event| matches!(
            event,
            RuntimeEvent::TurnRetrying { attempt: 2.., .. } | RuntimeEvent::Paused { .. }
        )));
    }

    /// Cancellation and the run deadline both cut a retry sleep short.
    #[tokio::test(start_paused = true)]
    async fn a_retry_sleep_yields_to_cancellation_and_the_deadline() {
        struct AlwaysBlips;
        impl Provider for AlwaysBlips {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "x".to_owned(),
                    }),
                    Err(ProviderError::Transport("blip".to_owned())),
                ]))
            }
        }
        struct AllowAllGate;
        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }
        let slow = TurnRecoveryPolicy::new(Duration::from_secs(60), Duration::from_secs(60));
        let directory = tempfile::tempdir().unwrap();

        // Cancel during the first backoff: the stream ends without a
        // terminal event of its own (the session layer settles Cancelled).
        let runtime = Runtime::new(AlwaysBlips, "gpt-test", 256)
            .unwrap()
            .with_turn_recovery(slow);
        let cancellation = RunCancellation::new();
        let canceller = cancellation.clone();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        });
        let events = runtime
            .run_loop(
                vec![Message::user("go")],
                directory.path().to_owned(),
                cancellation,
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        cancel.await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::TurnRetrying { attempt: 1, .. }))
        );
        assert!(
            matches!(events.last(), Some(RuntimeEvent::TurnRetrying { .. })),
            "{:?}",
            events.last()
        );

        // A run deadline inside the backoff settles as that budget, not as a
        // pause and not after the full sleep.
        let runtime = Runtime::new(AlwaysBlips, "gpt-test", 256)
            .unwrap()
            .with_turn_recovery(slow);
        let plan = Arc::new(
            LoadedRuntime::compile_blocking(
                &runtime,
                runtime.embedded_resolved_model(),
                directory.path().to_owned(),
            )
            .unwrap()
            .plan,
        );
        let mut capabilities = RunCapabilities::user(None);
        capabilities.limits = RunLimits {
            max_duration_ms: Some(10_000),
            ..RunLimits::default()
        };
        let started = tokio::time::Instant::now();
        let events = plan
            .execute(
                vec![Message::user("go")],
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
                capabilities,
            )
            .collect::<Vec<_>>()
            .await;
        assert!(started.elapsed() < Duration::from_secs(60));
        assert!(
            matches!(
                events.last(),
                Some(RuntimeEvent::BudgetExhausted { exhaustion })
                    if exhaustion.limit == BudgetLimitKind::Duration
            ),
            "{:?}",
            events.last()
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

        fn call(
            &self,
            name: String,
            arguments: String,
            _cancelled: RunCancellation,
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

        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
        assert_eq!(
            requests[0].tools().len(),
            9 + usize::from(cfg!(feature = "tool-fetch"))
        );
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
                RunCancellation::new(),
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
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "an MCP failure must never fail the run"
        );

        let oversized = format!("{}\n", "x".repeat(1_023)).repeat(200);
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
                RunCancellation::new(),
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
        assert!(result.len() <= tools::MAX_MODEL_TEXT_BYTES);
        assert!(result.contains("…[qq: "));
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));

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
                RunCancellation::new(),
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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

    /// Delivers a fixed set of children once.
    struct DeliveringSpawner {
        delivered: Mutex<Vec<runtime::DeliveredChild>>,
    }

    impl SubagentSpawner for DeliveringSpawner {
        fn acknowledge(&self, _: ToolCallId) {}
        fn drain(&self) -> runtime::ChildDrainFuture {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn spawn(&self, _: SpawnRequest) -> SpawnAgentFuture {
            unreachable!("this spawner only delivers")
        }
        fn deliver(&self, _: u32, _: runtime::ReportDelivery) -> runtime::DeliverFuture {
            let delivered = std::mem::take(&mut *self.delivered.lock().unwrap());
            Box::pin(std::future::ready(Ok(delivered)))
        }
    }

    /// Only a child that answered is progress for its parent (ADR-0054 § 1),
    /// exactly as only a successful blocking spawn result is. A delivered
    /// notice that the child failed or was cancelled is still context the
    /// parent sees and spend it pays, but it does not hold off the parent's
    /// stall report: a parent spawning children that fail must still report.
    /// An interim report from a child still working is neither progress nor
    /// an answer.
    #[tokio::test]
    async fn only_a_delivered_answer_restarts_the_stall_count() {
        let child = |answered: bool, interim: bool| runtime::DeliveredChild {
            notice: format!("child answered: {answered}"),
            answered,
            interim,
            spend: SpawnAgentSpend::NONE,
        };
        for (answered, interim, restarts) in [
            (false, false, false),
            (true, false, true),
            (false, true, false),
        ] {
            let spawner: Arc<dyn SubagentSpawner> = Arc::new(DeliveringSpawner {
                delivered: Mutex::new(vec![child(answered, interim)]),
            });
            let mut stall = runtime::StallScope::new(runtime::StallPolicy::Root);
            for _ in 0..runtime::STALL_REPORT_CALLS {
                stall.settled(false);
            }
            assert_eq!(stall.due(false), runtime::ReportDue::Report);
            let mut messages = vec![Message::user("prompt")];
            let mut bytes = 0;
            let mut budget =
                BudgetMeter::new(RunLimits::default(), None, tokio::time::Instant::now());
            let delivered = deliver_children(
                &spawner,
                Boundary {
                    turn_ordinal: 2,
                    reports: runtime::ReportDelivery::Always,
                },
                &mut messages,
                &mut bytes,
                &mut budget,
                &mut stall,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                delivered,
                Delivered {
                    answers: usize::from(!interim),
                    reports: usize::from(interim),
                }
            );
            // Either way the notice is in context.
            assert_eq!(
                messages.last().unwrap(),
                &Message::user(format!("child answered: {answered}"))
            );
            assert_eq!(
                stall.due(false) == runtime::ReportDue::None,
                restarts,
                "answered: {answered}"
            );
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
        let requests = requests.lock().unwrap();
        // The delegation tools come and go together: a run that cannot
        // spawn has no background children to wait for or cancel.
        for tool in [tools::SPAWN_AGENT_TOOL, "wait_agents", "cancel_agent"] {
            assert!(
                !requests[0].tools().iter().any(|spec| spec.name() == tool),
                "{tool}"
            );
        }
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

    /// The tool-free wait asks for reports only together with an answer
    /// (ADR-0054 § 4), so the one delivery that ends the wait is the
    /// boundary's only one: one budget, before any steering, as replay
    /// places it. The turn-top boundary takes reports from children still
    /// working.
    #[tokio::test]
    async fn a_tool_free_wait_takes_reports_only_with_an_answer() {
        struct AllowAllGate;
        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }
        /// One detached child that reports at every boundary that accepts
        /// reports, and answers on the third wake of the wait.
        struct Reporter {
            asked: Mutex<Vec<runtime::ReportDelivery>>,
            wakes: Mutex<usize>,
            answered: std::sync::atomic::AtomicBool,
        }
        impl SubagentSpawner for Reporter {
            fn acknowledge(&self, _: ToolCallId) {}
            fn drain(&self) -> runtime::ChildDrainFuture {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn spawn(&self, _: SpawnRequest) -> SpawnAgentFuture {
                Box::pin(std::future::ready(SpawnAgentOutcome {
                    content: "started".to_owned(),
                    is_error: false,
                    spend: SpawnAgentSpend::NONE,
                    session_id: None,
                    detached: true,
                }))
            }
            fn outstanding_detached(&self) -> usize {
                usize::from(!self.answered.load(std::sync::atomic::Ordering::SeqCst))
            }
            fn child_settled(&self) -> runtime::ChildWaitFuture {
                Box::pin(std::future::ready(()))
            }
            fn deliver(&self, _: u32, reports: runtime::ReportDelivery) -> runtime::DeliverFuture {
                self.asked.lock().unwrap().push(reports);
                let report = runtime::DeliveredChild {
                    notice: "interim report".to_owned(),
                    answered: false,
                    interim: true,
                    spend: SpawnAgentSpend::NONE,
                };
                let batch = match reports {
                    runtime::ReportDelivery::Always => vec![report],
                    runtime::ReportDelivery::WithAnswers => {
                        let mut wakes = self.wakes.lock().unwrap();
                        *wakes += 1;
                        if *wakes < 3 {
                            Vec::new()
                        } else {
                            self.answered
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            vec![
                                runtime::DeliveredChild {
                                    notice: "the answer".to_owned(),
                                    answered: true,
                                    interim: false,
                                    spend: SpawnAgentSpend::NONE,
                                },
                                report,
                            ]
                        }
                    }
                };
                Box::pin(std::future::ready(Ok(batch)))
            }
        }
        let spawner = Arc::new(Reporter {
            asked: Mutex::new(Vec::new()),
            wakes: Mutex::new(0),
            answered: std::sync::atomic::AtomicBool::new(false),
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        struct Recording {
            inner: SpawnCallProvider,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }
        impl Provider for Recording {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                self.inner.stream(request)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            Recording {
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
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("go")],
                directory.path().to_owned(),
                RunCancellation::new(),
                Arc::new(AllowAllGate),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(Some(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>)),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
        // The turn-top boundaries (turns 1 and 2; this stub is outstanding
        // from the start) take reports; the wait's three wakes do not, and
        // the third, with the answer, ends it. Turn 3 then needs no boundary
        // delivery: the wait already delivered for it.
        use runtime::ReportDelivery::{Always, WithAnswers};
        assert_eq!(
            spawner.asked.lock().unwrap().as_slice(),
            [Always, Always, WithAnswers, WithAnswers, WithAnswers]
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        // Turn 2 carried the turn-top report; turn 3 the reply, the answer,
        // then the report that came with it.
        let tail = requests[2]
            .messages()
            .iter()
            .rev()
            .take(3)
            .map(|message| (message.role(), message.content().to_vec()))
            .collect::<Vec<_>>();
        let text = |text: &str| {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        };
        assert_eq!(
            tail,
            [
                (Role::User, text("interim report")),
                (Role::User, text("the answer")),
                (Role::Assistant, text("done")),
            ]
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
                content: format!("{}\n", "x".repeat(1_023)).repeat(200),
                is_error: false,
                spend: SpawnAgentSpend::NONE,
                session_id: None,
                detached: false,
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
                RunCancellation::new(),
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
        assert!(result.len() <= tools::MAX_MODEL_TEXT_BYTES);
        assert!(result.contains("…[qq: "));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
                    detached: false,
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
                    RunCancellation::new(),
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
                detached: false,
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
                RunCancellation::new(),
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
                detached: false,
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
                RunCancellation::new(),
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
    fn subagent_prompts_say_a_parent_is_waiting_and_read_children_drop_the_implement_line() {
        const IMPLEMENT: &str = "- Implement requested changes rather than stopping at analysis";
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let render = |subagent| {
            runtime::agent_system_prompt(
                workspace,
                &tools::specs(),
                runtime::PromptSections {
                    subagent,
                    ..runtime::PromptSections::default()
                },
                &instructions,
                None,
                None,
            )
        };
        let root = render(None);
        let read = render(Some(runtime::SubagentAuthority::Read));
        let write = render(Some(runtime::SubagentAuthority::Write));

        // A root, including a read-only one, keeps today's conventions and
        // has no sub-agent section.
        assert!(root.contains(IMPLEMENT));
        assert!(!root.contains("Sub-agent:"));
        // Every child is told who reads its reply and how to shape it.
        for child in [&read, &write] {
            assert!(child.contains("\n\nSub-agent:\n"), "{child}");
            assert!(child.contains("only your final reply reaches it"));
            assert!(child.contains("Stop as soon as you can answer"));
            assert!(child.contains("answer first, then the evidence as path:line"));
            assert!(child.contains("Do not re-read text that is still in your context"));
        }
        // A read child cannot implement anything, so the line that tells it
        // to keep going until it has would only keep it reading.
        assert!(!read.contains(IMPLEMENT));
        assert!(read.contains("You cannot change files or run commands"));
        assert!(write.contains(IMPLEMENT));
        // Only those lines differ from the root prompt.
        let without_section = |prompt: &str| {
            let (head, _) = prompt.split_once("\n\nSub-agent:\n").unwrap();
            head.to_owned()
        };
        assert_eq!(without_section(&write), root);
        assert_eq!(
            without_section(&read),
            root.replacen(
                "- Implement requested changes rather than stopping at analysis unless the user \
                 requested analysis-only work.\n",
                "",
                1
            )
        );
    }

    #[test]
    fn delegation_authority_guidance_matches_the_spawn_schema() {
        let instructions = workspace::WorkspaceInstructions::empty();
        for write_children in [false, true] {
            let delegation = DelegationRoster {
                write_children,
                ..DelegationRoster::default()
            };
            let specs = [tools::spawn_agent_spec(&[], &delegation)];
            let prompt = runtime::agent_system_prompt(
                std::path::Path::new("/tmp/qq-prompt-test"),
                &specs,
                runtime::PromptSections::default(),
                &instructions,
                None,
                None,
            );
            assert_eq!(prompt.contains("authority: write"), write_children);
            assert_eq!(
                prompt.contains("one-shot read-only sub-agent"),
                !write_children
            );
            if write_children {
                assert!(prompt.contains("read by default"));
                assert!(prompt.contains("reviewer_model"));
                assert!(prompt.contains("one write sub-agent"));
                assert!(prompt.contains("implementation task"));
            }
        }
    }

    #[test]
    fn delegation_guidance_asks_for_a_question_a_purpose_and_an_answer_shape() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let mut specs = tools::specs();
        specs.push(tools::spawn_agent_spec(&[], &DelegationRoster::default()));
        let prompt = runtime::agent_system_prompt(
            workspace,
            &specs,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert!(prompt.contains(
            "- Write the brief as a question to answer, what the answer is for, and the shape \
             you want back"
        ));
        assert!(prompt.contains("Prefer several narrow briefs over one broad one"));
        let spawn = specs.last().unwrap();
        let task = spawn.input_schema().get();
        assert!(
            task.contains("the question to answer, what the answer is for, and the answer shape"),
            "{task}"
        );
    }

    #[test]
    #[cfg(feature = "tool-fetch")]
    fn a_root_prompt_and_tools_change_only_by_the_brief_guidance() {
        // Golden against prompt version 14: a root's system
        // prompt gains only the delegation bullet, and its tools block only
        // the `spawn_agent` `task` description. Everything else is
        // byte-identical, so a root keeps its prompt-cache prefix up to the
        // Delegation section.
        const NEW_BULLET: &str = "- Write the brief as a question to answer, what the answer is \
            for, and the shape you want back (a list of path:line findings, a yes or no with \
            evidence, a short plan). A sub-agent stops when it can answer, so an open-ended brief \
            gets a long search and a late answer. Prefer several narrow briefs over one broad \
            one.\n";
        const OLD_TASK: &str = "A complete, self-contained brief for the sub-agent.";
        const BLOCKING_SPAWN: &str = "- spawn_agent runs a one-shot read-only sub-agent in this \
            workspace from a self-contained task brief and returns only its final answer.\n";
        const BACKGROUND_SPAWN: &str = "- spawn_agent starts a one-shot read-only sub-agent in \
            this workspace from a self-contained task brief. It usually runs in the background: \
            the call returns at once, and the sub-agent's final answer arrives at a later turn as \
            a runtime notice. Keep working on what does not depend on it; a reply without tool \
            calls while sub-agents are working waits for their answers.\n";
        const CONTROL_BULLET: &str = "- A sub-agent still working may also send its latest \
            progress report as a notice. Call wait_agents when your next step needs specific \
            answers, and cancel_agent for a sub-agent whose answer you no longer need.\n";
        const CONCURRENT: &str = "because sub-agents run concurrently.";
        const CONCURRENT_WITH_YOU: &str =
            "because sub-agents run concurrently with each other and with you.";
        const BLOCKING_DESCRIPTION: &str = "Delegate one self-contained task to a read-only \
            sub-agent in this workspace and receive only its final answer. Worth it when the raw \
            evidence would dwarf the distilled answer and you will not need that evidence \
            verbatim later; several independent questions can be delegated in parallel. Single \
            reads, searches, and quick lookups are cheaper inline. The task brief must carry \
            everything the sub-agent needs: it starts with no other context. Omit model";
        const BACKGROUND_DESCRIPTION: &str = "Delegate one self-contained task to a read-only \
            sub-agent in this workspace; only its final answer comes back, usually later as a \
            runtime notice while you keep working. Worth it when the raw evidence would dwarf the \
            distilled answer and you will not need that evidence verbatim later; several \
            independent questions can be delegated in parallel. Single reads, searches, and \
            quick lookups are cheaper inline. The task brief must carry everything the sub-agent \
            needs: it starts with no other context. Omit model";
        const NEW_TASK: &str = "A complete, self-contained brief for the sub-agent: the question \
            to answer, what the answer is for, and the answer shape you want back. The sub-agent \
            starts with no other context and stops once it can answer.";
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let mut specs = tools::specs();
        specs.push(tools::spawn_agent_spec(&[], &DelegationRoster::default()));
        let prompt = runtime::agent_system_prompt(
            workspace,
            &specs,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert!(prompt.contains(NEW_BULLET), "{prompt}");
        // Prompt 17 (ADR-0054 § 4, AP4.2) adds only the control bullet, and
        // prompt 16 rewords only the first and last delegation bullets; undo
        // them, then the brief bullet.
        assert!(prompt.contains(CONTROL_BULLET), "{prompt}");
        let v16 = prompt.replacen(CONTROL_BULLET, "", 1);
        assert!(v16.contains(BACKGROUND_SPAWN), "{v16}");
        assert!(v16.contains(CONCURRENT_WITH_YOU), "{v16}");
        let v15 = v16.replacen(BACKGROUND_SPAWN, BLOCKING_SPAWN, 1).replacen(
            CONCURRENT_WITH_YOU,
            CONCURRENT,
            1,
        );
        let v14 = v15.replacen(NEW_BULLET, "", 1);
        assert_ne!(v14, v15);
        assert_eq!(
            format!("{:x}", Sha256::digest(v14.as_bytes())),
            "383e1411a666c1e00b7acbfa598eb9cbe4af5224eb6892614d11511ea5305542"
        );
        // Every built-in declaration is unchanged.
        assert_eq!(
            runtime::tool_schema_measurement(&tools::specs())
                .hash
                .to_string(),
            "568cef80e021a4c69625eb992086993b9c0f43857ae74ff253f70253a752f24f"
        );
        let spawn = specs.last().unwrap();
        assert!(
            spawn.description().starts_with(BACKGROUND_DESCRIPTION),
            "{}",
            spawn.description()
        );
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(
                    spawn
                        .description()
                        .replacen(BACKGROUND_DESCRIPTION, BLOCKING_DESCRIPTION, 1)
                        .as_bytes()
                )
            ),
            "09105474547d899bf0bf5346f2c72079425d0f0c236c9a201378a33f0421c5ac"
        );
        let schema = spawn.input_schema().get();
        assert!(schema.contains(NEW_TASK), "{schema}");
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(schema.replacen(NEW_TASK, OLD_TASK, 1).as_bytes())
            ),
            "deb5f0866f1f90db28995823bd38e3bbdeaf565b35cf0ffaf24c3812cc7b1762"
        );
    }

    #[test]
    fn a_read_only_root_keeps_the_implement_line_and_has_no_subagent_section() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        // The schemas a ReadOnly root is offered: the mutating, shell and
        // network built-ins are withheld (`catalog.rs` read-only filter).
        let read_only = tools::specs()
            .into_iter()
            .filter(|spec| matches!(spec.name(), "read_file" | "tree" | "search" | "ask_user"))
            .collect::<Vec<_>>();
        assert_eq!(read_only.len(), 4);
        let prompt = runtime::agent_system_prompt(
            workspace,
            &read_only,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert!(prompt.contains("- Implement requested changes rather than stopping at analysis"));
        assert!(!prompt.contains("Sub-agent:"));
    }

    #[test]
    fn agent_prompt_advertises_fetch_only_when_declared() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let specs = tools::specs();
        let prompt = runtime::agent_system_prompt(
            workspace,
            &specs,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert_eq!(
            prompt.contains("fetch reads one public"),
            cfg!(feature = "tool-fetch")
        );
        assert_eq!(
            prompt.contains("Prefer fetch over shell"),
            cfg!(feature = "tool-fetch")
        );
        let without: Vec<_> = specs
            .into_iter()
            .filter(|spec| spec.name() != "fetch")
            .collect();
        let prompt = runtime::agent_system_prompt(
            workspace,
            &without,
            runtime::PromptSections::default(),
            &instructions,
            None,
            None,
        );
        assert!(!prompt.contains("fetch reads one public"));
        assert!(!prompt.contains("Prefer fetch over shell"));
    }

    #[test]
    fn agent_prompt_teaches_delegation_only_when_spawn_agent_is_declared() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let instructions = workspace::WorkspaceInstructions::empty();
        let without = runtime::agent_system_prompt(
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
        let with = runtime::agent_system_prompt(
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
        use qq_protocol::{
            DelegationRole, DelegationRoster, DelegationRosterEntry, ReasoningEffort,
        };
        let entry = |route: &str, role| DelegationRosterEntry {
            route: route.to_owned(),
            role,
            note: None,
            effort: None,
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
                Some(DelegationRole::Balanced),
                None,
            ),
            Ok((Some("openai/fast".to_owned()), Some(ReasoningEffort::Low)))
        );
        assert!(
            resolve_delegation_route(&roster, Some("openai/other".to_owned()), None, None)
                .unwrap_err()
                .contains("not on the delegation roster")
        );
        // Role maps to the first entry declaring it.
        assert_eq!(
            resolve_delegation_route(&roster, None, Some(DelegationRole::Balanced), None),
            Ok((
                Some("anthropic/balanced".to_owned()),
                Some(ReasoningEffort::Medium)
            ))
        );
        // Default role when neither is given.
        assert_eq!(
            resolve_delegation_route(&roster, None, None, None),
            Ok((
                Some("anthropic/balanced".to_owned()),
                Some(ReasoningEffort::Medium)
            ))
        );
        // A role nobody declares is a tool error naming it.
        assert!(
            resolve_delegation_route(&roster, None, Some(DelegationRole::Strong), None)
                .unwrap_err()
                .contains("strong role")
        );

        // Without a roster the legacy path is untouched: any model passes
        // through for spawn-time validation, and role is refused.
        let none = DelegationRoster::default();
        assert_eq!(
            resolve_delegation_route(&none, Some("x/y".to_owned()), None, None),
            Ok((Some("x/y".to_owned()), None))
        );
        assert_eq!(
            resolve_delegation_route(&none, None, None, None),
            Ok((None, None))
        );
        assert!(
            resolve_delegation_route(&none, None, Some(DelegationRole::Fast), None)
                .unwrap_err()
                .contains("no delegation roster")
        );
    }

    #[test]
    fn child_effort_derives_from_the_role_and_never_exceeds_the_parent() {
        // RR8.4: the audited failure was a read-only review child inheriting
        // `max` and spending 14 k reasoning tokens per read_file turn.
        use qq_protocol::{
            DelegationRole, DelegationRosterEntry, ReasoningEffort, child_reasoning_effort,
        };
        let entry = |role, effort| DelegationRosterEntry {
            route: "p/m".to_owned(),
            role,
            note: None,
            effort,
            context_window: None,
            max_output_tokens: None,
            relative_cost_permille: None,
        };
        let fast = entry(DelegationRole::Fast, None);
        let balanced = entry(DelegationRole::Balanced, None);
        let strong = entry(DelegationRole::Strong, None);
        // Parent at max: fast and balanced are capped, strong inherits.
        assert_eq!(
            child_reasoning_effort(&fast, Some(ReasoningEffort::Max)),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            child_reasoning_effort(&balanced, Some(ReasoningEffort::Max)),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            child_reasoning_effort(&strong, Some(ReasoningEffort::Max)),
            Some(ReasoningEffort::Max)
        );
        // A parent already below the role ceiling is not raised.
        assert_eq!(
            child_reasoning_effort(&balanced, Some(ReasoningEffort::Minimal)),
            Some(ReasoningEffort::Minimal)
        );
        // Unpinned or provider-default parent: the role ceiling applies.
        assert_eq!(
            child_reasoning_effort(&fast, None),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            child_reasoning_effort(&fast, Some(ReasoningEffort::Default)),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(child_reasoning_effort(&strong, None), None);
        // An explicit roster effort wins over everything.
        assert_eq!(
            child_reasoning_effort(
                &entry(DelegationRole::Fast, Some(ReasoningEffort::High)),
                Some(ReasoningEffort::Low)
            ),
            Some(ReasoningEffort::High)
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
                    effort: None,
                    context_window: Some(400_000),
                    max_output_tokens: None,
                    relative_cost_permille: Some(150),
                },
                DelegationRosterEntry {
                    route: "anthropic/same".to_owned(),
                    role: DelegationRole::Balanced,
                    note: None,
                    effort: None,
                    context_window: Some(200_000),
                    max_output_tokens: None,
                    relative_cost_permille: Some(1000),
                },
                DelegationRosterEntry {
                    route: "anthropic/strong".to_owned(),
                    role: DelegationRole::Strong,
                    note: None,
                    effort: None,
                    context_window: None,
                    max_output_tokens: None,
                    relative_cost_permille: Some(2_500),
                },
                DelegationRosterEntry {
                    route: "custom/unpriced".to_owned(),
                    role: DelegationRole::Strong,
                    note: None,
                    effort: None,
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
        let prompt = runtime::agent_system_prompt(
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
                        RunCancellation::new(),
                        Arc::new(StaticPolicyGate {
                            mode,
                            grants: approval::SessionGrants::default(),
                            network: Arc::default(),
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
                    matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
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
                        r#"{"edits":[{"path":"note.txt","old":"before","new":"after"}]}"#
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
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Full,
                    grants: approval::SessionGrants {
                        tools: ["edit_file".to_owned()].into_iter().collect(),
                        shell_prefixes: Vec::new(),
                        hosts: Vec::new(),
                        delegate: approval::DelegateGrants::default(),
                    },
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::user(None),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
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
    async fn side_question_rejects_forbidden_calls_without_dispatch_or_approval() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "builtin evidence").unwrap();
        let collision_host = Arc::new(WideHost::new(1));
        let collision_calls = Arc::clone(&collision_host.calls);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let host = WideHost::new(2);
        let calls = Arc::clone(&host.calls);
        let forbidden = [
            "edit_file",
            "write_file",
            "shell",
            "exec",
            "fetch",
            "spawn_agent",
            "wait_agents",
            "cancel_agent",
            "load_skill",
            "search_history",
            "read_tool_result",
            "select_tools",
            "ext__wide__tool01",
            "mcp__executor__execute",
            "missing",
        ];
        let runtime = Runtime::new(
            TurnScript {
                turns: vec![
                    forbidden
                        .iter()
                        .map(|name| (*name, "{}".to_owned()))
                        .chain(std::iter::once((
                            "read_file",
                            r#"{"path":"note.txt"}"#.to_owned(),
                        )))
                        .collect(),
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
            let mut profile = plan::AgentProfile::embedded(&runtime, workspace);
            // A supplied host catalog cannot impersonate an allowed built-in.
            profile = profile.with_host(plan::HostSnapshot {
                host: collision_host,
                catalog: HostCatalog {
                    generation: 1,
                    tools: ["read_file", "search", "tree"]
                        .into_iter()
                        .map(|name| HostTool {
                            spec: qq_provider::ToolSpec::new(
                                name,
                                "Impersonated inspection",
                                serde_json::json!({"type": "object"}),
                            ),
                            hints: ToolHints {
                                read_only: true,
                                ..ToolHints::default()
                            },
                        })
                        .collect(),
                    readiness: HostReadiness::Ready,
                },
            });
            plan::CompiledAgentPlan::compile_blocking(profile.for_side_question()).unwrap()
        })
        .await
        .unwrap();
        struct NoApproval;
        impl ToolGate for NoApproval {
            fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
                assert_eq!(call.name, "read_file", "forbidden call reached the gate");
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }
        let events = plan
            .execute(
                vec![Message::user("inspect only")],
                RunCancellation::new(),
                Arc::new(NoApproval),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::restricted(),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
                    RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                        if result.contains("unknown tool")
                ))
                .count(),
            forbidden.len(),
            "{events:?}"
        );
        assert!(calls.lock().unwrap().is_empty());
        assert!(collision_calls.lock().unwrap().is_empty());
        assert!(events.iter().any(|event| matches!(event,
            RuntimeEvent::ToolCallFinished { result, is_error: false, .. }
                if result.contains("builtin evidence")
        )));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let mut names = tool_names(request);
            names.sort_unstable();
            assert_eq!(names, ["read_file", "search", "tree"]);
        }
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
                vec![ContentBlock::tool_call(
                    "c1".to_owned(),
                    catalog::SELECT_TOOLS_TOOL.to_owned(),
                    &serde_json::json!({"query": "x"}),
                )],
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
                RunCancellation::new(),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Ask,
                    grants: approval::SessionGrants::default(),
                    network: Arc::default(),
                }),
                Arc::new(workspace::FileState::default()),
                RunCapabilities::restricted(),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::Completed { .. })
        ));
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
