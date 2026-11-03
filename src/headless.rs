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
    ApprovalDecision, ApprovalMode, BudgetLimitKind, CommandId, CommandOutcome, CommandReceipt,
    ContentHash, HeadlessOutcome, HeadlessRecordRef, HeadlessTrial, InputPart, ModelSelection,
    RunId, RunLimits, RunOutcome, RunPromptIdentity, SessionAccounting, SessionCommand,
    SessionEvent, SessionEventEnvelope, SessionId, SnapshotRequest, SubscribeRequest, TokenUsage,
    ToolCallState, WorkspaceId,
};
pub use qq_protocol::{HeadlessApproval, HeadlessStatus};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
pub const MAX_PENDING_STEER_LINES: usize = 8;

const MAX_ACTIVITY_BYTES: usize = 160;

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub prompt: String,
    pub workspace: PathBuf,
    pub session: Option<SessionId>,
    pub model: ModelSelection,
    pub profile: qq_protocol::AgentProfileId,
    pub context_window: Option<u32>,
    pub pricing_provenance: Option<String>,
    pub approval: HeadlessApproval,
    pub approval_explicit: bool,
    pub timeout: Option<Duration>,
    pub max_turns: Option<u32>,
    pub max_cost_usd_nanos: Option<u64>,
    pub correlation: qq_protocol::Correlation,
    pub output: Option<Box<qq_protocol::OutputContract>>,
    pub format: HeadlessFormat,
    pub trace: Option<PathBuf>,
    pub resume_hint: bool,
    pub arm: Option<String>,
}

const fn approval_mode(approval: HeadlessApproval) -> ApprovalMode {
    match approval {
        HeadlessApproval::ReadOnly => ApprovalMode::ReadOnly,
        HeadlessApproval::Ask => ApprovalMode::Ask,
        HeadlessApproval::Auto => ApprovalMode::Auto,
        HeadlessApproval::Full => ApprovalMode::Full,
    }
}

