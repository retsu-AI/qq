//! Durable non-interactive execution: the `qq run` mode.
//!
//! One validated set of run options goes in; one terminal status comes out.
//! Everything between — session creation, prompt submission, event
//! subscription, unattended approval, budget watching, cancellation, trace
//! writing, and exit-code mapping — stays behind this module. The run drives
//! the same `SessionRuntime` interface (`command`, `snapshot`, `subscribe`)
//! that the TUI and server compose; there is no separate agent path.

use std::{
    future::Future,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    pin::pin,
    process::ExitCode,
    time::Duration,
};

use futures_util::StreamExt;
use qq_core::{SessionRuntime, SessionRuntimeError};
use qq_protocol::{
    ApprovalDecision, ApprovalMode, CommandId, CommandOutcome, CommandReceipt, MessageId,
    MessageRole, ModelSelection, RunActivity, RunId, RunOutcome, RunPromptIdentity,
    SessionAccounting, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    SnapshotRequest, SubscribeRequest, TokenUsage, ToolCallState, WorkspaceId,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

/// How long a cancelled run may take to reach its terminal durable event
/// before the invocation gives up and reports a harness failure.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// How long an `auto` headless run holds an escalated approval open for the
/// configured reviewer before denying it. Covers the reviewer's own 10s
/// request timeout with margin; without a verdict by then the deny proceeds
/// so the run never stalls.
const REVIEWER_DENY_GRACE: Duration = Duration::from_secs(20);

/// Byte budget for one concise tool-activity line in text format.
const MAX_ACTIVITY_BYTES: usize = 160;

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub prompt: String,
    /// Workspace directory; resolved to its canonical form by the store.
    pub workspace: PathBuf,
    pub model: ModelSelection,
    /// Resolved model context limit when configured; unknown stays absent.
    pub context_window: Option<u32>,
    /// Source of the pricing table used for durable accounting.
    pub pricing_provenance: Option<String>,
    pub approval: HeadlessApproval,
    /// Whether the workspace configuration declares a reviewer model. With a
    /// reviewer, `auto` holds an escalated call briefly so the reviewer can
    /// approve it, instead of denying the moment the request is published.
    pub reviewer_configured: bool,
    pub timeout: Option<Duration>,
    pub max_turns: Option<u16>,
    pub max_cost_usd_nanos: Option<u64>,
    pub format: HeadlessFormat,
    pub trace: Option<PathBuf>,
}

/// Unattended approval policies. Interactive `ask` approval is unrepresentable
/// here: a headless run must never wait for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessApproval {
    ReadOnly,
    Auto,
    Full,
}

impl HeadlessApproval {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }

    const fn approval_mode(self) -> ApprovalMode {
        match self {
            Self::ReadOnly => ApprovalMode::ReadOnly,
            Self::Auto => ApprovalMode::Auto,
            Self::Full => ApprovalMode::Full,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessFormat {
    Text,
    Jsonl,
}

/// The terminal status of one headless invocation. Exit codes distinguish
/// success, task/model failure, invalid configuration, timeout or budget
/// exhaustion, harness/persistence failure, and user interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessStatus {
    Completed,
    TaskFailed,
    InvalidConfiguration,
    TimedOut,
    BudgetExhausted,
    Interrupted,
    HarnessFailure,
}

impl HeadlessStatus {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::TaskFailed => 1,
            Self::InvalidConfiguration => 2,
            Self::TimedOut | Self::BudgetExhausted => 3,
            Self::HarnessFailure => 4,
            Self::Interrupted => 130,
        }
    }

    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::TaskFailed => "task_failed",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::TimedOut => "timed_out",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Interrupted => "interrupted",
            Self::HarnessFailure => "harness_failure",
        }
    }
}

/// One line of the JSONL trial stream: metadata, an ordered protocol event,
/// or the single terminal outcome.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TrialRecord<'a> {
    Trial {
        qq_version: &'static str,
        qq_source_revision: &'static str,
        protocol_version: u16,
        workspace_identity: &'a str,
        model: &'a ModelSelection,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pricing_provenance: Option<&'a str>,
        approval: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_turns: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_cost_usd_nanos: Option<u64>,
        workspace_id: String,
        session_id: String,
        run_id: String,
    },
    Event {
        envelope: &'a SessionEventEnvelope,
    },
    Outcome {
        status: &'static str,
        exit_code: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_cost_usd_nanos: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_identity: Option<&'a RunPromptIdentity>,
    },
}

/// Writes trial records to the optional trace file and, in JSONL format, to
/// stdout. Text format still traces when a trace path is given.
struct RecordSink {
    trace: Option<BufWriter<std::fs::File>>,
    to_stdout: bool,
}

impl RecordSink {
    fn open(options: &HeadlessOptions) -> io::Result<Self> {
        let trace = options
            .trace
            .as_deref()
            .map(std::fs::File::create)
            .transpose()?
            .map(BufWriter::new);
        Ok(Self {
            trace,
            to_stdout: options.format == HeadlessFormat::Jsonl,
        })
    }

