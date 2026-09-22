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
    ApprovalDecision, ApprovalGrant, ApprovalMode, BudgetLimitKind, CommandId, CommandOutcome,
    CommandReceipt, ContentHash, HeadlessOutcome, HeadlessRecordRef, HeadlessTrial, InputPart,
    MessageId, MessageRole, ModelSelection, RunId, RunLimits, RunOutcome, RunPromptIdentity,
    SessionAccounting, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    ShellCommandPreview, SnapshotRequest, SubscribeRequest, TokenUsage, ToolCallState, WorkspaceId,
};
pub use qq_protocol::{HeadlessApproval, HeadlessStatus};
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
/// Steering lines buffered between stdin and the run. Beyond this the reader
/// waits; the runtime's own per-run pending bound refuses the rest anyway.
pub const MAX_PENDING_STEER_LINES: usize = 8;

/// Byte budget for one concise tool-activity line in text format.
const MAX_ACTIVITY_BYTES: usize = 160;

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub prompt: String,
    /// Workspace directory; resolved to its canonical form by the store.
    pub workspace: PathBuf,
    /// An existing idle root session of the workspace to submit into. `None`
    /// creates a fresh session.
    pub session: Option<SessionId>,
    pub model: ModelSelection,
    /// Agent profile the session runs as; validated against the workspace
    /// configuration before the run starts.
    pub profile: qq_protocol::AgentProfileId,
    /// Resolved model context limit when configured; unknown stays absent.
    pub context_window: Option<u32>,
    /// Source of the pricing table used for durable accounting.
    pub pricing_provenance: Option<String>,
    pub approval: HeadlessApproval,
    /// Whether the workspace configuration declares a reviewer model. With a
    /// reviewer, `auto` holds an escalated call briefly so the reviewer can
    /// approve it, instead of denying the moment the request is published.
    pub reviewer_configured: bool,
    /// Tools whose held calls are approved for the session on first request.
    pub allow_tools: Vec<String>,
    /// Shell prefixes (word-boundary, as the policy matches them) whose held
    /// commands are approved for the session on first request.
    pub allow_shell_prefixes: Vec<String>,
    /// Hosts (exact or `*.suffix`) whose held `fetch` calls are approved for
    /// the session on first request.
    pub allow_hosts: Vec<String>,
    pub timeout: Option<Duration>,
    pub max_turns: Option<u32>,
    pub max_cost_usd_nanos: Option<u64>,
    /// Opaque labels stamped on the session and the run; echoed on the trial
    /// record and every session snapshot. Never interpreted.
    pub correlation: qq_protocol::Correlation,
    /// The typed-output contract, already compiled once by the caller so an
    /// unenforceable schema was refused before the runtime opened.
    pub output: Option<Box<qq_protocol::OutputContract>>,
    pub format: HeadlessFormat,
    pub trace: Option<PathBuf>,
    /// Print the resume hint (session id and the command that continues it)
    /// to stderr after the outcome. Set only for text output to a terminal:
    /// JSONL consumers read `session_id` from the trial record, and scripts
    /// capturing stderr get nothing they did not ask for.
    pub resume_hint: bool,
    /// An evaluation arm label (`QQ_EVAL_ARM`) stamped on the trial record so
    /// paired comparisons can tell configurations apart without inferring
    /// them from prompt or schema hashes. Never affects behavior.
    pub arm: Option<String>,
}

/// The session approval mode a headless policy submits with.
const fn approval_mode(approval: HeadlessApproval) -> ApprovalMode {
    match approval {
        HeadlessApproval::ReadOnly => ApprovalMode::ReadOnly,
        HeadlessApproval::Auto => ApprovalMode::Auto,
        HeadlessApproval::Full => ApprovalMode::Full,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessFormat {
    Text,
    Jsonl,
}

/// The process exit for a terminal status.
#[must_use]
pub fn exit_code(status: HeadlessStatus) -> ExitCode {
    ExitCode::from(status.code())
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

    fn record(&mut self, stdout: &mut impl Write, record: HeadlessRecordRef<'_>) -> io::Result<()> {
        if self.trace.is_none() && !self.to_stdout {
            return Ok(());
        }
        let line = serde_json::to_string(&record).map_err(io::Error::other)?;
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

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: HeadlessStatus::InvalidConfiguration,
            message: message.into(),
        }
    }
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
    /// How the final answer was audited, when it was.
    audit: Option<Box<qq_protocol::AuditRecord>>,
    /// The typed-output verdict of a run submitted with a contract.
    final_output: Option<Box<qq_protocol::FinalOutput>>,
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
            audit: None,
            final_output: None,
            answer: String::new(),
        }
    }
}