const fn headless_approval(mode: ApprovalMode) -> HeadlessApproval {
    match mode {
        ApprovalMode::ReadOnly => HeadlessApproval::ReadOnly,
        ApprovalMode::Ask => HeadlessApproval::Ask,
        ApprovalMode::Auto => HeadlessApproval::Auto,
        ApprovalMode::Full => HeadlessApproval::Full,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessFormat {
    Text,
    Jsonl,
}

#[must_use]
pub fn exit_code(status: HeadlessStatus) -> ExitCode {
    ExitCode::from(status.code())
}

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
            let result = trace
                .write_all(line.as_bytes())
                .and_then(|()| trace.write_all(b"\n"));
            if let Err(error) = result {
                self.trace = None;
                return Err(error);
            }
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
struct SessionHandle {
    approval: HeadlessApproval,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    subscribe_after: qq_protocol::EventCursor,
}

#[derive(Clone)]
struct RunHandle {
    approval: HeadlessApproval,
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
    let _ = writeln!(io::stderr().lock(), "error: {message}");
}

struct RunEnd {
    delivery_recovered: bool,
    status: HeadlessStatus,
    message: Option<String>,
    usage: Option<TokenUsage>,
    estimated_cost_usd_nanos: Option<u64>,
    prompt_identity: Option<Box<RunPromptIdentity>>,
    audit: Option<Box<qq_protocol::AuditRecord>>,
    final_output: Option<Box<qq_protocol::FinalOutput>>,
    answer: String,
    denied_calls: usize,
}

impl RunEnd {
    fn failure(failure: Failure) -> Self {
        Self {
            delivery_recovered: false,
            status: failure.status,
            message: Some(failure.message),
            usage: None,
            estimated_cost_usd_nanos: None,
            prompt_identity: None,
            audit: None,
            final_output: None,
            answer: String::new(),
            denied_calls: 0,
        }
    }
}

pub async fn run(
    sessions: &SessionRuntime,
    options: HeadlessOptions,
    interrupt: impl Future<Output = ()>,
    steering: Option<tokio::sync::mpsc::Receiver<String>>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> HeadlessStatus {
    let mut interrupt = pin!(interrupt);
    let mut sink = match RecordSink::open(&options) {
        Ok(sink) => sink,
        Err(error) => {
            let _ = writeln!(stderr, "error: could not open the trace file: {error}");
            return HeadlessStatus::InvalidConfiguration;
        }
    };

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
        approval: handle.approval,
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
        sessions,
        &options,
        &handle,
        &mut sink,
        interrupt.as_mut(),
        steering,
        stdout,
        stderr,
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
        delivery_recovered,
        status,
        message,
        usage,
        estimated_cost_usd_nanos,
        prompt_identity,
        audit,
        final_output,
        mut answer,
        denied_calls,
    } = end;
    let mut outcome = HeadlessOutcome {
        recovered_answer: (delivery_recovered
            && (options.format == HeadlessFormat::Jsonl || options.trace.is_some()))
        .then(|| std::mem::take(&mut answer)),
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

    let answer = outcome.recovered_answer.take().unwrap_or(answer);
    if options.format == HeadlessFormat::Text {
        match status {
            HeadlessStatus::Completed => {
                let _ = writeln!(stderr);
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
        if handle.approval == HeadlessApproval::ReadOnly && denied_calls > 0 {
            let _ = writeln!(
                stderr,
                "held calls were denied under --approval read-only; rerun with --approval auto to allow workspace edits"
            );
        }
    } else if let Some(message) = &outcome.message {
        let _ = writeln!(stderr, "error: {message}");
    }
    if options.resume_hint && options.format == HeadlessFormat::Text {
        let _ = write!(stderr, "\n{}", crate::cli::resume_hint(handle.session_id));
    }

    status
}

async fn submit(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    input: Vec<qq_protocol::InputPart>,
) -> Result<RunHandle, Failure> {
    let SessionHandle {
        approval,
        workspace_id,
        session_id,
        subscribe_after,
    } = prepare_session(sessions, options).await?;
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
        approval,
        workspace_id,
        session_id,
        run_id,
        subscribe_after,
    })
}

async fn prepare_session(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
) -> Result<SessionHandle, Failure> {
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

    let (session_id, subscribe_after, approval) = match options.session {
        None => {
            let created = send(
                sessions,
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: options.model.clone(),
                    approval_mode: approval_mode(options.approval),
                    profile: options.profile.clone(),
                    reasoning_effort: None,
                    correlation: options.correlation.clone(),
                },
            )
            .await?;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                return Err(Failure::harness(
                    "session creation returned an unexpected outcome",
                ));
            };
            (session_id, created.committed_through, options.approval)
        }
        Some(session_id) => {
            let snapshot = match sessions
                .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 1, 1))
                .await
            {
                Ok(snapshot) => snapshot,
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
            let approval = if options.approval_explicit {
                options.approval
            } else {
                headless_approval(summary.approval_mode)
            };
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
            if summary.approval_mode != approval_mode(approval) {
                let receipt = send(
                    sessions,
                    SessionCommand::SetApprovalMode {
                        session_id,
                        mode: approval_mode(approval),
                    },
                )
                .await?;
                after = receipt.committed_through;
            }
            (session_id, after, approval)
        }
    };
    Ok(SessionHandle {
        approval,
        workspace_id,
        session_id,
        subscribe_after,
    })
}

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
    let mut needs_input: Option<String> = None;
    let mut shutdown_at: Option<Instant> = None;

    let mut delivery_recovered = false;
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
                    Some(Err(SessionRuntimeError::CursorExpired)) => {
                        delivery_recovered = true;
                        let snapshot = recovery_snapshot(sessions, handle.workspace_id, handle.session_id).await?;
                        events = subscribe_current(sessions, &snapshot)?;
                        if let Some(result) = sessions.run_result(handle.workspace_id, handle.session_id, handle.run_id).await
                            .map_err(|error| Failure::harness(error.to_string()))?
                        {
                            sink.record(stdout, HeadlessRecordRef::Event { envelope: &result.completion.receipt })
                                .map_err(|error| Failure::harness(error.to_string()))?;
                            return finish_run(result, needs_input.as_deref(), interrupted, delivery_recovered, text, stderr);
                        }
                        for session in sessions.approval_sessions(handle.workspace_id, handle.session_id).await.map_err(|error| Failure::harness(error.to_string()))? {
                            let body = recovery_snapshot(sessions, handle.workspace_id, session).await?.focused
                                .ok_or_else(|| Failure::harness("approval session has no snapshot body"))?;
                            for hold in &body.summary.pending_holds {
                                let call = body.tool_calls.iter().find(|call| call.id==hold.tool_call_id)
                                    .ok_or_else(|| Failure::harness("pending approval is missing its call"))?;
                                if session==handle.session_id && call.run_id!=handle.run_id { continue; }
                                let preview = body.approval_previews.iter().find(|entry| entry.tool_call_id==call.id).map(|entry| &entry.preview);
                                if let Some(question) = preview.and_then(|preview| preview.question.as_ref()) {
                                    if session==handle.session_id {
                                        if needs_input.is_none() {
                                            needs_input = Some(question.questions.first().map(|question| question.prompt.clone()).unwrap_or_default());
                                            request_cancel(sessions, handle.run_id).await?;
                                            shutdown_at = Some(Instant::now()+SHUTDOWN_GRACE);
                                        }
                                    } else {
                                        respond_approval(sessions, call.run_id, call.id, ApprovalDecision::Answer { answers: Vec::new() }, stderr).await;
                                    }
                                } else if preview.is_some() {
                                    let client = SessionHandle { approval: handle.approval, workspace_id: handle.workspace_id, session_id: handle.session_id, subscribe_after: snapshot.cursor };
                                    unattended_call(sessions, &client, call, stderr).await;
                                } else {
                                    respond_approval(sessions, call.run_id, call.id, ApprovalDecision::Deny, stderr).await;
                                }
                            }
                        }
                        continue;
                    }
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

                let ours = envelope.session_id == handle.session_id && envelope.run_id == Some(handle.run_id);
                match &envelope.event {
                    SessionEvent::RunOutputTruncated { continuation, .. } if ours => {
                        if text {
                            let _ = writeln!(
                                stderr,
                                "\n[run] output truncated; continuing ({continuation}/{})",
                                qq_core::MAX_OUTPUT_CONTINUATIONS
                            );
                        }
                    }
                    SessionEvent::TextAppended { text: chunk, .. } if ours && text => {
                        let _ = stderr.write_all(chunk.as_bytes());
                        let _ = stderr.flush();
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
                    SessionEvent::ToolApprovalRequested { tool_call, question: Some(question), .. }
                        if ours && envelope.run_id == Some(handle.run_id) =>
                    {
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
                    SessionEvent::ToolApprovalRequested { tool_call, question, .. } => {
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
                        unattended_approval(sessions, &SessionHandle {
                            approval: handle.approval,
                            workspace_id: handle.workspace_id, session_id: handle.session_id,
                            subscribe_after: handle.subscribe_after,
                        }, &envelope, stderr).await;
                    }
                    SessionEvent::RunFinished { run_id, .. } if *run_id == handle.run_id => {
                        let result = sessions.run_result(handle.workspace_id, handle.session_id, handle.run_id).await
                            .map_err(|error| Failure::harness(error.to_string()))?
                            .ok_or_else(|| Failure::harness("the terminal run has no durable result"))?;
                        return finish_run(result, needs_input.as_deref(), interrupted, delivery_recovered, text, stderr);
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

async fn recovery_snapshot(
    sessions: &SessionRuntime,
    workspace: WorkspaceId,
    session: SessionId,
) -> Result<qq_protocol::WorkspaceSnapshot, Failure> {
    sessions
        .snapshot(SnapshotRequest::new(workspace, Some(session), 1, 1))
        .await
        .map_err(|error| Failure::harness(error.to_string()))
}

fn subscribe_current(
    sessions: &SessionRuntime,
    snapshot: &qq_protocol::WorkspaceSnapshot,
) -> Result<qq_core::SessionEventStream, Failure> {
    sessions
        .subscribe(SubscribeRequest {
            workspace_id: snapshot.cursor.workspace_id,
            after: snapshot.cursor,
        })
        .map_err(|error| Failure::harness(error.to_string()))
}

fn finish_run(
    result: qq_core::RunResult,
    needs_input: Option<&str>,
    interrupted: bool,
    delivery_recovered: bool,
    text: bool,
    stderr: &mut impl Write,
) -> Result<RunEnd, Failure> {
    let SessionEvent::RunFinished {
        session,
        outcome,
        usage,
        final_output,
        ..
    } = result.completion.receipt.event
    else {
        return Err(Failure::harness("the terminal receipt has the wrong type"));
    };
    let (status, message) = match (needs_input, &outcome) {
        (Some(question), RunOutcome::Cancelled) => (
            HeadlessStatus::NeedsInput,
            Some(format!(
                "the model asked the user a question and no client could answer: {question}"
            )),
        ),
        _ => settle(&outcome, final_output.as_deref(), interrupted),
    };
    if text
        && matches!(
            status,
            HeadlessStatus::BudgetExhausted | HeadlessStatus::TimedOut
        )
    {
        let _ = writeln!(stderr, "[run] {}", status.as_str());
    }
    Ok(RunEnd {
        delivery_recovered,
        status,
        message,
        usage: inclusive_usage(session.accounting, usage),
        estimated_cost_usd_nanos: inclusive_cost(
            session.accounting,
            session.estimated_cost_usd_nanos,
        ),
        prompt_identity: result.completion.run.prompt_identity,
        audit: result.completion.run.audit,
        final_output,
        answer: result.answer,
        denied_calls: result.denied_calls,
    })
}

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
        RunOutcome::Paused { pause } => {
            (HeadlessStatus::TaskFailed, Some(format!("the run {pause}")))
        }
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
        let snapshot = recovery_snapshot(sessions, handle.workspace_id, handle.session_id).await?;
        events = subscribe_current(sessions, &snapshot)?;
        let mut check_terminal = true;
        loop {
            if check_terminal {
                if sessions
                    .run_snapshot(handle.workspace_id, handle.session_id, handle.run_id)
                    .await
                    .map_err(|error| Failure::harness(error.to_string()))?
                    .outcome
                    .is_some()
                {
                    return Ok(());
                }
                check_terminal = false;
            }
            match events.next().await {
                Some(Ok(SessionEventEnvelope {
                    event: SessionEvent::RunFinished { run_id, .. },
                    ..
                })) if run_id == handle.run_id => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(SessionRuntimeError::CursorExpired)) => {
                    let snapshot =
                        recovery_snapshot(sessions, handle.workspace_id, handle.session_id).await?;
                    events = subscribe_current(sessions, &snapshot)?;
                    check_terminal = true;
                }
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

async fn unattended_approval(
    sessions: &SessionRuntime,
    handle: &SessionHandle,
    envelope: &SessionEventEnvelope,
    stderr: &mut impl Write,
) {
    let SessionEvent::ToolApprovalRequested { tool_call, .. } = &envelope.event else {
        return;
    };
    unattended_call(sessions, handle, tool_call, stderr).await;
}

async fn unattended_call(
    sessions: &SessionRuntime,
    handle: &SessionHandle,
    tool_call: &qq_protocol::ToolCallSnapshot,
    stderr: &mut impl Write,
) {
    let approval =
        if handle.approval == HeadlessApproval::Ask || tool_call.session_id == handle.session_id {
            handle.approval
        } else {
            HeadlessApproval::Auto
        };
    let decision = match approval {
        HeadlessApproval::Full => ApprovalDecision::ApproveOnce,
        HeadlessApproval::Ask | HeadlessApproval::Auto | HeadlessApproval::ReadOnly => {
            ApprovalDecision::Deny
        }
    };
    respond_approval(sessions, tool_call.run_id, tool_call.id, decision, stderr).await;
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
mod tests;