    fn record(&mut self, stdout: &mut impl Write, record: &TrialRecord<'_>) -> io::Result<()> {
        if self.trace.is_none() && !self.to_stdout {
            return Ok(());
        }
        let line = serde_json::to_string(record).map_err(io::Error::other)?;
        if let Some(trace) = &mut self.trace {
            trace.write_all(line.as_bytes())?;
            trace.write_all(b"\n")?;
        }
        if self.to_stdout {
            stdout.write_all(line.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if let Some(trace) = &mut self.trace {
            trace.flush()?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Failure {
    status: HeadlessStatus,
    message: String,
}

impl Failure {
    fn harness(message: impl Into<String>) -> Self {
        Self {
            status: HeadlessStatus::HarnessFailure,
            message: message.into(),
        }
    }
}

/// Why this invocation asked the runtime to cancel the run.
enum CancelReason {
    Timeout,
    Budget(String),
    Interrupt,
}

#[derive(Clone)]
struct RunHandle {
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    subscribe_after: qq_protocol::EventCursor,
}

struct AcceptedRunGuard {
    cleanup: Option<(SessionRuntime, RunHandle)>,
}

impl AcceptedRunGuard {
    fn new(sessions: SessionRuntime, handle: RunHandle) -> Self {
        Self {
            cleanup: Some((sessions, handle)),
        }
    }

    fn disarm(&mut self) {
        self.cleanup = None;
    }

    async fn settle(&mut self) -> Result<(), Failure> {
        let Some((sessions, handle)) = &self.cleanup else {
            return Ok(());
        };
        let result = cancel_and_settle(sessions, handle).await;
        if result.is_ok() {
            self.disarm();
        }
        result
    }
}

impl Drop for AcceptedRunGuard {
    fn drop(&mut self) {
        let Some((sessions, handle)) = self.cleanup.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            report_detached_cleanup_failure(
                "accepted run cleanup could not start outside a Tokio runtime; \
                 restart recovery must settle it",
            );
            return;
        };
        drop(runtime.spawn(async move {
            if let Err(failure) = cancel_and_settle(&sessions, &handle).await {
                report_detached_cleanup_failure(&format!(
                    "accepted run cleanup failed: {}",
                    failure.message
                ));
            }
        }));
    }
}

fn report_detached_cleanup_failure(message: &str) {
    // Drop has neither the invocation's borrowed writer nor a return channel.
    // Reporting is therefore best-effort; restart recovery remains the
    // durable backstop if stderr is unavailable too.
    let _ = writeln!(io::stderr().lock(), "error: {message}");
}

/// The terminal result of the event-streaming phase.
struct RunEnd {
    status: HeadlessStatus,
    message: Option<String>,
    usage: Option<TokenUsage>,
    estimated_cost_usd_nanos: Option<u64>,
    prompt_identity: Option<Box<RunPromptIdentity>>,
    /// Accumulated text of the last assistant message: the final answer.
    answer: String,
}

impl RunEnd {
    fn failure(failure: Failure) -> Self {
        Self {
            status: failure.status,
            message: Some(failure.message),
            usage: None,
            estimated_cost_usd_nanos: None,
            prompt_identity: None,
            answer: String::new(),
        }
    }
}

/// Runs one headless task to a terminal status. Never panics the process on
/// task problems: every path maps to a distinguishable exit status.
pub async fn run(
    sessions: &SessionRuntime,
    options: HeadlessOptions,
    interrupt: impl Future<Output = ()>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> HeadlessStatus {
    // The trace file opens before any command so a bad path fails the
    // invocation without leaving a half-created session behind.
    let mut sink = match RecordSink::open(&options) {
        Ok(sink) => sink,
        Err(error) => {
            let _ = writeln!(stderr, "error: could not open the trace file: {error}");
            return HeadlessStatus::InvalidConfiguration;
        }
    };

    let handle = match submit(sessions, &options).await {
        Ok(handle) => handle,
        Err(failure) => {
            let _ = writeln!(stderr, "error: {}", failure.message);
            return failure.status;
        }
    };
    let mut accepted = AcceptedRunGuard::new(sessions.clone(), handle.clone());

    let workspace_identity = workspace_identity(&options.workspace);
    let trial = TrialRecord::Trial {
        qq_version: env!("CARGO_PKG_VERSION"),
        qq_source_revision: option_env!("QQ_SOURCE_REVISION").unwrap_or("unknown"),
        protocol_version: qq_protocol::PROTOCOL_VERSION,
        workspace_identity: &workspace_identity,
        model: &options.model,
        context_window: options.context_window,
        pricing_provenance: options.pricing_provenance.as_deref(),
        approval: options.approval.as_str(),
        timeout_seconds: options.timeout.map(|timeout| timeout.as_secs()),
        max_turns: options.max_turns,
        max_cost_usd_nanos: options.max_cost_usd_nanos,
        workspace_id: handle.workspace_id.to_string(),
        session_id: handle.session_id.to_string(),
        run_id: handle.run_id.to_string(),
    };
    if let Err(error) = sink.record(stdout, &trial) {
        let _ = writeln!(stderr, "error: could not write the trial record: {error}");
        if let Err(failure) = accepted.settle().await {
            let _ = writeln!(stderr, "error: {}", failure.message);
        }
        return HeadlessStatus::HarnessFailure;
    }

    let end = match stream_run(
        sessions, &options, &handle, &mut sink, interrupt, stdout, stderr,
    )
    .await
    {
        Ok(end) => {
            accepted.disarm();
            end
        }
        Err(mut failure) => {
            if let Err(cleanup) = accepted.settle().await {
                failure.message.push_str("; cleanup also failed: ");
                failure.message.push_str(&cleanup.message);
            }
            RunEnd::failure(failure)
        }
    };

    let outcome = TrialRecord::Outcome {
        status: end.status.as_str(),
        exit_code: end.status.code(),
        message: end.message.as_deref(),
        usage: end.usage,
        estimated_cost_usd_nanos: end.estimated_cost_usd_nanos,
        prompt_identity: end.prompt_identity.as_deref(),
    };
    if let Err(error) = sink.record(stdout, &outcome).and_then(|()| sink.finish()) {
        let _ = writeln!(stderr, "error: could not write the outcome record: {error}");
        return HeadlessStatus::HarnessFailure;
    }

    if options.format == HeadlessFormat::Text {
        match end.status {
            HeadlessStatus::Completed => {
                let _ = writeln!(stderr);
                let mut answer = end.answer;
                if !answer.ends_with('\n') {
                    answer.push('\n');
                }
                if let Err(error) = stdout
                    .write_all(answer.as_bytes())
                    .and_then(|()| stdout.flush())
                {
                    let _ = writeln!(stderr, "error: could not write the final answer: {error}");
                    return HeadlessStatus::HarnessFailure;
                }
            }
            _ => {
                if let Some(message) = &end.message {
                    let _ = writeln!(stderr, "error: {message}");
                }
            }
        }
    } else if let Some(message) = &end.message {
        let _ = writeln!(stderr, "error: {message}");
    }

    end.status
}

/// Resolves the workspace, creates the session with the model and approval
/// choices, and submits the prompt. Any failure here happens before the model
/// sees the task.
async fn submit(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
) -> Result<RunHandle, Failure> {
    let workspace = options.workspace.display().to_string();
    let resolved = send(
        sessions,
        SessionCommand::ResolveWorkspace { path: workspace },
    )
    .await?;
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        return Err(Failure::harness(
            "workspace resolution returned an unexpected outcome",
        ));
    };

    let created = send(
        sessions,
        SessionCommand::CreateSession {
            workspace_id,
            parent_id: None,
            model: options.model.clone(),
            approval_mode: options.approval.approval_mode(),
        },
    )
    .await?;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        return Err(Failure::harness(
            "session creation returned an unexpected outcome",
        ));
    };

    let queued = send(
        sessions,
        SessionCommand::SubmitPrompt {
            session_id,
            prompt: options.prompt.clone(),
        },
    )
    .await?;
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        return Err(Failure::harness(
            "prompt submission returned an unexpected outcome",
        ));
    };

    Ok(RunHandle {
        workspace_id,
        session_id,
        run_id,
        // Subscribing from the session-creation cursor replays the queued
        // prompt and everything after it, so no event is lost to the gap
        // between submission and subscription.
        subscribe_after: created.committed_through,
    })
}

/// Streams durable events to completion, answering approvals, enforcing the
/// timeout and budgets through the ordinary idempotent cancellation command,
/// and rendering output per the selected format.
async fn stream_run(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    handle: &RunHandle,
    sink: &mut RecordSink,
    interrupt: impl Future<Output = ()>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<RunEnd, Failure> {
    let mut events = sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .map_err(|error| Failure {
            status: status_for_error(&error),
            message: format!("could not subscribe to session events: {error}"),
        })?;

    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let mut interrupt = pin!(interrupt);
    let mut interrupt_armed = true;
    let mut cancel: Option<CancelReason> = None;
    let mut shutdown_at: Option<Instant> = None;

    // The final answer is the text of the last assistant message that starts
    // streaming; earlier turns are progress, not the answer.
    let mut answer = String::new();
    let mut answer_message: Option<MessageId> = None;
    let mut observed_turns = 0_u16;
    let mut child_sessions = Vec::new();
    let text = options.format == HeadlessFormat::Text;

    loop {
        tokio::select! {
            biased;
            () = interrupt.as_mut(), if interrupt_armed => {
                interrupt_armed = false;
                if cancel.is_none() {
                    request_cancel(sessions, handle.run_id).await?;
                    cancel = Some(CancelReason::Interrupt);
                    shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
                    if text {
                        let _ = writeln!(stderr, "[run] interrupt received; cancelling");
                    }
                }
            }
            () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                if deadline.is_some() && cancel.is_none() => {
                request_cancel(sessions, handle.run_id).await?;
                cancel = Some(CancelReason::Timeout);
                shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
                if text {
                    let _ = writeln!(stderr, "[run] timeout reached; cancelling");
                }
            }
            () = tokio::time::sleep_until(shutdown_at.unwrap_or_else(Instant::now)),
                if shutdown_at.is_some() => {
                return Err(Failure::harness(
                    "the cancelled run did not reach a terminal durable event \
                     within the shutdown period",
                ));
            }
            event = events.next() => {
                let envelope = match event {
                    Some(Ok(envelope)) => envelope,
                    Some(Err(error)) => {
                        return Err(Failure {
                            status: status_for_error(&error),
                            message: format!("the event stream failed: {error}"),
                        });
                    }
                    None => {
                        return Err(Failure::harness(
                            "the event stream ended without a terminal run event",
                        ));
                    }
                };
                sink.record(stdout, &TrialRecord::Event { envelope: &envelope })
                    .map_err(|error| {
                        Failure::harness(format!("could not write an event record: {error}"))
                    })?;

                let ours = envelope.session_id == handle.session_id;
                if let SessionEvent::SessionCreated { session } = &envelope.event
                    && session.parent_id == Some(handle.session_id)
                    && !child_sessions.contains(&session.id)
                {
                    child_sessions.push(session.id);
                }
                let related = ours || child_sessions.contains(&envelope.session_id);
                let mut over_turn_budget = false;
                let mut check_cost = false;
                match &envelope.event {
                    SessionEvent::RunActivityChanged {
                        run_id,
                        activity: RunActivity::WaitingForProvider,
                    } if *run_id == handle.run_id => {
                        observed_turns = observed_turns.saturating_add(1);
                        // Every waiting boundary after the first follows a
                        // completed turn, including a call-free internal
                        // checkpoint whose provider omitted usage.
                        check_cost = observed_turns > 1;
                        over_turn_budget = options
                            .max_turns
                            .is_some_and(|limit| observed_turns > limit);
                    }
                    SessionEvent::RunActivityChanged {
                        activity: RunActivity::WaitingForProvider,
                        ..
                    } if related => {
                        // Child turns contribute to the selected session's
                        // inclusive hard-cost budget even though their turn
                        // count does not consume the parent's turn budget.
                        check_cost = true;
                    }
                    SessionEvent::AssistantMessageStarted { message } if ours => {
                        if message.role == MessageRole::Assistant {
                            answer_message = Some(message.id);
                            answer.clear();
                            answer.push_str(&message.output);
                        }
                        over_turn_budget = beyond_turn_budget(options, message.turn_ordinal);
                    }
                    SessionEvent::TextAppended { message_id, text: chunk, .. } if ours => {
                        if Some(*message_id) == answer_message {
                            answer.push_str(chunk);
                            if text {
                                let _ = stderr.write_all(chunk.as_bytes());
                                let _ = stderr.flush();
                            }
                        }
                    }
                    SessionEvent::ToolCallStarted { tool_call } if ours => {
                        if text {
                            let _ = writeln!(
                                stderr,
                                "[tool] {} {}",
                                tool_call.name,
                                concise(&tool_call.arguments),
                            );
                        }
                    }
                    SessionEvent::ToolCallFinished { tool_call } if ours => {
                        if text {
                            let verdict = match tool_call.state {
                                ToolCallState::Completed => "ok",
                                ToolCallState::Denied => "denied",
                                _ => "failed",
                            };
                            let _ = writeln!(stderr, "[tool] {} {verdict}", tool_call.name);
                        }
                    }
                    SessionEvent::ToolCallRequested { tool_call } if related => {
                        if ours {
                            over_turn_budget = beyond_turn_budget(options, tool_call.turn_ordinal);
                        }
                        // Tool requests commit atomically with the turn's
                        // cumulative accounting, including an explicit
                        // unknown value when provider usage was absent. Child
                        // accounting is included in the parent snapshot.
                        check_cost = true;
                    }
                    SessionEvent::ToolApprovalRequested { tool_call, .. } => {
                        // The headless invocation is the approval client.
                        // Full approves everything unattended; auto denies
                        // whatever the policy escalated (dangerous shell) so
                        // the run never stalls waiting for a human — but
                        // when a reviewer model is configured the deny is
                        // deferred briefly, giving the reviewer its window.
                        // A late deny is harmless: resolution is idempotent,
                        // so a reviewer approval that landed first stands.
                        let decision = match options.approval {
                            HeadlessApproval::Full => Some(ApprovalDecision::ApproveOnce),
                            HeadlessApproval::Auto if options.reviewer_configured => {
                                if let Some(run_id) = envelope.run_id {
                                    let sessions = sessions.clone();
                                    let tool_call_id = tool_call.id;
                                    tokio::spawn(async move {
                                        tokio::time::sleep(REVIEWER_DENY_GRACE).await;
                                        let _ = send(
                                            &sessions,
                                            SessionCommand::RespondToolApproval {
                                                run_id,
                                                tool_call_id,
                                                decision: ApprovalDecision::Deny,
                                            },
                                        )
                                        .await;
                                    });
                                }
                                None
                            }
                            HeadlessApproval::Auto => Some(ApprovalDecision::Deny),
                            HeadlessApproval::ReadOnly => Some(ApprovalDecision::Deny),
                        };
                        if let (Some(run_id), Some(decision)) = (envelope.run_id, decision) {
                            respond_approval(sessions, run_id, tool_call.id, decision, stderr)
                                .await;
                        }
                    }
                    SessionEvent::RunContextUpdated { .. } if related => {
                        check_cost = true;
                    }
                    SessionEvent::SessionContextUpdated { .. }
                    | SessionEvent::SessionUpdated { .. }
                        if related => {
                        check_cost = true;
                    }
                    SessionEvent::RunFinished { session, run_id, outcome, usage, .. }
                        if *run_id == handle.run_id => {
                        let usage = inclusive_usage(session.accounting, *usage);
                        let cost = inclusive_cost(
                            session.accounting,
                            session.estimated_cost_usd_nanos,
                        );
                        let (status, message) = if matches!(outcome, RunOutcome::Completed)
                            && let Some(limit) = options.max_cost_usd_nanos
                            && let Some(message) = cost_budget_violation(limit, cost)
                        {
                            (HeadlessStatus::BudgetExhausted, Some(message))
                        } else {
                            settle(outcome, cancel.as_ref())
                        };
                        let prompt_identity = run_prompt_identity(sessions, handle).await?;
                        return Ok(RunEnd {
                            status,
                            message,
                            usage,
                            estimated_cost_usd_nanos: cost,
                            prompt_identity,
                            answer,
                        });
                    }
                    SessionEvent::RunFinished { .. } if related => {
                        check_cost = true;
                    }
                    _ => {}
                }

                if over_turn_budget && cancel.is_none() {
                    let limit = options.max_turns.unwrap_or_default();
                    request_cancel(sessions, handle.run_id).await?;
                    cancel = Some(CancelReason::Budget(format!(
                        "the run exceeded its {limit} model turn budget"
                    )));
                    shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
                    if text {
                        let _ = writeln!(stderr, "[run] model turn budget exhausted; cancelling");
                    }
                }
                if check_cost && cancel.is_none() && let Some(limit) = options.max_cost_usd_nanos {
                    let cost = session_cost(sessions, handle).await?;
                    if let Some(message) = cost_budget_violation(limit, cost) {
                        request_cancel(sessions, handle.run_id).await?;
                        cancel = Some(CancelReason::Budget(message));
                        shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
                        if text {
                            let _ = writeln!(stderr, "[run] cost budget exhausted; cancelling");
                        }
                    }
                }
            }
        }
    }
}

fn beyond_turn_budget(options: &HeadlessOptions, turn_ordinal: u16) -> bool {
    options.max_turns.is_some_and(|limit| turn_ordinal > limit)
}

fn cost_budget_violation(limit: u64, cost: Option<u64>) -> Option<String> {
    match cost {
        Some(cost) if cost > limit => Some(format!(
            "the run's estimated cost exceeded its budget \
             ({cost} > {limit} USD nanos)"
        )),
        Some(_) => None,
        None => Some(
            "the run's cost became unknown, so its hard cost budget could not be enforced"
                .to_owned(),
        ),
    }
}

fn inclusive_cost(
    accounting: Option<SessionAccounting>,
    legacy_direct_cost: Option<u64>,
) -> Option<u64> {
    match accounting {
        Some(accounting) => accounting.inclusive.estimated_cost_usd_nanos,
        None => legacy_direct_cost,
    }
}

fn inclusive_usage(
    accounting: Option<SessionAccounting>,
    legacy_direct_usage: Option<TokenUsage>,
) -> Option<TokenUsage> {
    match accounting {
        Some(accounting) => accounting.inclusive.usage,
        None => legacy_direct_usage,
    }
}

fn workspace_identity(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let mut identity = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(identity, "{byte:02x}").expect("writing to a String cannot fail");
    }
    identity
}

async fn run_prompt_identity(
    sessions: &SessionRuntime,
    handle: &RunHandle,
) -> Result<Option<Box<RunPromptIdentity>>, Failure> {
    let snapshot = sessions
        .snapshot(SnapshotRequest {
            workspace_id: handle.workspace_id,
            focused_session_id: Some(handle.session_id),
            session_limit: 1,
            message_limit: 1,
        })
        .await
        .map_err(|error| Failure {
            status: status_for_error(&error),
            message: format!("could not read the terminal run identity: {error}"),
        })?;
    snapshot
        .focused
        .and_then(|session| session.runs.into_iter().find(|run| run.id == handle.run_id))
        .map(|run| run.prompt_identity)
        .ok_or_else(|| Failure::harness("the terminal run is missing from its session snapshot"))
}

/// Maps the run's durable outcome plus this invocation's cancellation intent
/// to a terminal status.
fn settle(outcome: &RunOutcome, cancel: Option<&CancelReason>) -> (HeadlessStatus, Option<String>) {
    match outcome {
        RunOutcome::Completed => (HeadlessStatus::Completed, None),
        RunOutcome::Failed { failure } => {
            let status = match failure.kind {
                qq_protocol::RunFailureKind::Server => HeadlessStatus::HarnessFailure,
                _ => HeadlessStatus::TaskFailed,
            };
            (status, Some(failure.message.clone()))
        }
        RunOutcome::Cancelled => match cancel {
            Some(CancelReason::Timeout) => (
                HeadlessStatus::TimedOut,
                Some("the run was cancelled after its timeout elapsed".to_owned()),
            ),
            Some(CancelReason::Budget(message)) => {
                (HeadlessStatus::BudgetExhausted, Some(message.clone()))
            }
            Some(CancelReason::Interrupt) => (
                HeadlessStatus::Interrupted,
                Some("the run was cancelled by an interrupt".to_owned()),
            ),
            None => (
                HeadlessStatus::HarnessFailure,
                Some("the run was cancelled outside this invocation".to_owned()),
            ),
        },
        RunOutcome::Interrupted => (
            HeadlessStatus::HarnessFailure,
            Some("the run was interrupted before reaching a terminal outcome".to_owned()),
        ),
    }
}

/// The session's inclusive estimated cost, including durable in-progress run
/// accounting, read through the ordinary snapshot operation. `None` means the
/// total is unknown, never zero.
async fn session_cost(
    sessions: &SessionRuntime,
    handle: &RunHandle,
) -> Result<Option<u64>, Failure> {
    let snapshot = sessions
        .snapshot(SnapshotRequest {
            workspace_id: handle.workspace_id,
            focused_session_id: Some(handle.session_id),
            session_limit: 1,
            message_limit: 1,
        })
        .await
        .map_err(|error| Failure {
            status: status_for_error(&error),
            message: format!("could not read the session snapshot: {error}"),
        })?;
    Ok(snapshot.focused.and_then(|session| {
        inclusive_cost(
            session.summary.accounting,
            session.summary.estimated_cost_usd_nanos,
        )
    }))
}

/// Sends the ordinary idempotent cancellation command. A run that already
/// finished is success: the terminal event is on its way or already replayed.
async fn request_cancel(sessions: &SessionRuntime, run_id: RunId) -> Result<(), Failure> {
    let receipt = send(sessions, SessionCommand::CancelRun { run_id }).await?;
    match receipt.outcome {
        CommandOutcome::CancellationRequested { .. }
        | CommandOutcome::RunAlreadyFinished { .. } => Ok(()),
        _ => Err(Failure::harness(
            "cancellation returned an unexpected outcome",
        )),
    }
}

/// Retains ownership after a post-submit harness failure: request ordinary
/// durable cancellation, then wait for the matching terminal event before the
/// invocation returns. Restart recovery remains the backstop if persistence
/// itself is unavailable.
async fn cancel_and_settle(sessions: &SessionRuntime, handle: &RunHandle) -> Result<(), Failure> {
    request_cancel(sessions, handle.run_id).await?;
    let mut events = sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .map_err(|error| Failure {
            status: status_for_error(&error),
            message: format!("could not observe cancellation settlement: {error}"),
        })?;
    tokio::time::timeout(SHUTDOWN_GRACE, async {
        loop {
            match events.next().await {
                Some(Ok(SessionEventEnvelope {
                    event: SessionEvent::RunFinished { run_id, .. },
                    ..
                })) if run_id == handle.run_id => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Err(Failure {
                        status: status_for_error(&error),
                        message: format!("cancellation settlement stream failed: {error}"),
                    });
                }
                None => {
                    return Err(Failure::harness(
                        "cancellation settlement stream ended before the terminal event",
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| Failure::harness("accepted run did not settle within the shutdown period"))?
}

/// Answers one pending tool approval. Failures are reported but never fatal:
/// an unanswerable approval resolves by the runtime's own deny-by-timeout,
/// so the run still cannot stall forever.
async fn respond_approval(
    sessions: &SessionRuntime,
    run_id: RunId,
    tool_call_id: qq_protocol::ToolCallId,
    decision: ApprovalDecision,
    stderr: &mut impl Write,
) {
    let responded = send(
        sessions,
        SessionCommand::RespondToolApproval {
            run_id,
            tool_call_id,
            decision,
        },
    )
    .await;
    if let Err(failure) = responded {
        let _ = writeln!(
            stderr,
            "warning: could not resolve a tool approval: {}",
            failure.message
        );
    }
}

async fn send(
    sessions: &SessionRuntime,
    command: SessionCommand,
) -> Result<CommandReceipt, Failure> {
    let command_id = CommandId::generate().map_err(|error| {
        Failure::harness(format!("could not generate a command identifier: {error}"))
    })?;
    sessions
        .command(command_id, command)
        .await
        .map_err(|error| Failure {
            status: status_for_error(&error),
            message: error.to_string(),
        })
}

/// Distinguishes caller mistakes (invalid configuration) from harness and
/// persistence problems.
const fn status_for_error(error: &SessionRuntimeError) -> HeadlessStatus {
    match error {
        SessionRuntimeError::EmptyWorkspace
        | SessionRuntimeError::InvalidWorkspace
        | SessionRuntimeError::EmptyPrompt
        | SessionRuntimeError::PromptTooLarge
        | SessionRuntimeError::InvalidModelSelection => HeadlessStatus::InvalidConfiguration,
        _ => HeadlessStatus::HarnessFailure,
    }
}

/// One bounded single-line summary of tool arguments for text output.
fn concise(arguments: &str) -> String {
    let mut summary = String::with_capacity(arguments.len().min(MAX_ACTIVITY_BYTES + 1));
    for character in arguments.chars() {
        if summary.len() + character.len_utf8() > MAX_ACTIVITY_BYTES {
            summary.push('…');
            break;
        }
        summary.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    summary
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{Arc, Mutex},
    };

    use futures_util::stream;
    use qq_core::{
        LoadedRuntime, Runtime, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest,
        RuntimeLoader, SessionRuntimeOptions,
    };
    use qq_protocol::{AccountingTotal, ModelPricing, RunStatus, SessionStatus, WorkspaceSnapshot};
    use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream, ProviderUsage};

    use super::*;

    /// Builds a fresh provider per claimed run, mirroring how the real
    /// loader compiles a runtime per run.
    struct ProviderLoader<F>(F);

    impl<P, F> RuntimeLoader for ProviderLoader<F>
    where
        P: Provider + 'static,
        F: Fn() -> P + Send + Sync + 'static,
    {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = (self.0)();
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: None,
                            cache_write_usd_nanos_per_token: None,
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: qq_protocol::RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ParentChildLoader {
        parent: Arc<dyn Provider>,
        child: Arc<dyn Provider>,
    }

    impl RuntimeLoader for ParentChildLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = if request.model.model.as_deref() == Some("test/child") {
                Arc::clone(&self.child)
            } else {
                Arc::clone(&self.parent)
            };
            Box::pin(async move {
                Runtime::with_provider(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(
                            runtime.with_spawn_model_routes(vec!["test/child".to_owned()]),
                        ),
                        pricing: Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: None,
                            cache_write_usd_nanos_per_token: None,
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: qq_protocol::RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    /// Streams "hello" as two deltas and completes with usage.
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

    /// Turn one requests a workspace write and a shell command; turn two
    /// completes with a final answer.
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
                        json: r#"{"command":"printf ok > shelled.txt"}"#.to_owned(),
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

    /// Never produces an event; only cancellation can end its run.
    struct HangingProvider;

    impl Provider for HangingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::pending())
        }
    }

    /// Requests one read per turn forever, so only a budget can stop it.
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
            if request.tools().is_empty() {
                state.1 = true;
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "slice checkpoint".to_owned(),
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

    /// Completes two tool turns, then stalls before producing content on the
    /// third. A turn budget must observe the provider request boundary rather
    /// than waiting for text or a tool call that may never arrive.
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
        let workspace = directory.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(&workspace).unwrap();
        let sessions = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
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

    fn options(workspace: &Path) -> HeadlessOptions {
        HeadlessOptions {
            prompt: "do the task".to_owned(),
            workspace: workspace.to_owned(),
            model: ModelSelection {
                model: Some("test/model".to_owned()),
                max_output_tokens: Some(256),
                organization: None,
            },
            context_window: Some(128_000),
            pricing_provenance: Some("test fixture".to_owned()),
            approval: HeadlessApproval::ReadOnly,
            reviewer_configured: false,
            timeout: None,
            max_turns: None,
            max_cost_usd_nanos: None,
            format: HeadlessFormat::Jsonl,
            trace: None,
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
            .map(|line| serde_json::from_str(line).expect("every stdout line must be JSON"))
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
                session_limit: 16,
                message_limit: 16,
            })
            .await
            .unwrap()
    }

    /// The core Phase 1 guarantee: cancellation and timeout must never leave
    /// an active run behind.
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
                        RunStatus::Completed | RunStatus::Cancelled | RunStatus::Failed
                    ),
                    "run {run:?} must be terminal"
                );
            }
        }
    }

    #[tokio::test]
    async fn auto_mode_executes_write_and_shell_calls_through_the_session_runtime() {
        let fixture = fixture(MutatingProvider::new).await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        // The tool calls really executed inside the workspace.
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("note.txt")).unwrap(),
            "hello from qq\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("shelled.txt")).unwrap(),
            "ok"
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
        // Under auto, edits and safe shell run directly: no approval
        // round-trip happened and nothing waited for a human.
        assert!(
            event_records(&records)
                .iter()
                .all(|record| { record["envelope"]["event"]["type"] != "tool_approval_requested" })
        );
    }

    #[tokio::test]
    async fn read_only_mode_denies_mutations_without_stalling() {
        let fixture = fixture(MutatingProvider::new).await;
        let options = options(&fixture.workspace);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        // The run completes (denials are tool errors the model can react
        // to), but nothing was written and nothing waited for approval.
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
    async fn jsonl_records_have_monotonic_cursors_and_exactly_one_terminal_outcome() {
        let fixture = fixture(|| TextProvider).await;
        let options = options(&fixture.workspace);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert_eq!(records[0]["type"], "trial", "metadata must lead the trial");
        assert_eq!(records[0]["model"]["model"], "test/model");
        assert_eq!(records[0]["approval"], "read-only");
        assert!(records[0].get("workspace").is_none());
        assert_eq!(records[0]["workspace_identity"].as_str().unwrap().len(), 64);
        assert_eq!(records[0]["context_window"], 128_000);
        assert_eq!(records[0]["pricing_provenance"], "test fixture");
        assert!(records[0]["qq_source_revision"].is_string());

        let mut previous = None;
        for record in event_records(&records) {
            let envelope: SessionEventEnvelope = serde_json::from_value(record["envelope"].clone())
                .expect("event records must decode as protocol envelopes");
            if let Some(previous) = previous {
                assert!(
                    envelope.cursor.sequence > previous,
                    "cursors must be strictly monotonic"
                );
            }
            previous = Some(envelope.cursor.sequence);
        }
        assert!(previous.is_some(), "the trial must contain events");

        let outcomes: Vec<_> = records
            .iter()
            .filter(|record| record["type"] == "outcome")
            .collect();
        assert_eq!(outcomes.len(), 1, "exactly one terminal outcome");
        assert_eq!(outcomes[0]["status"], "completed");
        assert_eq!(outcomes[0]["exit_code"], 0);
        assert_eq!(outcomes[0]["prompt_identity"]["version"], 7);
        assert!(outcomes[0]["prompt_identity"]["system_prompt_hash"].is_string());
        assert!(outcomes[0]["prompt_identity"]["tool_schema_hash"].is_string());
        assert_eq!(
            records.last().unwrap()["type"],
            "outcome",
            "the outcome must be the final record"
        );
    }

    #[tokio::test]
    async fn internal_slice_rollover_is_not_a_headless_terminal_outcome() {
        let fixture = fixture(CompletesAfterInternalSlice::new).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let options = options(&fixture.workspace);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["type"] == "outcome")
                .count(),
            1
        );
        assert_eq!(
            event_records(&records)
                .iter()
                .filter(|record| record["envelope"]["event"]["type"] == "run_finished")
                .count(),
            1
        );
        assert_eq!(finished_tool_calls(&records).len(), 256);
        assert!(event_records(&records).iter().any(|record| {
            record["envelope"]["event"]["type"] == "text_appended"
                && record["envelope"]["event"]["text"] == "slice checkpoint"
        }));
        assert!(event_records(&records).iter().any(|record| {
            record["envelope"]["event"]["type"] == "text_appended"
                && record["envelope"]["event"]["text"] == "task complete"
        }));
    }