/// Runs one headless task to a terminal status. Never panics the process on
/// task problems: every path maps to a distinguishable exit status.
/// `steering` delivers user lines to inject at the run's next boundary; `None`
/// means the invocation has no steering source.
pub async fn run(
    sessions: &SessionRuntime,
    options: HeadlessOptions,
    interrupt: impl Future<Output = ()>,
    steering: Option<tokio::sync::mpsc::Receiver<String>>,
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

    // `@` mentions resolve here, in the client, exactly as the TUI does:
    // the server never sees the syntax. Notes are advisory and go to stderr.
    let input = {
        let workspace = options.workspace.clone();
        let prompt = options.prompt.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            qq_core::mentions::resolve_prompt(&workspace, &prompt)
        })
        .await
        .unwrap_or_else(|_| qq_core::mentions::ResolvedPrompt {
            parts: vec![qq_protocol::InputPart::text(options.prompt.clone())],
            notes: vec!["mention resolution stopped unexpectedly".to_owned()],
            skill: None,
        });
        for note in &resolved.notes {
            let _ = writeln!(stderr, "note: {note}");
        }
        let mut parts = resolved.parts;
        if let Some(skill) = resolved.skill {
            let rest: String = parts
                .iter()
                .filter_map(|part| match part {
                    qq_protocol::InputPart::Text { text } => Some(text.as_str()),
                    qq_protocol::InputPart::WorkspaceFile { .. } => None,
                })
                .collect();
            let files: Vec<_> = parts
                .into_iter()
                .filter(|part| matches!(part, qq_protocol::InputPart::WorkspaceFile { .. }))
                .collect();
            parts = vec![qq_protocol::InputPart::text(format!("/{skill}{rest}"))];
            parts.extend(files);
        }
        parts
    };
    let handle = match submit(sessions, &options, input).await {
        Ok(handle) => handle,
        Err(failure) => {
            let _ = writeln!(stderr, "error: {}", failure.message);
            return failure.status;
        }
    };
    let mut accepted = AcceptedRunGuard::new(sessions.clone(), handle.clone());

    // The contract compiled before submission, so its canonical encoding
    // exists; an encoding failure here would be a serde_json bug.
    let output_schema_sha256 = options.output.as_deref().map(|contract| {
        let encoded = serde_json::to_vec(&contract.schema).unwrap_or_default();
        ContentHash::from_bytes(Sha256::digest(&encoded).into())
    });
    let trial = HeadlessTrial {
        qq_version: env!("CARGO_PKG_VERSION").to_owned(),
        qq_source_revision: env!("QQ_SOURCE_REVISION").to_owned(),
        protocol_version: qq_protocol::PROTOCOL_VERSION,
        workspace_identity: workspace_identity(&options.workspace),
        model: options.model.clone(),
        profile: options.profile.clone(),
        context_window: options.context_window,
        pricing_provenance: options.pricing_provenance.clone(),
        approval: options.approval,
        timeout_seconds: options.timeout.map(|timeout| timeout.as_secs()),
        max_turns: options.max_turns,
        max_cost_usd_nanos: options.max_cost_usd_nanos,
        correlation: options.correlation.clone(),
        arm: options.arm.clone(),
        output_schema_sha256,
        output_repair_turns: options
            .output
            .as_deref()
            .map(|contract| contract.repair_turns),
        workspace_id: handle.workspace_id,
        session_id: handle.session_id,
        run_id: handle.run_id,
    };
    if let Err(error) = sink.record(stdout, HeadlessRecordRef::Trial(&trial)) {
        let _ = writeln!(stderr, "error: could not write the trial record: {error}");
        if let Err(failure) = accepted.settle().await {
            let _ = writeln!(stderr, "error: {}", failure.message);
        }
        return HeadlessStatus::HarnessFailure;
    }

    let end = match stream_run(
        sessions, &options, &handle, &mut sink, interrupt, steering, stdout, stderr,
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

    let RunEnd {
        status,
        message,
        usage,
        estimated_cost_usd_nanos,
        prompt_identity,
        audit,
        final_output,
        answer,
    } = end;
    let outcome = HeadlessOutcome {
        status,
        exit_code: status.code(),
        message,
        usage,
        estimated_cost_usd_nanos,
        prompt_identity,
        audit,
        final_output,
    };
    if let Err(error) = sink
        .record(stdout, HeadlessRecordRef::Outcome(&outcome))
        .and_then(|()| sink.finish())
    {
        let _ = writeln!(stderr, "error: could not write the outcome record: {error}");
        return HeadlessStatus::HarnessFailure;
    }

    if options.format == HeadlessFormat::Text {
        match status {
            HeadlessStatus::Completed => {
                let _ = writeln!(stderr);
                // With a contract the validated document is the answer: a
                // consumer piping stdout gets exactly the JSON, not a fence
                // the model may have wrapped it in.
                let mut answer = match outcome.final_output.as_deref() {
                    Some(qq_protocol::FinalOutput::Valid { value, .. }) => {
                        serde_json::to_string_pretty(value).unwrap_or(answer)
                    }
                    Some(qq_protocol::FinalOutput::Invalid { .. }) | None => answer,
                };
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
                if let Some(message) = &outcome.message {
                    let _ = writeln!(stderr, "error: {message}");
                }
            }
        }
    } else if let Some(message) = &outcome.message {
        let _ = writeln!(stderr, "error: {message}");
    }
    // The session persists whatever the outcome; an interrupted or exhausted
    // run is exactly when a person wants to pick it up again.
    if options.resume_hint && options.format == HeadlessFormat::Text {
        let _ = write!(stderr, "\n{}", crate::cli::resume_hint(handle.session_id));
    }

    status
}

/// Resolves the workspace, creates the session (or adopts the requested
/// existing one) with the model and approval choices, and submits the prompt.
/// Any failure here happens before the model sees the task.
async fn submit(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    input: Vec<qq_protocol::InputPart>,
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

    let (session_id, subscribe_after) = match options.session {
        None => {
            let created = send(
                sessions,
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: options.model.clone(),
                    approval_mode: approval_mode(options.approval),
                    profile: options.profile.clone(),
                    correlation: options.correlation.clone(),
                },
            )
            .await?;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                return Err(Failure::harness(
                    "session creation returned an unexpected outcome",
                ));
            };
            (session_id, created.committed_through)
        }
        Some(session_id) => {
            // Store ownership was taken when the runtime opened and its
            // recovery sweep has already run, so what the snapshot shows is
            // the session's settled state. Everything below is a rejection
            // or a session-level setting; no run exists until the prompt
            // is admitted.
            let snapshot = match sessions
                .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 1, 1))
                .await
            {
                Ok(snapshot) => snapshot,
                // Unknown ids and sessions of other workspaces are the same
                // answer: nothing to resume here. Neither leaks that the id
                // exists elsewhere.
                Err(SessionRuntimeError::SessionNotFound) => {
                    return Err(Failure::invalid(format!(
                        "session {session_id} does not exist in this workspace"
                    )));
                }
                Err(error) => {
                    return Err(Failure {
                        status: status_for_error(&error),
                        message: error.to_string(),
                    });
                }
            };
            let Some(focused) = snapshot.focused else {
                return Err(Failure::harness(
                    "workspace snapshot omitted the focused session",
                ));
            };
            let summary = focused.summary;
            if summary.parent_id.is_some() {
                return Err(Failure::invalid(format!(
                    "session {session_id} is a spawned sub-agent session; \
                     resume its root session instead"
                )));
            }
            if summary.status != qq_protocol::SessionStatus::Idle
                || summary.active_run_id.is_some()
                || summary.queued_prompts > 0
            {
                return Err(Failure::invalid(format!(
                    "session {session_id} is {}; it must be idle with no queued prompts",
                    match summary.status {
                        qq_protocol::SessionStatus::Idle => "settling",
                        qq_protocol::SessionStatus::Queued => "queued",
                        qq_protocol::SessionStatus::Running => "running",
                    }
                )));
            }
            // The invocation decides the run, exactly as it would for a new
            // session. The model selection is always written (the summary
            // shows only the route, not the output cap or organization);
            // profile and approval are written only when they differ.
            let receipt = send(
                sessions,
                SessionCommand::SetSessionModel {
                    session_id,
                    model: options.model.clone(),
                },
            )
            .await?;
            let mut after = receipt.committed_through;
            if summary.profile != options.profile {
                let receipt = send(
                    sessions,
                    SessionCommand::SetSessionProfile {
                        session_id,
                        profile: options.profile.clone(),
                    },
                )
                .await?;
                after = receipt.committed_through;
            }
            if summary.approval_mode != approval_mode(options.approval) {
                let receipt = send(
                    sessions,
                    SessionCommand::SetApprovalMode {
                        session_id,
                        mode: approval_mode(options.approval),
                    },
                )
                .await?;
                after = receipt.committed_through;
            }
            (session_id, after)
        }
    };

    // Budgets are core-owned: the runtime enforces them and settles the run
    // with a typed outcome, so this adapter only relays and renders.
    let queued = send(
        sessions,
        SessionCommand::SubmitPrompt {
            session_id,
            input,
            limits: RunLimits {
                max_duration_ms: options
                    .timeout
                    .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
                max_model_turns: options.max_turns,
                max_tool_calls: None,
                max_total_tokens: None,
                max_cost_usd_nanos: options.max_cost_usd_nanos,
                max_input_tokens: None,
                max_output_tokens: None,
                max_tool_output_bytes: None,
                max_children: None,
                max_concurrent_children: None,
            },
            correlation: options.correlation.clone(),
            output: options.output.clone(),
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
        // Subscribing from the cursor just before submission replays the
        // queued prompt and everything after it, so no event is lost to the
        // gap between submission and subscription. For a resumed session that
        // is the snapshot cursor (or the last settings write), not the
        // session's history.
        subscribe_after,
    })
}

/// Streams durable events to completion, answering approvals, relaying an
/// interrupt through the ordinary idempotent cancellation command, and
/// rendering output per the selected format. Time and budget bounds are
/// enforced by the core runtime and arrive as typed run outcomes.
#[expect(
    clippy::too_many_arguments,
    reason = "the CLI's resolved options, passed once from main"
)]
async fn stream_run(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    handle: &RunHandle,
    sink: &mut RecordSink,
    interrupt: impl Future<Output = ()>,
    mut steering: Option<tokio::sync::mpsc::Receiver<String>>,
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

    let mut interrupt = pin!(interrupt);
    let mut interrupt_armed = true;
    let mut interrupted = false;
    // Set when the model asked the user a question nobody here can answer:
    // the run is cancelled and settles as `needs_input`, naming the question.
    let mut needs_input: Option<String> = None;
    let mut shutdown_at: Option<Instant> = None;

    // The final answer is the text of the last assistant message that starts
    // streaming; earlier turns are progress, not the answer.
    let mut answer = String::new();
    let mut answer_message: Option<MessageId> = None;
    let mut answer_truncated = false;
    let text = options.format == HeadlessFormat::Text;

    loop {
        tokio::select! {
            biased;
            () = interrupt.as_mut(), if interrupt_armed => {
                interrupt_armed = false;
                if !interrupted {
                    request_cancel(sessions, handle.run_id).await?;
                    interrupted = true;
                    shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
                    if text {
                        let _ = writeln!(stderr, "[run] interrupt received; cancelling");
                    }
                }
            }
            line = async { steering.as_mut().unwrap().recv().await }, if steering.is_some() => {
                match line {
                    Some(line) if !line.trim().is_empty() => {
                        // Steering that arrives before the run starts or after it
                        // ends is refused by the runtime; that is reported and
                        // the run continues rather than failing the invocation.
                        match send(
                            sessions,
                            SessionCommand::SteerRun {
                                run_id: handle.run_id,
                                input: vec![InputPart::text(line)],
                                interrupt: false,
                            },
                        )
                        .await
                        {
                            Ok(_) if text => {
                                let _ = writeln!(stderr, "[run] steering queued");
                            }
                            Ok(_) => {}
                            Err(failure) => {
                                let _ = writeln!(stderr, "warning: steering refused: {}", failure.message);
                            }
                        }
                    }
                    Some(_) => {}
                    None => steering = None,
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
                sink.record(stdout, HeadlessRecordRef::Event { envelope: &envelope })
                    .map_err(|error| {
                        Failure::harness(format!("could not write an event record: {error}"))
                    })?;

                let ours = envelope.session_id == handle.session_id;
                match &envelope.event {
                    SessionEvent::AssistantMessageStarted { message } if ours => {
                        if message.role == MessageRole::Assistant {
                            // A message that follows a truncated one is the
                            // same answer resumed: keep the prefix.
                            if !answer_truncated {
                                answer.clear();
                            }
                            answer_truncated = false;
                            answer_message = Some(message.id);
                            answer.push_str(&message.output);
                        }
                    }
                    SessionEvent::RunOutputTruncated { continuation, .. } if ours => {
                        answer_truncated = true;
                        if text {
                            let _ = writeln!(
                                stderr,
                                "\n[run] output truncated; continuing ({continuation}/{})",
                                qq_core::MAX_OUTPUT_CONTINUATIONS
                            );
                        }
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
                    SessionEvent::RoutingStarted { .. } if ours => {
                        if text { let _ = writeln!(stderr, "[jev] routing pending"); }
                    }
                    SessionEvent::RoutingCompleted { decision, .. } if ours => {
                        if text {
                            let _ = writeln!(stderr, "[jev] routing {:?}: {}; {}", decision.outcome,
                                decision.model.model.as_deref().unwrap_or("configured model"), concise(&decision.reason));
                        }
                    }
                    SessionEvent::CheckpointReviewed {
                        correlation,
                        phase,
                        outcome,
                        confidence_basis_points,
                        feedback,
                        ..
                    } if ours => {
                        if text {
                            let color = if matches!(outcome, qq_protocol::CheckpointOutcome::Supported) {
                                "GREEN"
                            } else {
                                "RED"
                            };
                            let confidence = confidence_basis_points
                                .map(|value| format!(" confidence={:.2}%", f64::from(value) / 100.0))
                                .unwrap_or_default();
                            let _ = writeln!(
                                stderr,
                                "[jev] {color} {phase:?} {correlation} outcome={outcome:?}{confidence}: {feedback}"
                            );
                        }
                    }
                    SessionEvent::ToolApprovalRequested { tool_call, question: Some(question), .. }
                        if ours && envelope.run_id == Some(handle.run_id) =>
                    {
                        // No human is attached to a headless run: rather
                        // than hang or fake an answer, stop at the question
                        // so a supervisor can resume with the answer.
                        if needs_input.is_none() {
                            let first = question
                                .questions
                                .first()
                                .map(|item| item.prompt.clone())
                                .unwrap_or_default();
                            if text {
                                let _ = writeln!(
                                    stderr,
                                    "[tool] {} needs input: {first}",
                                    tool_call.name
                                );
                            }
                            needs_input = Some(first);
                            request_cancel(sessions, handle.run_id).await?;
                            shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
                        }
                    }
                    SessionEvent::ToolApprovalRequested { tool_call, shell, question, fetch, .. } => {
                        // A child session's question has no answerer either;
                        // declining lets the child proceed on its judgement.
                        if question.is_some() {
                            if let Some(run_id) = envelope.run_id {
                                respond_approval(
                                    sessions,
                                    run_id,
                                    tool_call.id,
                                    ApprovalDecision::Answer { answers: Vec::new() },
                                    stderr,
                                )
                                .await;
                            }
                            continue;
                        }
                        // The headless invocation is the approval client.
                        // An explicit allowlist answers first, as a session
                        // grant so the same tool or prefix is not held again.
                        // Otherwise full approves everything unattended; auto
                        // denies whatever the policy escalated (dangerous
                        // shell) so the run never stalls waiting for a human —
                        // but when a reviewer model is configured the deny is
                        // deferred briefly, giving the reviewer its window.
                        // A late deny is harmless: resolution is idempotent,
                        // so a reviewer approval that landed first stands.
                        // A child session's held calls belong to a supervised
                        // write child: the reviewer adjudicates them, and the
                        // headless root only supplies the unattended fallback
                        // (a deferred deny), never a blanket approve. The
                        // allowlist still applies: the human declared it.
                        let granted = allowlisted_grant(
                            options,
                            &tool_call.name,
                            shell.as_deref(),
                            fetch.as_deref(),
                        );
                        let approval = if ours {
                            options.approval
                        } else {
                            HeadlessApproval::Auto
                        };
                        let decision = match (granted, approval) {
                            (Some(grant), _) => {
                                if text {
                                    let _ = writeln!(
                                        stderr,
                                        "[tool] {} approved for the session by allowlist",
                                        tool_call.name
                                    );
                                }
                                Some(ApprovalDecision::ApproveForSession { grant })
                            }
                            (None, HeadlessApproval::Full) => Some(ApprovalDecision::ApproveOnce),
                            (None, HeadlessApproval::Auto) if options.reviewer_configured || !ours => {
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
                            (None, HeadlessApproval::Auto | HeadlessApproval::ReadOnly) => {
                                Some(ApprovalDecision::Deny)
                            }
                        };
                        if let (Some(run_id), Some(decision)) = (envelope.run_id, decision) {
                            respond_approval(sessions, run_id, tool_call.id, decision, stderr)
                                .await;
                        }
                    }
                    SessionEvent::RunFinished { session, run_id, outcome, usage, final_output, .. }
                        if *run_id == handle.run_id => {
                        let usage = inclusive_usage(session.accounting, *usage);
                        let cost = inclusive_cost(
                            session.accounting,
                            session.estimated_cost_usd_nanos,
                        );
                        let (status, message) = match (&needs_input, outcome) {
                            (Some(question), RunOutcome::Cancelled) => (
                                HeadlessStatus::NeedsInput,
                                Some(format!(
                                    "the model asked the user a question and no client could answer: {question}"
                                )),
                            ),
                            _ => settle(outcome, final_output.as_deref(), interrupted),
                        };
                        if text
                            && matches!(
                                status,
                                HeadlessStatus::BudgetExhausted | HeadlessStatus::TimedOut
                            )
                        {
                            let _ = writeln!(stderr, "[run] {}", status.as_str());
                        }
                        let (prompt_identity, audit) =
                            run_prompt_identity(sessions, handle).await?;
                        return Ok(RunEnd {
                            status,
                            message,
                            usage,
                            estimated_cost_usd_nanos: cost,
                            prompt_identity,
                            audit,
                            final_output: final_output.clone(),
                            answer,
                        });
                    }
                    _ => {}
                }
            }
        }
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

fn workspace_identity(workspace: &Path) -> ContentHash {
    ContentHash::from_bytes(Sha256::digest(workspace.as_os_str().as_encoded_bytes()).into())
}

async fn run_prompt_identity(
    sessions: &SessionRuntime,
    handle: &RunHandle,
) -> Result<
    (
        Option<Box<RunPromptIdentity>>,
        Option<Box<qq_protocol::AuditRecord>>,
    ),
    Failure,
> {
    let snapshot = sessions
        .snapshot(SnapshotRequest {
            workspace_id: handle.workspace_id,
            focused_session_id: Some(handle.session_id),
            include_sessions: Vec::new(),
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
        .map(|run| (run.prompt_identity, run.audit))
        .ok_or_else(|| Failure::harness("the terminal run is missing from its session snapshot"))
}

/// Maps the run's durable outcome plus this invocation's interrupt intent to
/// a terminal status. Budget outcomes are core-owned: the wall-clock bound
/// keeps its historical `timed_out` status; every other bound is
/// `budget_exhausted`.
/// A completed run whose answer failed its output contract is a task failure
/// (exit 1): the agent finished but did not deliver what was asked. Exit codes
/// stay stable; `final_output.status` tells the two apart.
fn settle(
    outcome: &RunOutcome,
    final_output: Option<&qq_protocol::FinalOutput>,
    interrupted: bool,
) -> (HeadlessStatus, Option<String>) {
    match (outcome, final_output) {
        (
            RunOutcome::Completed,
            Some(qq_protocol::FinalOutput::Invalid {
                errors,
                repair_turns,
            }),
        ) => (
            HeadlessStatus::TaskFailed,
            Some(format!(
                "the final answer did not satisfy the output schema after {repair_turns} repair turn(s): {}",
                errors.join("; ")
            )),
        ),
        (RunOutcome::Completed, Some(qq_protocol::FinalOutput::Valid { .. }) | None) => {
            (HeadlessStatus::Completed, None)
        }
        (outcome, _) => settle_outcome(outcome, interrupted),
    }
}

fn settle_outcome(outcome: &RunOutcome, interrupted: bool) -> (HeadlessStatus, Option<String>) {
    match outcome {
        RunOutcome::Completed => (HeadlessStatus::Completed, None),
        RunOutcome::Failed { failure } => {
            let status = match failure.kind {
                qq_protocol::RunFailureKind::Server => HeadlessStatus::HarnessFailure,
                _ => HeadlessStatus::TaskFailed,
            };
            (status, Some(failure.message.clone()))
        }
        RunOutcome::BudgetExhausted { exhaustion } => {
            let status = match exhaustion.limit {
                BudgetLimitKind::Duration => HeadlessStatus::TimedOut,
                BudgetLimitKind::ModelTurns
                | BudgetLimitKind::ToolCalls
                | BudgetLimitKind::TotalTokens
                | BudgetLimitKind::Cost
                | BudgetLimitKind::CostUnknown
                | BudgetLimitKind::InputTokens
                | BudgetLimitKind::OutputTokens
                | BudgetLimitKind::TokensUnknown
                | BudgetLimitKind::ToolOutputBytes => HeadlessStatus::BudgetExhausted,
            };
            (status, Some(exhaustion.message.clone()))
        }
        RunOutcome::Cancelled if interrupted => (
            HeadlessStatus::Interrupted,
            Some("the run was cancelled by an interrupt".to_owned()),
        ),
        // Same exit a retry-exhausted provider failure has always had, so
        // supervisors see no new code; the message names the pause.
        RunOutcome::Paused { pause } => (
            HeadlessStatus::TaskFailed,
            Some(format!(
                "the run paused after {} retries of turn {} on a provider fault: {}",
                pause.attempts, pause.turn_ordinal, pause.message
            )),
        ),
        RunOutcome::Cancelled => (
            HeadlessStatus::HarnessFailure,
            Some("the run was cancelled outside this invocation".to_owned()),
        ),
        RunOutcome::Interrupted => (
            HeadlessStatus::HarnessFailure,
            Some("the run was interrupted before reaching a terminal outcome".to_owned()),
        ),
    }
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
/// The session grant an allowlist entry earns a held call, if any. Shell
/// prefixes match the server's own preview of the command with the policy's
/// word-boundary rule, so `--allow-shell "cargo test"` covers
/// `cargo test -p qq-core` and never `cargo test | sh`.
fn allowlisted_grant(
    options: &HeadlessOptions,
    tool_name: &str,
    shell: Option<&ShellCommandPreview>,
    fetch: Option<&qq_protocol::FetchPreview>,
) -> Option<ApprovalGrant> {
    if options.allow_tools.iter().any(|name| name == tool_name) {
        return Some(ApprovalGrant::Tool {
            name: tool_name.to_owned(),
        });
    }
    if let Some(fetch) = fetch {
        return options
            .allow_hosts
            .iter()
            .find(|grant| qq_core::host_grant_matches(grant, &fetch.host))
            .map(|grant| ApprovalGrant::Host {
                host: grant.clone(),
            });
    }
    let command = shell.map(|preview| preview.command.as_str())?;
    options
        .allow_shell_prefixes
        .iter()
        .find(|prefix| qq_core::shell_prefix_matches(prefix, command))
        .map(|prefix| ApprovalGrant::ShellPrefix {
            prefix: prefix.clone(),
        })
}

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
        | SessionRuntimeError::InvalidSlashCommand(_)
        | SessionRuntimeError::InvalidRunLimits
        | SessionRuntimeError::InvalidOutputContract(_)
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
    use qq_provider::{
        Message, ModelRequest, Provider, ProviderEvent, ProviderStream, ProviderUsage,
    };

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

    /// Builds a fresh provider per claimed run, mirroring how the real
    /// loader compiles a runtime per run.
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

    /// Issues a dangerous shell command (held under `auto`) on each of two
    /// turns, then answers. Distinct call ids so the second is a fresh
    /// approval decision, not a replay.
    struct DangerousShellProvider {
        turn: Mutex<usize>,
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
                        json: format!(r#"{{"command":"rm -r scratch{current}"}}"#),
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

    /// Turn one asks the user a question; a second turn would answer, but a
    /// headless run never gets there.
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

    /// Turn one fetches a reserved (never-resolving) host; turn two answers.
    struct FetchingProvider {
        turn: Mutex<usize>,
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
                        json: r#"{"url":"https://docs.invalid/x"}"#.to_owned(),
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

    /// Turn one requests a read, holding its completion until released, so a
    /// steering line sent meanwhile lands at the boundary before turn two.
    /// Turn two answers and records the request it saw.
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
            // The slice checkpoint keeps tools declared (RR1); the system-prompt
            // notice is the observable marker of that turn.
            let checkpoint = request
                .system()
                .is_some_and(|system| system.contains("safe tool-call boundary"));
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
        // macOS commonly exposes TMPDIR through `/var`, which is a symlink to
        // `/private/var`. Store databases deliberately use SQLite NOFOLLOW, so
        // fixtures must construct both workspace and database paths from the
        // canonical temporary root.
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
            reviewer_configured: false,
            allow_tools: Vec::new(),
            allow_shell_prefixes: Vec::new(),
            allow_hosts: Vec::new(),
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

    /// Every stdout line must be a strict `HeadlessRecord` that re-encodes
    /// byte-for-byte: the binary emits exactly the protocol's vocabulary.
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

    /// The selected profile reaches the runtime loader with the run (which
    /// is what compiles the pack persona and tool policy) and is recorded
    /// in the trial metadata so a benchmark knows which profile ran it.
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
        // Under auto, edits and shell commands the classifier allows (a
        // workspace-relative `cp`) run directly: no approval round-trip
        // happened and nothing waited for a human.
        assert!(
            event_records(&records)
                .iter()
                .all(|record| { record["envelope"]["event"]["type"] != "tool_approval_requested" })
        );
    }

    /// `--allow-shell` answers a held command with a session grant: the first
    /// request is approved for the session and the second identical prefix
    /// is never held at all. A prefix that does not match is denied as
    /// before, and the grant never extends over a control character.
    #[tokio::test]
    async fn shell_allowlist_grants_the_session_on_first_hold() {
        let fixture = fixture(|| DangerousShellProvider {
            turn: Mutex::new(0),
        })
        .await;
        for name in ["scratch0", "scratch1"] {
            std::fs::create_dir_all(fixture.workspace.join(name)).unwrap();
        }
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        options.allow_shell_prefixes = vec!["rm -r".to_owned()];

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        assert!(!fixture.workspace.join("scratch0").exists());
        assert!(!fixture.workspace.join("scratch1").exists());
        let records = parse_records(&stdout);
        let approvals: Vec<_> = event_records(&records)
            .into_iter()
            .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_requested")
            .collect();
        // The runtime asks about the first call only; the grant covers the
        // second, which is never held.
        assert_eq!(approvals.len(), 1, "one held call");
        let resolved = event_records(&records)
            .into_iter()
            .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
            .count();
        assert_eq!(resolved, 1);
        assert!(
            finished_tool_calls(&records)
                .iter()
                .all(|call| call["state"] == "completed")
        );

        // A prefix that matches nothing leaves auto's deny in place.
        let unmatched = self::fixture(|| DangerousShellProvider {
            turn: Mutex::new(0),
        })
        .await;
        let mut strict = self::options(&unmatched.workspace);
        strict.approval = HeadlessApproval::Auto;
        strict.allow_shell_prefixes = vec!["rm -rf".to_owned(), "cargo".to_owned()];
        let (status, stdout, _) = run_to_end(&unmatched, strict, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed);
        assert!(
            finished_tool_calls(&parse_records(&stdout))
                .iter()
                .all(|call| call["state"] == "denied")
        );
    }

    /// `--allow-tool` approves a held tool for the session under read-only
    /// too: the allowlist is explicit authority, narrower than `full`.
    #[tokio::test]
    async fn tool_allowlist_approves_held_calls_under_auto() {
        let fixture = fixture(|| DangerousShellProvider {
            turn: Mutex::new(0),
        })
        .await;
        for name in ["scratch0", "scratch1"] {
            std::fs::create_dir_all(fixture.workspace.join(name)).unwrap();
        }
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        options.allow_tools = vec!["shell".to_owned()];
        let (status, stdout, _) = run_to_end(&fixture, options, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed);
        assert!(
            finished_tool_calls(&parse_records(&stdout))
                .iter()
                .all(|call| call["state"] == "completed")
        );
    }

    /// A line on the steering channel is injected at the run's next boundary
    /// and reaches the model on the following turn; a blank line is ignored;
    /// closing the channel does not end the run.
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
            // Wait until the run is executing (turn one is held open by the
            // provider), steer, wait for the steering to be durably queued,
            // then release the turn so the boundary applies it.
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if session_requests.lock().unwrap().len() == 1 {
                    break;
                }
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
                tokio::time::sleep(Duration::from_millis(20)).await;
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

    /// `--allow-host` answers a held `fetch` with a session host grant; the
    /// grant is the matching pattern, not the request's host, so a wildcard
    /// covers the site's other names too.
    #[tokio::test]
    async fn allow_host_grants_a_held_fetch_for_the_session() {
        let fixture = fixture(|| FetchingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        options.allow_hosts = vec!["*.invalid".to_owned()];
        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        let records = parse_records(&stdout);
        let resolved = event_records(&records)
            .into_iter()
            .find(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
            .expect("the allowlist resolves the hold");
        assert_eq!(
            resolved["envelope"]["event"]["resolution"],
            "approved_for_session"
        );
        let requested = event_records(&records)
            .into_iter()
            .find(|record| record["envelope"]["event"]["type"] == "tool_approval_requested")
            .unwrap();
        assert_eq!(
            requested["envelope"]["event"]["fetch"]["host"],
            "docs.invalid"
        );
        // The approved call ran and failed at resolution: a tool error, not
        // a denial, and the run still completed.
        let finished = finished_tool_calls(&records);
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0]["state"], "failed");
        assert!(
            finished[0]["result"]
                .as_str()
                .is_some_and(|result| result.contains("could not resolve docs.invalid")),
            "{}",
            finished[0]["result"]
        );

        // Without the allowlist, auto denies the unattended hold.
        let bare = self::fixture(|| FetchingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let mut bare_options = self::options(&bare.workspace);
        bare_options.approval = HeadlessApproval::Auto;
        let (status, stdout, _) = run_to_end(&bare, bare_options, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        let resolved = event_records(&records)
            .into_iter()
            .find(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
            .unwrap();
        assert_eq!(resolved["envelope"]["event"]["resolution"], "denied");
        assert_eq!(
            resolved["envelope"]["event"]["tool_call"]["state"],
            "denied"
        );
    }

    /// A question with nobody to answer it ends the run at the question with
    /// its own status, rather than hanging until the approval timeout or
    /// faking an answer. The stream carries the question for a supervisor.
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
        // The cancel interrupts the held call; the model never got a second
        // turn, so nothing was answered on its behalf.
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

    /// Answers with text and records every request, so a test can see the
    /// history a resumed run was given.
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
        // The unresolvable mention is reported, not fatal, and left literal.
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
            prompt.contains(
                "<attached-file path=\"notes.md\" lines=\"2-3/3\">\n````\nbeta\ngamma\n````"
            ),
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
        // The transcript row keeps the placeholder form.
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

    #[tokio::test]
    async fn session_resume_submits_into_the_idle_session_and_applies_the_invocation() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fixture = {
            let requests = Arc::clone(&requests);
            fixture(move || RecordingTextProvider {
                requests: Arc::clone(&requests),
            })
            .await
        };

        // First run creates the session.
        let first = HeadlessOptions {
            prompt: "first task".to_owned(),
            ..options(&fixture.workspace)
        };
        let (status, stdout, _) = run_to_end(&fixture, first, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        let session_id: SessionId = records[0]["session_id"].as_str().unwrap().parse().unwrap();
        let first_run_id = records[0]["run_id"].as_str().unwrap().to_owned();

        // Second run resumes it with a different approval and model cap.
        let second = HeadlessOptions {
            prompt: "second task".to_owned(),
            session: Some(session_id),
            approval: HeadlessApproval::Auto,
            model: ModelSelection {
                model_is_fallback: false,
                model: Some("test/model".to_owned()),
                max_output_tokens: Some(128),
                organization: None,
            },
            ..options(&fixture.workspace)
        };
        let (status, stdout, stderr) = run_to_end(&fixture, second, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        let records = parse_records(&stdout);
        assert_eq!(records[0]["type"], "trial");
        assert_eq!(records[0]["session_id"], session_id.to_string());
        assert_ne!(records[0]["run_id"], first_run_id);
        assert_eq!(records[0]["approval"], "auto");
        // No session_created: the session already existed. The stream starts
        // at the prompt, not at the session's history and not at the
        // settings writes that preceded submission.
        let events = event_records(&records);
        assert!(events.iter().all(|record| {
            let kind = &record["envelope"]["event"]["type"];
            kind != "session_created" && kind != "session_updated"
        }));
        assert_eq!(
            events[0]["envelope"]["event"]["type"], "prompt_queued",
            "{:?}",
            events[0]
        );
        assert_eq!(
            events
                .iter()
                .filter(|record| record["envelope"]["event"]["type"] == "prompt_queued")
                .count(),
            1
        );

        // The resumed run's provider request carries the first exchange.
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 2);
            let history = captured[1].messages();
            assert!(history.len() >= 3, "{history:?}");
            assert_eq!(history[0], Message::user("first task"));
            assert_eq!(history[history.len() - 1], Message::user("second task"));
        }

        let workspace = workspace_snapshot(&fixture).await;
        assert_eq!(workspace.sessions.len(), 1, "no second session was created");
        let session = &workspace.sessions[0];
        assert_eq!(session.approval_mode, ApprovalMode::Auto);
        assert_eq!(session.status, SessionStatus::Idle);
    }

    #[tokio::test]
    async fn session_resume_rejects_unknown_foreign_busy_and_child_sessions() {
        let fixture = fixture(|| TextProvider).await;

        // Unknown id: invalid configuration, nothing created.
        let unknown = HeadlessOptions {
            session: Some(SessionId::generate().unwrap()),
            ..options(&fixture.workspace)
        };
        let (status, stdout, stderr) = run_to_end(&fixture, unknown, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::InvalidConfiguration);
        assert!(
            stderr.contains("does not exist in this workspace"),
            "{stderr}"
        );
        assert!(stdout.is_empty(), "no trial record before a session exists");
        assert!(workspace_snapshot(&fixture).await.sessions.is_empty());

        // A session in another workspace is not visible from this one.
        let other_workspace = fixture.workspace.parent().unwrap().join("other");
        std::fs::create_dir_all(&other_workspace).unwrap();
        let (status, stdout, _) = run_to_end(
            &fixture,
            HeadlessOptions {
                workspace: other_workspace.clone(),
                ..options(&other_workspace)
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(status, HeadlessStatus::Completed);
        let foreign: SessionId = parse_records(&stdout)[0]["session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let (status, _, stderr) = run_to_end(
            &fixture,
            HeadlessOptions {
                session: Some(foreign),
                ..options(&fixture.workspace)
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(status, HeadlessStatus::InvalidConfiguration);
        assert!(
            stderr.contains("does not exist in this workspace"),
            "{stderr}"
        );

        // A child session is never a resume target.
        let resolved = send(
            &fixture.sessions,
            SessionCommand::ResolveWorkspace {
                path: fixture.workspace.display().to_string(),
            },
        )
        .await
        .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
            panic!("unexpected receipt");
        };
        let created = send(
            &fixture.sessions,
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: None,
                model: options(&fixture.workspace).model,
                approval_mode: ApprovalMode::ReadOnly,
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
        .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt");
        };
        let child = send(
            &fixture.sessions,
            SessionCommand::CreateSession {
                workspace_id,
                parent_id: Some(session_id),
                model: options(&fixture.workspace).model,
                approval_mode: ApprovalMode::ReadOnly,
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
        .unwrap();
        let CommandOutcome::SessionCreated {
            session_id: child_id,
        } = child.outcome
        else {
            panic!("unexpected receipt");
        };
        let (status, _, stderr) = run_to_end(
            &fixture,
            HeadlessOptions {
                session: Some(child_id),
                ..options(&fixture.workspace)
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(status, HeadlessStatus::InvalidConfiguration);
        assert!(stderr.contains("sub-agent"), "{stderr}");
    }

    /// Hangs on the first request (so a process can die mid-run) and answers
    /// with text on every later one, recording what it saw.
    struct HangsOnceProvider {
        turn: Arc<Mutex<usize>>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for HangsOnceProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::pending())
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "recovered".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    #[tokio::test]
    async fn session_resume_after_an_unclean_exit_recovers_the_interrupted_run_first() {
        let turn = Arc::new(Mutex::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let loader: Arc<dyn RuntimeLoader> = {
            let turn = Arc::clone(&turn);
            let requests = Arc::clone(&requests);
            Arc::new(ProviderLoader(move || HangsOnceProvider {
                turn: Arc::clone(&turn),
                requests: Arc::clone(&requests),
            }))
        };
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(&workspace).unwrap();
        let database = directory.path().join("sessions.sqlite3");

        // Process one: start a run that never finishes, then die without
        // shutting down. The store is left with a `running` run.
        let first =
            SessionRuntime::open(SessionRuntimeOptions::new(database.clone()), loader.clone())
                .await
                .unwrap();
        let handle = submit(
            &first,
            &HeadlessOptions {
                prompt: "hang".to_owned(),
                ..options(&workspace)
            },
            vec![qq_protocol::InputPart::text("hang")],
        )
        .await
        .unwrap();
        let mut events = first
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let envelope = events.next().await.unwrap().unwrap();
                if matches!(envelope.event, SessionEvent::RunStarted { .. }) {
                    break;
                }
            }
            // `RunStarted` precedes the first provider request; die only
            // once the provider is actually mid-turn.
            while requests.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(events);
        // The hanging task still holds the runtime; abandon the store, which
        // is what process death looks like to it.
        first.abandon_for_test().await.unwrap();
        drop(first);

        // Process two: `qq run --session ID`. Opening the runtime recovers
        // the interrupted run before submit() ever looks at the session.
        let second = SessionRuntime::open(SessionRuntimeOptions::new(database), loader)
            .await
            .unwrap();
        let fixture = Fixture {
            sessions: second,
            workspace: workspace.clone(),
            _directory: directory,
        };
        let (status, stdout, stderr) = run_to_end(
            &fixture,
            HeadlessOptions {
                prompt: "continue".to_owned(),
                session: Some(handle.session_id),
                ..options(&workspace)
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        let records = parse_records(&stdout);
        assert_eq!(records[0]["session_id"], handle.session_id.to_string());
        assert_ne!(records[0]["run_id"], handle.run_id.to_string());

        // The first run settled as interrupted, durably, before the resume.
        let snapshot = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                8,
                8,
            ))
            .await
            .unwrap();
        let runs = &snapshot.focused.unwrap().runs;
        let interrupted = runs.iter().find(|run| run.id == handle.run_id).unwrap();
        assert_eq!(interrupted.status, RunStatus::Interrupted);
        assert_eq!(runs.len(), 2);

        // The provider was asked exactly twice: the hung first turn and the
        // resumed turn. The interrupted turn was not re-executed, and the
        // resumed request carries the notice about it.
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let history = captured[1].messages();
        assert_eq!(history[0], Message::user("hang"));
        assert!(
            history.iter().any(|message| {
                message.content().iter().any(|block| {
                    matches!(
                        block,
                        qq_provider::ContentBlock::Text { text }
                            if text.contains("Do not automatically retry tool calls")
                    )
                })
            }),
            "{history:?}"
        );
        assert_eq!(history[history.len() - 1], Message::user("continue"));
    }

    /// Answers each turn with the next scripted text.
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

    fn report_contract(repair_turns: u8) -> Box<qq_protocol::OutputContract> {
        Box::new(qq_protocol::OutputContract {
            schema: serde_json::json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "required": ["ok"],
                "additionalProperties": false
            }),
            repair_turns,
        })
    }

    #[tokio::test]
    async fn a_valid_typed_answer_rides_the_outcome_record_and_the_trial_names_the_schema() {
        let fixture =
            fixture(|| ScriptedTextProvider::new(&["```json\n{\"ok\": true}\n```"])).await;
        let options = HeadlessOptions {
            output: Some(report_contract(3)),
            ..options(&fixture.workspace)
        };

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert_eq!(records[0]["type"], "trial");
        assert_eq!(records[0]["output_repair_turns"], 3);
        assert_eq!(
            records[0]["output_schema_sha256"].as_str().unwrap().len(),
            64
        );
        let outcome = records.last().unwrap();
        assert_eq!(outcome["type"], "outcome");
        assert_eq!(outcome["status"], "completed");
        assert_eq!(outcome["exit_code"], 0);
        assert_eq!(outcome["final_output"]["status"], "valid");
        assert_eq!(
            outcome["final_output"]["value"],
            serde_json::json!({"ok": true})
        );
        assert_eq!(outcome["final_output"]["repair_turns"], 0);
        // The event stream carries the same verdict on run_finished.
        let finished = event_records(&records)
            .into_iter()
            .find(|record| record["envelope"]["event"]["type"] == "run_finished")
            .expect("a run_finished event");
        assert_eq!(
            finished["envelope"]["event"]["final_output"]["status"],
            "valid"
        );
    }

    #[tokio::test]
    async fn a_repaired_typed_answer_reports_the_repairs_it_spent() {
        let fixture =
            fixture(|| ScriptedTextProvider::new(&["not json", r#"{"ok": false}"#])).await;
        let options = HeadlessOptions {
            output: Some(report_contract(2)),
            ..options(&fixture.workspace)
        };

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        let outcome = parse_records(&stdout).pop().unwrap();
        assert_eq!(outcome["final_output"]["status"], "valid");
        assert_eq!(
            outcome["final_output"]["value"],
            serde_json::json!({"ok": false})
        );
        assert_eq!(outcome["final_output"]["repair_turns"], 1);
    }

    #[tokio::test]
    async fn an_answer_that_never_validates_is_a_task_failure_with_the_typed_verdict() {
        let fixture = fixture(|| ScriptedTextProvider::new(&["nope", r#"{"ok": "yes"}"#])).await;
        let options = HeadlessOptions {
            output: Some(report_contract(1)),
            ..options(&fixture.workspace)
        };

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::TaskFailed);
        let outcome = parse_records(&stdout).pop().unwrap();
        assert_eq!(outcome["status"], "task_failed");
        assert_eq!(outcome["exit_code"], 1);
        assert_eq!(outcome["final_output"]["status"], "invalid");
        assert_eq!(outcome["final_output"]["repair_turns"], 1);
        assert_eq!(
            outcome["final_output"]["errors"],
            serde_json::json!(["/ok: expected boolean, found string"])
        );
        assert!(
            outcome["message"]
                .as_str()
                .unwrap()
                .contains("did not satisfy the output schema after 1 repair turn(s)"),
            "{outcome}"
        );
    }

    #[tokio::test]
    async fn text_format_prints_the_validated_document_not_the_fence() {
        let fixture = fixture(|| ScriptedTextProvider::new(&["```json\n{\"ok\":true}\n```"])).await;
        let options = HeadlessOptions {
            output: Some(report_contract(0)),
            format: HeadlessFormat::Text,
            ..options(&fixture.workspace)
        };

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        assert_eq!(stdout, "{\n  \"ok\": true\n}\n");
    }

    #[tokio::test]
    async fn without_a_contract_the_records_carry_no_output_fields() {
        let fixture = fixture(|| TextProvider).await;
        let (status, stdout, _stderr) = run_to_end(
            &fixture,
            options(&fixture.workspace),
            std::future::pending(),
        )
        .await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert!(records[0].get("output_schema_sha256").is_none());
        assert!(records[0].get("output_repair_turns").is_none());
        assert!(records.last().unwrap().get("final_output").is_none());
    }

    #[tokio::test]
    async fn an_empty_correlation_is_absent_from_the_trial_record() {
        let fixture = fixture(|| TextProvider).await;

        let (status, stdout, _stderr) = run_to_end(
            &fixture,
            options(&fixture.workspace),
            std::future::pending(),
        )
        .await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert_eq!(records[0]["type"], "trial");
        assert!(
            records[0].get("correlation").is_none(),
            "the default payload must not grow a field no flag asked for"
        );
    }

    #[tokio::test]
    async fn jsonl_records_have_monotonic_cursors_and_exactly_one_terminal_outcome() {
        let fixture = fixture(|| TextProvider).await;
        let options = HeadlessOptions {
            arm: Some("A1".to_owned()),
            ..options(&fixture.workspace)
        };

        let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert_eq!(records[0]["type"], "trial", "metadata must lead the trial");
        assert_eq!(
            records[0]["arm"], "A1",
            "the arm label rides the trial record"
        );
        assert_eq!(records[0]["model"]["model"], "test/model");
        assert_eq!(records[0]["profile"], "default");
        assert_eq!(records[0]["approval"], "read-only");
        assert!(records[0].get("workspace").is_none());
        assert_eq!(records[0]["workspace_identity"].as_str().unwrap().len(), 64);
        assert_eq!(records[0]["context_window"], 128_000);
        assert_eq!(records[0]["pricing_provenance"], "test fixture");
        assert_eq!(records[0]["qq_source_revision"], env!("QQ_SOURCE_REVISION"));

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
        assert_eq!(outcomes[0]["prompt_identity"]["version"], 14);
        assert!(outcomes[0]["prompt_identity"]["system_prompt_hash"].is_string());
        assert!(outcomes[0]["prompt_identity"]["tool_schema_hash"].is_string());
        assert_eq!(
            records.last().unwrap()["type"],
            "outcome",
            "the outcome must be the final record"
        );
    }

    #[test]
    fn embedded_source_revision_is_exact_and_matches_display_revision() {
        let source = env!("QQ_SOURCE_REVISION");
        let display = env!("QQ_BUILD_REVISION");
        let source = source.strip_suffix("-dirty").unwrap_or(source);
        let display = display.strip_suffix("-dirty").unwrap_or(display);

        if source == "unknown" {
            assert_eq!(display, "unknown");
            return;
        }

        assert_eq!(source.len(), 40);
        assert!(source.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(source.starts_with(display));
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
        assert!(
            !stderr.contains("To continue this session"),
            "no hint unless asked for (stderr was not a terminal): {stderr}"
        );
    }

    #[tokio::test]
    async fn the_resume_hint_names_the_session_on_stderr_and_never_touches_stdout() {
        let fixture = fixture(|| TextProvider).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;
        options.resume_hint = true;

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        assert_eq!(stdout, "hello\n", "the hint must not pollute the answer");
        let session_id = workspace_snapshot(&fixture).await.sessions[0].id;
        assert!(
            stderr.ends_with(&crate::cli::resume_hint(session_id)),
            "{stderr}"
        );
        assert!(stderr.contains(&format!("qq run --session {session_id}")));
        // Both continuations are shown: the interactive one comes first
        // because it is the one a person at a terminal wants.
        assert!(stderr.contains(&format!("\n  qq --session {session_id}\n")));
    }

    #[tokio::test]
    async fn the_resume_hint_is_printed_after_a_timed_out_run_too() {
        // A run that ended early is exactly when a person wants to continue.
        let fixture = fixture(|| HangingProvider).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;
        options.resume_hint = true;
        options.timeout = Some(Duration::from_millis(100));

        let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::TimedOut);
        let session_id = workspace_snapshot(&fixture).await.sessions[0].id;
        assert!(
            stderr.ends_with(&crate::cli::resume_hint(session_id)),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn jsonl_output_never_carries_the_resume_hint() {
        // Even if a caller sets the flag, JSONL consumers get the id from the
        // trial record and nothing else on stderr they did not ask for.
        let fixture = fixture(|| TextProvider).await;
        let mut options = options(&fixture.workspace);
        options.resume_hint = true;

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        assert!(parse_records(&stdout)[0]["session_id"].is_string());
        assert!(!stderr.contains("To continue this session"), "{stderr}");
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
                None,
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
                None,
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
                None,
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
            None,
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
                None,
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
            reasoning_tokens: None,
        };
        let inclusive = TokenUsage {
            input_tokens: 30,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 4,
            output_tokens: 9,
            reasoning_tokens: None,
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