    #[tokio::test]
    async fn text_format_streams_progress_to_stderr_and_answers_on_stdout() {
        let fixture = fixture(|| TextProvider).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        assert_eq!(stdout, "hello\n", "stdout carries only the final answer");
        assert!(stderr.contains("hello"), "stderr streams the progress");
    }

    #[tokio::test]
    async fn timeout_sends_cancellation_and_leaves_no_active_run() {
        let fixture = fixture(|| HangingProvider).await;
        let mut options = options(&fixture.workspace);
        options.timeout = Some(Duration::from_millis(100));

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::TimedOut);
        assert_eq!(status.code(), 3);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["type"], "outcome");
        assert_eq!(outcome["status"], "timed_out");
        assert_eq!(outcome["exit_code"], 3);
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn output_failure_after_prompt_submission_cancels_and_settles_the_run() {
        let fixture = fixture(|| HangingProvider).await;
        let options = options(&fixture.workspace);
        let mut stdout = BrokenWriter;
        let mut stderr = Vec::new();

        let status = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &fixture.sessions,
                options,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .expect("an output failure must not leave the accepted run detached");

        assert_eq!(status, HeadlessStatus::HarnessFailure);
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn event_output_failure_cancels_and_settles_the_run() {
        let fixture = fixture(|| HangingProvider).await;
        let options = options(&fixture.workspace);
        let mut stdout = BreaksAfterFlush { broken: false };
        let mut stderr = Vec::new();

        let status = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &fixture.sessions,
                options,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .expect("an event-output failure must retain ownership through settlement");

        assert_eq!(status, HeadlessStatus::HarnessFailure);
        assert_no_active_run(&fixture).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn trace_failure_after_prompt_submission_cancels_and_settles_the_run() {
        let fixture = fixture(|| HangingProvider).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;
        options.prompt = "x".repeat(16 * 1024);
        options.trace = Some(PathBuf::from("/dev/full"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &fixture.sessions,
                options,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .expect("a trace failure must retain ownership through settlement");

        assert_eq!(status, HeadlessStatus::HarnessFailure);
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn final_text_output_failure_is_a_harness_failure() {
        let fixture = fixture(|| TextProvider).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;
        let mut stdout = BrokenWriter;
        let mut stderr = Vec::new();

        let status = run(
            &fixture.sessions,
            options,
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        )
        .await;

        assert_eq!(status, HeadlessStatus::HarnessFailure);
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn aborting_the_headless_owner_still_settles_its_accepted_run() {
        let fixture = fixture(|| HangingProvider).await;
        let options = options(&fixture.workspace);
        let sessions = fixture.sessions.clone();
        let owner = tokio::spawn(async move {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run(
                &sessions,
                options,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = workspace_snapshot(&fixture).await;
                if snapshot
                    .sessions
                    .iter()
                    .any(|session| session.active_run_id.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the headless owner must accept a run before it is aborted");
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = workspace_snapshot(&fixture).await;
                if snapshot
                    .sessions
                    .iter()
                    .all(|session| session.active_run_id.is_none())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the detached accepted run must settle after owner abort");
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn interrupt_sends_cancellation_and_leaves_no_active_run() {
        let fixture = fixture(|| HangingProvider).await;
        let options = options(&fixture.workspace);
        let interrupt = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        let (status, stdout, _stderr) = run_to_end(&fixture, options, interrupt).await;

        assert_eq!(status, HeadlessStatus::Interrupted);
        assert_eq!(status.code(), 130);
        let records = parse_records(&stdout);
        assert_eq!(records.last().unwrap()["status"], "interrupted");
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn turn_budget_cancels_a_looping_run_as_budget_exhaustion() {
        let fixture = fixture(ReadLoopProvider::new).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let mut options = options(&fixture.workspace);
        options.max_turns = Some(2);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(
            status,
            HeadlessStatus::BudgetExhausted,
            "stdout: {stdout}\nstderr: {_stderr}"
        );
        assert_eq!(status.code(), 3);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted");
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn cost_budget_cancels_a_looping_run_as_budget_exhaustion() {
        let fixture = fixture(ReadLoopProvider::new).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let mut options = options(&fixture.workspace);
        options.max_cost_usd_nanos = Some(4_000);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(
            status,
            HeadlessStatus::BudgetExhausted,
            "stdout: {stdout}\nstderr: {_stderr}"
        );
        assert_eq!(status.code(), 3);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted");
        assert!(
            outcome["estimated_cost_usd_nanos"]
                .as_u64()
                .is_some_and(|cost| cost >= 6_000)
        );
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn hard_cost_budget_cancels_when_provider_usage_becomes_unknown() {
        let fixture = fixture(ReadLoopProvider::unmetered).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let mut options = options(&fixture.workspace);
        options.max_cost_usd_nanos = Some(4_000);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::BudgetExhausted);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted");
        assert!(
            outcome["message"]
                .as_str()
                .is_some_and(|message| message.contains("cost became unknown"))
        );
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn hard_cost_budget_cancels_an_unmetered_looping_child() {
        let parent: Arc<dyn Provider> = Arc::new(SpawnsLoopingChildProvider::new());
        let child: Arc<dyn Provider> = Arc::new(ReadLoopProvider::unmetered());
        let fixture = fixture_with_loader(Arc::new(ParentChildLoader { parent, child })).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let mut options = options(&fixture.workspace);
        options.max_cost_usd_nanos = Some(4_000);

        let (status, stdout, _stderr) = tokio::time::timeout(
            Duration::from_secs(2),
            run_to_end(&fixture, options, std::future::pending()),
        )
        .await
        .expect("the child turn must trigger its parent's inclusive cost budget");

        assert_eq!(status, HeadlessStatus::BudgetExhausted);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted");
        assert!(
            outcome["message"]
                .as_str()
                .is_some_and(|message| message.contains("cost became unknown"))
        );
        assert!(event_records(&records).iter().any(|record| {
            record["envelope"]["event"]["type"] == "session_created"
                && record["envelope"]["event"]["session"]["parent_id"].is_string()
        }));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = workspace_snapshot(&fixture).await;
                if snapshot.sessions.iter().all(|session| {
                    session.status == SessionStatus::Idle && session.active_run_id.is_none()
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("parent cancellation must settle its in-flight child");
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn completed_unmetered_turn_still_reports_cost_budget_exhaustion() {
        let fixture = fixture(|| UnmeteredTextProvider).await;
        let mut options = options(&fixture.workspace);
        options.max_cost_usd_nanos = Some(4_000);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::BudgetExhausted);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted");
        assert!(
            outcome["message"]
                .as_str()
                .is_some_and(|message| message.contains("cost became unknown"))
        );
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn completed_over_cost_turn_still_reports_cost_budget_exhaustion() {
        let fixture = fixture(|| TextProvider).await;
        let mut options = options(&fixture.workspace);
        options.max_cost_usd_nanos = Some(10_000);

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::BudgetExhausted);
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted");
        assert!(
            outcome["estimated_cost_usd_nanos"]
                .as_u64()
                .is_some_and(|cost| cost > 10_000)
        );
        assert_no_active_run(&fixture).await;
    }

    #[tokio::test]
    async fn turn_budget_cancels_before_a_silent_over_budget_turn_can_hang() {
        let fixture = fixture(ReadsThenHangsProvider::new).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let mut options = options(&fixture.workspace);
        options.max_turns = Some(2);

        let (status, stdout, _stderr) = tokio::time::timeout(
            Duration::from_secs(1),
            run_to_end(&fixture, options, std::future::pending()),
        )
        .await
        .expect("the turn budget must cancel before the silent third turn stalls");

        assert_eq!(status, HeadlessStatus::BudgetExhausted);
        let records = parse_records(&stdout);
        assert_eq!(records.last().unwrap()["status"], "budget_exhausted");
        assert_no_active_run(&fixture).await;
    }

    #[test]
    fn inclusive_cost_never_substitutes_a_direct_alias_for_unknown_child_cost() {
        let accounting = SessionAccounting {
            direct: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: Some(5),
            },
            inclusive: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            },
        };

        assert_eq!(inclusive_cost(Some(accounting), Some(5)), None);
        assert_eq!(inclusive_cost(None, Some(5)), Some(5));
    }

    #[test]
    fn inclusive_usage_counts_children_and_preserves_unknown_totals() {
        let direct = TokenUsage {
            input_tokens: 10,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 3,
        };
        let inclusive = TokenUsage {
            input_tokens: 30,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 4,
            output_tokens: 9,
        };
        let accounting = SessionAccounting {
            direct: AccountingTotal {
                usage: Some(direct),
                estimated_cost_usd_nanos: Some(5),
            },
            inclusive: AccountingTotal {
                usage: Some(inclusive),
                estimated_cost_usd_nanos: Some(15),
            },
        };
        assert_eq!(
            inclusive_usage(Some(accounting), Some(direct)),
            Some(inclusive)
        );

        let unknown_children = SessionAccounting {
            inclusive: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            },
            ..accounting
        };
        assert_eq!(inclusive_usage(Some(unknown_children), Some(direct)), None);
        assert_eq!(inclusive_usage(None, Some(direct)), Some(direct));
    }
}
