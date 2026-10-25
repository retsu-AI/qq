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
    ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, BudgetLimitKind, CommandId,
    CommandOutcome, CommandReceipt, ContentHash, HeadlessOutcome, HeadlessRecordRef, HeadlessTrial,
    InputPart, ModelSelection, RunId, RunLimits, RunOutcome, RunPromptIdentity, SessionAccounting,
    SessionCommand, SessionEvent, SessionEventEnvelope, SessionId, ShellCommandPreview,
    SnapshotRequest, SubscribeRequest, TokenUsage, ToolCallState, WorkspaceId,
};
pub use qq_protocol::{HeadlessApproval, HeadlessStatus};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

/// How long a cancelled run may take to reach its terminal durable event
/// before the invocation gives up and reports a harness failure.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// How long an `auto` headless run holds an escalated approval open for the
/// configured delegate before denying it. Covers the reviewer model's own
/// 10 s bound with margin. A delegate's
/// `approve` and `deny` both settle the call themselves; this deny is the
/// unattended answer to an `escalate`, a delegate timeout, or an outage, so
/// the run never stalls. Without a delegate configured the deny is immediate.
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
    /// Whether the invocation explicitly overrides a resumed session's mode.
    pub approval_explicit: bool,
    /// Whether a held call has an approval delegate that exists: a
    /// configured `reviewer_model`.
    /// Whether one is *consulted* also depends on `approval_delegate`, which
    /// the run applies with the session's own override (see
    /// `configured_delegate`). With a consulted delegate, `auto` holds an
    /// escalated call briefly so the delegate can approve it, instead of
    /// denying the moment the request is published.
    pub reviewer_configured: bool,
    /// The resolved `approval_delegate` setting (override, profile, top
    /// level). A resumed session's persisted override wins over it, exactly
    /// as the session gate applies it.
    pub configured_delegate: qq_protocol::ApprovalDelegate,
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

#[derive(Debug, Clone)]
pub struct GoalInvocation {
    pub objective: Option<String>,
    pub check: Option<String>,
    pub check_approval: Option<qq_protocol::GoalCheckScope>,
    pub deadline: Option<Duration>,
    pub max_runs: Option<u32>,
    pub replace: bool,
    pub defaults: qq_config::GoalsConfig,
}

/// The session approval mode a headless policy submits with.
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

/// The process exit for a terminal status.
#[must_use]
pub fn exit_code(status: HeadlessStatus) -> ExitCode {
    ExitCode::from(status.code())
}

enum GoalRecordState {
    Pending,
    Attempted,
    Emitted(u8),
}

/// Writes trial records to the optional trace file and, in JSONL format, to
/// stdout. Text format still traces when a trace path is given.
struct RecordSink {
    goal_record: GoalRecordState,
    stdout_failed: bool,
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
            goal_record: GoalRecordState::Pending,
            stdout_failed: false,
            trace,
            to_stdout: options.format == HeadlessFormat::Jsonl,
        })
    }

    fn record(&mut self, stdout: &mut impl Write, record: HeadlessRecordRef<'_>) -> io::Result<()> {
        if self.trace.is_none() && !self.to_stdout {
            return Ok(());
        }
        let line = serde_json::to_string(&record).map_err(io::Error::other)?;
        let final_code = match &record {
            HeadlessRecordRef::Goal(goal) => Some(goal.exit_code),
            _ => None,
        };
        if self.to_stdout
            && let Some(code) = final_code
        {
            if let Err(error) = stdout
                .write_all(line.as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush())
            {
                self.stdout_failed = true;
                return Err(error);
            }
            self.goal_record = GoalRecordState::Emitted(code);
        }
        if let Some(trace) = &mut self.trace {
            let result = trace
                .write_all(line.as_bytes())
                .and_then(|()| trace.write_all(b"\n"));
            if let Err(error) = result {
                // Keep a failed optional trace from blocking the final stdout
                // record after an earlier event could not be traced.
                self.trace = None;
                return Err(error);
            }
        }
        if self.to_stdout
            && final_code.is_none()
            && let Err(error) = stdout
                .write_all(line.as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush())
        {
            self.stdout_failed = true;
            return Err(error);
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

#[derive(Clone, Copy)]
enum SessionIntent<'a> {
    Prompt,
    Goal(&'a GoalInvocation),
}

#[derive(Clone)]
struct SessionHandle {
    approval: HeadlessApproval,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    subscribe_after: qq_protocol::EventCursor,
    wait_for_delegate: bool,
    child_waits_for_delegate: bool,
}

#[derive(Clone)]
struct RunHandle {
    approval: HeadlessApproval,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    subscribe_after: qq_protocol::EventCursor,
    /// A held root call under `auto` waits for a delegate rather than being
    /// denied at once (see `submit`).
    wait_for_delegate: bool,
    /// The same for an owned child's held call (supervised mode).
    child_waits_for_delegate: bool,
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
    delivery_recovered: bool,
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
    /// Tool calls of this run that settled as denied. Under `read-only`
    /// that is every held call, which is what the closing hint explains.
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

/// Pure admission preparation runs before persisted session settings change.
fn prepare_goal_request(
    request: &GoalInvocation,
    options: &HeadlessOptions,
    session_id: SessionId,
    current: Option<&qq_protocol::GoalSnapshot>,
) -> Result<Option<SessionCommand>, Failure> {
    let command = if let Some(objective) = &request.objective {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| Failure::invalid(error.to_string()))?;
        let args = crate::cli::GoalSetArgs {
            target: crate::cli::GoalTarget {
                session: session_id,
                workspace: None,
                json: true,
            },
            objective: objective.clone(),
            replace: request.replace,
            check: request.check.clone(),
            clear_check: false,
            goal_deadline: request.deadline,
            goal_max_runs: request.max_runs,
            max_cost_usd_nanos: options.max_cost_usd_nanos,
        };
        let mut command = crate::goals::prepare_set(
            &args,
            current,
            &request.defaults,
            u64::try_from(now.as_millis())
                .map_err(|_| Failure::invalid("system clock exceeds supported range"))?,
        )
        .map_err(|error| Failure::invalid(error.to_string()))?;
        if let SessionCommand::SetGoal {
            budget,
            check_approval,
            ..
        } = &mut command
        {
            *check_approval = request.check_approval;
            if let Some(timeout) = options.timeout {
                budget.per_run.max_duration_ms = Some(
                    u64::try_from(timeout.as_millis())
                        .map_err(|_| Failure::invalid("per-run timeout is too large"))?,
                );
            }
            if let Some(turns) = options.max_turns {
                budget.per_run.max_model_turns = Some(turns);
            }
        }
        Some(command)
    } else {
        if current.is_none() {
            return Err(Failure::invalid("session has no stored goal"));
        }
        None
    };
    let check = match &command {
        Some(SessionCommand::SetGoal { check, .. }) => check.as_ref(),
        _ => current.and_then(|goal| goal.check.as_ref()),
    };
    if request.check_approval.is_some() {
        let check = check.ok_or_else(|| Failure::invalid("goal has no check"))?;
        if request.check.as_deref() != Some(check.command.as_str()) {
            return Err(Failure::invalid(
                "check consent requires --check with the exact stored command",
            ));
        }
    } else if request.objective.is_none() && request.check.is_some() {
        return Err(Failure::invalid(
            "stored check changes require --goal OBJECTIVE",
        ));
    }
    Ok(command)
}

/// Hosts the core goal driver until its goal leaves Active. No prompt is
/// resubmitted by this adapter; scheduling and spending remain runtime-owned.
pub async fn run_goal(
    sessions: &SessionRuntime,
    options: HeadlessOptions,
    request: GoalInvocation,
    interrupt: impl Future<Output = ()>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let mut interrupt = pin!(interrupt);
    let mut cleanup = None;
    let mut accepted: Option<(WorkspaceId, Box<qq_protocol::GoalSnapshot>)> = None;
    let mut record_sink = None;
    let mut direct_record_cursor = None;
    let result: Result<u8, Failure> = async {
        let sink = record_sink.insert(RecordSink::open(&options)
            .map_err(|error| Failure::invalid(format!("could not open trace: {error}")))?);
        let handle = prepare_session_with_interrupt(sessions, &options, SessionIntent::Goal(&request), interrupt.as_mut()).await?;
        direct_record_cursor = Some(handle.subscribe_after);
        let snapshot = sessions.snapshot(SnapshotRequest::new(
            handle.workspace_id, Some(handle.session_id), 1, 1,
        )).await.map_err(|error| Failure::harness(error.to_string()))?;
        let current = snapshot.focused.and_then(|session| session.goal);
        let command = prepare_goal_request(&request, &options, handle.session_id, current.as_deref())?;
        let created = command.is_some();
        let mut goal = match command {
            Some(command) => goal_receipt(send(sessions, command).await?)?,
            None => current.ok_or_else(|| Failure::invalid("session has no stored goal"))?,
        };
        accepted = Some((handle.workspace_id, goal.clone()));
        cleanup = Some(AcceptedGoalGuard::new(sessions.clone(), &goal, &handle));
        let guard = cleanup.as_mut().unwrap();
        if let Some(scope) = request.check_approval.filter(|_| !created) {
            let check = goal.check.as_ref().unwrap();
            goal = goal_receipt(send(sessions, SessionCommand::GoalControl {
                session_id: handle.session_id, expected: goal_identity(&goal),
                action: qq_protocol::GoalControl::ApproveCheck { scope, command: check.command.clone() },
            }).await?)?;
            guard.expected = goal_identity(&goal);
            guard.cleanup = (goal.status == qq_protocol::GoalStatus::Active)
                .then(|| (sessions.clone(), goal.session_id));
        }
        // Missing consent is a durable result, not an unreachable human wait.
        let needs_check = matches!(goal.status, qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::NeedsUser {
                cause: qq_protocol::GoalNeedsUser::CheckApproval | qq_protocol::GoalNeedsUser::CheckInterrupted,
            },
        });
        if !needs_check && matches!(goal.status,
            qq_protocol::GoalStatus::Paused { .. } | qq_protocol::GoalStatus::Blocked { .. }) {
            goal = goal_receipt(send(sessions, SessionCommand::GoalControl {
                session_id: handle.session_id, expected: goal_identity(&goal),
                action: qq_protocol::GoalControl::Resume,
            }).await?)?;
            guard.expected = goal_identity(&goal);
            guard.cleanup = (goal.status == qq_protocol::GoalStatus::Active)
                .then(|| (sessions.clone(), goal.session_id));
        }
        let mut expected = goal_identity(&goal);
        let stopped = if goal.status != qq_protocol::GoalStatus::Active {
            Some(settle_goal(sessions, &handle, expected).await?)
        } else { None };
        let mut events = sessions.subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id, after: handle.subscribe_after,
        }).map_err(|error| Failure::harness(error.to_string()))?;
        let mut interrupt_armed = stopped.is_none();
        let mut final_goal = stopped.map(|(goal, through)| (goal, through.sequence, None));
        let mut observed = handle.subscribe_after.sequence;
        loop {
            if final_goal.as_ref().is_some_and(|(_, through, _)| observed >= *through) {
                let (goal, _, forced) = final_goal.take().unwrap();
                guard.disarm();
                return finish_goal(sink, stdout, &options, handle.workspace_id, goal, forced);
            }
            tokio::select! {
                biased;
                () = interrupt.as_mut(), if interrupt_armed => {
                    interrupt_armed = false;
                    let paused = sessions.pause_goal_if_active(handle.session_id, expected).await;
                    let (paused_goal, interrupted) = match paused {
                        Ok(receipt) => {
                            let goal = goal_receipt(receipt)?;
                            // A stopped no-op retains its revision; only our pause advances it.
                            let interrupted = goal.revision > expected.revision
                                && goal.status == qq_protocol::GoalStatus::Paused {
                                    reason: qq_protocol::GoalPauseReason::User,
                                };
                            (goal, interrupted)
                        }
                        Err(error) => {
                            let current = sessions.snapshot(SnapshotRequest::new(handle.workspace_id, Some(handle.session_id), 1, 1)).await
                                .map_err(|error| Failure::harness(error.to_string()))?
                                .focused.and_then(|session| session.goal)
                                .filter(|goal| goal.goal_id == expected.goal_id && goal.status != qq_protocol::GoalStatus::Active);
                            match current { Some(goal) => (goal, false), None => return Err(Failure::harness(error.to_string())) }
                        }
                    };
                    goal = paused_goal;
                    guard.expected = goal_identity(&goal);
                    let (goal, through) = settle_goal(sessions, &handle, guard.expected).await?;
                    final_goal = Some((goal, through.sequence, interrupted.then_some(qq_protocol::HeadlessGoalStatus::Interrupted)));
                }
                event = events.next() => {
                    let envelope = match event {
                        Some(Ok(envelope)) => envelope,
                        Some(Err(SessionRuntimeError::CursorExpired)) => {
                            let snapshot = recovery_snapshot(sessions, handle.workspace_id, handle.session_id).await?;
                            events = subscribe_current(sessions, &snapshot)?;
                            replay_settled_goal_runs(sessions, sink, stdout, &handle, expected,
                                direct_record_cursor.unwrap_or(handle.subscribe_after), snapshot.cursor).await?;
                            direct_record_cursor = Some(snapshot.cursor);
                            observed = snapshot.cursor.sequence;
                            goal = snapshot.focused.and_then(|body| body.goal)
                                .filter(|goal| goal.goal_id==expected.goal_id && goal.revision>=expected.revision)
                                .ok_or_else(|| Failure::harness("goal changed while recovering delivery history"))?;
                            expected = goal_identity(&goal);
                            guard.expected = expected;
                            accepted = Some((handle.workspace_id, goal.clone()));
                            if goal.status!=qq_protocol::GoalStatus::Active {
                                interrupt_armed = false;
                                let (settled, through) = settle_goal(sessions, &handle, expected).await?;
                                let forced = final_goal.as_ref().and_then(|(_, _, forced)| *forced);
                                final_goal = Some((settled, through.sequence, forced));
                            }
                            if final_goal.is_none() {
                                for session in sessions.approval_sessions(handle.workspace_id, handle.session_id).await.map_err(|error| Failure::harness(error.to_string()))? {
                                    let body = recovery_snapshot(sessions, handle.workspace_id, session).await?.focused
                                        .ok_or_else(|| Failure::harness("approval session has no snapshot body"))?;
                                    for hold in &body.summary.pending_holds {
                                        if hold.delegated { continue; }
                                        let call = body.tool_calls.iter().find(|call| call.id==hold.tool_call_id)
                                            .ok_or_else(|| Failure::harness("pending approval is missing its call"))?;
                                        if !sessions.goal_owns_run(handle.session_id, expected, call.run_id).await.map_err(|error| Failure::harness(error.to_string()))? { continue; }
                                        let preview = body.approval_previews.iter().find(|entry| entry.tool_call_id==call.id).map(|entry| &entry.preview);
                                        if preview.is_some_and(|preview| preview.question.is_some()) {
                                            let id = CommandId::generate().map_err(|error| Failure::harness(error.to_string()))?;
                                            let receipt = match sessions.command(id, SessionCommand::GoalControl { session_id: handle.session_id, expected,
                                                action: qq_protocol::GoalControl::PauseForInput { tool_call_id: call.id } }).await {
                                                Ok(receipt) => receipt,
                                                Err(SessionRuntimeError::GoalInputNotPending) => continue,
                                                Err(error) => return Err(Failure::harness(error.to_string())),
                                            };
                                            goal = goal_receipt(receipt)?;
                                            expected = goal_identity(&goal);
                                            guard.expected = expected;
                                            interrupt_armed = false;
                                            let (settled, through) = settle_goal(sessions, &handle, expected).await?;
                                            final_goal = Some((settled, through.sequence, None));
                                            break;
                                        } else if let Some(preview) = preview {
                                            let client = SessionHandle { approval: handle.approval, workspace_id: handle.workspace_id, session_id: handle.session_id,
                                                subscribe_after: snapshot.cursor, wait_for_delegate: false, child_waits_for_delegate: false };
                                            unattended_call(sessions, &options, &client, call, preview.shell.as_ref(), preview.fetch.as_ref(), stderr).await;
                                        } else {
                                            respond_approval(sessions, call.run_id, call.id, ApprovalDecision::Deny, stderr).await;
                                        }
                                    }
                                    if final_goal.is_some() { break; }
                                }
                            }
                            continue;
                        }
                        Some(Err(error)) => return Err(Failure::harness(error.to_string())),
                        None => return Err(Failure::harness("goal event stream ended")),
                    };
                    // Cleanup owns this durable revision even if its output fails.
                    if let SessionEvent::GoalUpdated { goal: updated } = &envelope.event
                        && updated.session_id == handle.session_id
                        && updated.goal_id == expected.goal_id
                        && updated.revision >= expected.revision
                    {
                        accepted = Some((handle.workspace_id, updated.clone()));
                        expected = goal_identity(updated);
                        guard.expected = expected;
                    }
                    sink.record(stdout, HeadlessRecordRef::Event { envelope: &envelope })
                        .map_err(|error| Failure::harness(error.to_string()))?;
                    observed = envelope.cursor.sequence;
                    match &envelope.event {
                        SessionEvent::GoalUpdated { goal: updated }
                            if updated.session_id == handle.session_id
                            && updated.goal_id == expected.goal_id
                            && updated.revision >= expected.revision => {
                            if updated.status != qq_protocol::GoalStatus::Active && final_goal.is_none() {
                                interrupt_armed = false;
                                guard.expected = goal_identity(updated);
                                let (goal, through) = settle_goal(sessions, &handle, guard.expected).await?;
                                final_goal = Some((goal, through.sequence, None));
                            }
                        }
                        SessionEvent::RunFinished { origin: Some(origin), run_id, .. }
                            if envelope.session_id == handle.session_id && matches!(origin,
                                qq_protocol::RunOrigin::Goal { goal_id, .. }
                                | qq_protocol::RunOrigin::GoalCheck { goal_id, .. }
                                if *goal_id == expected.goal_id) => {
                            record_settled_goal_run(sessions, sink, stdout, &handle, *run_id).await?;
                            direct_record_cursor = Some(envelope.cursor);
                        }
                        SessionEvent::ToolApprovalRequested { .. } if final_goal.is_some() => {}
                        SessionEvent::ToolApprovalRequested { .. }
                            if match envelope.run_id {
                                Some(run) => !sessions.goal_owns_run(handle.session_id, expected, run).await
                                    .map_err(|error| Failure::harness(error.to_string()))?,
                                None => true,
                            } => {}
                        SessionEvent::ToolApprovalEscalated { tool_call_id, .. }
                            if final_goal.is_none() => {
                            if let Some(run_id) = envelope.run_id
                                && sessions.goal_owns_run(handle.session_id, expected, run_id).await
                                    .map_err(|error| Failure::harness(error.to_string()))?
                            {
                                // A recovered delegated hold did not arm the
                                // live request's deferred deny. Once the reviewer
                                // passes or times out, supply the unattended answer.
                                respond_approval(sessions, run_id, *tool_call_id,
                                    ApprovalDecision::Deny, stderr).await;
                            }
                        }
                        SessionEvent::ToolApprovalRequested { tool_call, question: Some(_), .. } => {
                            // The core validates held-question ownership before stopping.
                            let id = CommandId::generate()
                                .map_err(|error| Failure::harness(error.to_string()))?;
                            let receipt = match sessions.command(id, SessionCommand::GoalControl {
                                session_id: handle.session_id, expected,
                                action: qq_protocol::GoalControl::PauseForInput { tool_call_id: tool_call.id },
                            }).await {
                                Ok(receipt) => receipt,
                                Err(SessionRuntimeError::GoalInputNotPending) => continue,
                                Err(error) => return Err(Failure::harness(error.to_string())),
                            };
                            goal = goal_receipt(receipt)?;
                            guard.expected = goal_identity(&goal);
                            interrupt_armed = false;
                            let (goal, through) = settle_goal(sessions, &handle, guard.expected).await?;
                            final_goal = Some((goal, through.sequence, None));
                        }
                        SessionEvent::ToolApprovalRequested { .. } => {
                            unattended_approval(sessions, &options, &handle, &envelope, stderr).await;
                        }
                        SessionEvent::TextAppended { text, .. }
                            if options.format == HeadlessFormat::Text
                                && envelope.session_id == handle.session_id => {
                            stderr.write_all(text.as_bytes()).and_then(|()| stderr.flush())
                                .map_err(|error| Failure::harness(error.to_string()))?;
                        }
                        _ => {}
                    }
                }
            }
        }
    }.await;
    match result {
        Ok(code) => code,
        Err(failure) => {
            let _ = writeln!(stderr, "error: {}", failure.message);
            let mut code = failure.status.code();
            if let Some(guard) = &mut cleanup {
                match guard.settle().await {
                    Ok(Some((goal, through))) => {
                        if let Some(sink) = record_sink.as_mut()
                            && matches!(sink.goal_record, GoalRecordState::Pending)
                            && !sink.stdout_failed
                            && let Some(after) = direct_record_cursor
                            && let Err(error) = replay_settled_goal_runs(
                                sessions,
                                sink,
                                stdout,
                                &guard.handle,
                                goal_identity(&goal),
                                after,
                                through,
                            )
                            .await
                        {
                            let _ = writeln!(
                                stderr,
                                "error: goal result replay failed: {}",
                                error.message
                            );
                        }
                        accepted = Some((guard.handle.workspace_id, goal));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = writeln!(stderr, "error: goal cleanup failed: {}", error.message);
                        code = HeadlessStatus::HarnessFailure.code();
                    }
                }
            }
            if let Some(RecordSink {
                goal_record: GoalRecordState::Emitted(emitted),
                ..
            }) = &record_sink
            {
                return *emitted;
            }
            if code == HeadlessStatus::HarnessFailure.code()
                && let Some((workspace, mut goal)) = accepted
                && let Some(sink) = record_sink.as_mut()
                && matches!(sink.goal_record, GoalRecordState::Pending)
                && !sink.stdout_failed
            {
                // Prefer authoritative state when readable; otherwise retain
                // the last observed goal without inventing a durable outcome.
                if let Ok(snapshot) = sessions
                    .snapshot(SnapshotRequest::new(workspace, Some(goal.session_id), 1, 1))
                    .await
                    && let Some(current) = snapshot.focused.and_then(|session| session.goal)
                    && current.goal_id == goal.goal_id
                {
                    goal = current;
                }
                if let Err(error) = finish_goal(
                    sink,
                    stdout,
                    &options,
                    workspace,
                    goal,
                    Some(qq_protocol::HeadlessGoalStatus::HarnessFailure),
                ) {
                    let _ = writeln!(stderr, "error: final goal record failed: {}", error.message);
                }
            }
            code
        }
    }
}

async fn record_settled_goal_run(
    sessions: &SessionRuntime,
    sink: &mut RecordSink,
    stdout: &mut impl Write,
    handle: &SessionHandle,
    run_id: RunId,
) -> Result<(), Failure> {
    let run = sessions
        .run_snapshot(handle.workspace_id, handle.session_id, run_id)
        .await
        .map_err(|error| Failure::harness(error.to_string()))?;
    record_goal_run(sink, stdout, handle, run)
}

fn record_goal_run(
    sink: &mut RecordSink,
    stdout: &mut impl Write,
    handle: &SessionHandle,
    run: qq_protocol::RunSnapshot,
) -> Result<(), Failure> {
    let goal = match &run.origin {
        Some(
            qq_protocol::RunOrigin::Goal { goal_id, revision }
            | qq_protocol::RunOrigin::GoalCheck { goal_id, revision },
        ) => qq_protocol::GoalIdentity {
            goal_id: *goal_id,
            revision: *revision,
        },
        _ => return Err(Failure::harness("goal run has no durable goal origin")),
    };
    let record = qq_protocol::HeadlessGoalRun {
        workspace_id: handle.workspace_id,
        session_id: handle.session_id,
        goal,
        run: Box::new(run),
    };
    sink.record(stdout, HeadlessRecordRef::GoalRun(&record))
        .map_err(|error| Failure::harness(error.to_string()))
}

async fn replay_settled_goal_runs(
    sessions: &SessionRuntime,
    sink: &mut RecordSink,
    stdout: &mut impl Write,
    handle: &SessionHandle,
    expected: qq_protocol::GoalIdentity,
    after: qq_protocol::EventCursor,
    through: qq_protocol::EventCursor,
) -> Result<(), Failure> {
    tokio::time::timeout(SHUTDOWN_GRACE, async {
        if after.sequence >= through.sequence {
            return Ok(());
        }
        let mut cursor = after;
        loop {
            let page = sessions
                .completed_runs(handle.session_id, cursor, through)
                .await
                .map_err(|error| Failure::harness(error.to_string()))?;
            if page.is_empty() {
                return Ok(());
            }
            for completion in page {
                cursor = completion.receipt.cursor;
                if matches!(&completion.run.origin,
                    Some(qq_protocol::RunOrigin::Goal { goal_id, .. }
                        | qq_protocol::RunOrigin::GoalCheck { goal_id, .. })
                    if *goal_id == expected.goal_id)
                {
                    record_goal_run(sink, stdout, handle, completion.run)?;
                }
            }
        }
    })
    .await
    .map_err(|_| Failure::harness("goal result replay exceeded the shutdown period"))?
}

/// Once stopped, cancel and drain only this goal's paid family. The single
/// deadline bounds the entire cleanup, including late admitted descendants.
async fn settle_goal(
    sessions: &SessionRuntime,
    handle: &SessionHandle,
    expected: qq_protocol::GoalIdentity,
) -> Result<(Box<qq_protocol::GoalSnapshot>, qq_protocol::EventCursor), Failure> {
    let mut events = sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .map_err(|error| Failure::harness(error.to_string()))?;
    tokio::time::timeout(SHUTDOWN_GRACE, async {
        let snapshot = recovery_snapshot(sessions, handle.workspace_id, handle.session_id).await?;
        events = subscribe_current(sessions, &snapshot)?;
        loop {
            let (pending, cancel) = match sessions
                .goal_pending_runs(handle.session_id, expected)
                .await
            {
                Ok(pending) => (pending, true),
                Err(
                    SessionRuntimeError::GoalNotFound | SessionRuntimeError::GoalConflict { .. },
                ) => {
                    // Clear/replacement revokes the live identity, not the old
                    // family's cancellation obligation. Observe only work the
                    // core already cancelled; never cancel a replacement run.
                    (
                        sessions
                            .cancelled_goal_runs(handle.session_id, expected.goal_id)
                            .await
                            .map_err(|error| Failure::harness(error.to_string()))?,
                        false,
                    )
                }
                Err(error) => return Err(Failure::harness(error.to_string())),
            };
            if pending.is_empty() {
                break;
            }
            if cancel {
                for run in &pending {
                    request_cancel(sessions, *run).await?;
                }
            }
            let mut pending: std::collections::HashSet<_> = pending.into_iter().collect();
            while !pending.is_empty() {
                let event = match events.next().await {
                    Some(Ok(event)) => event,
                    Some(Err(SessionRuntimeError::CursorExpired)) => {
                        let snapshot =
                            recovery_snapshot(sessions, handle.workspace_id, handle.session_id)
                                .await?;
                        events = subscribe_current(sessions, &snapshot)?;
                        break;
                    }
                    Some(Err(error)) => return Err(Failure::harness(error.to_string())),
                    None => return Err(Failure::harness("goal drain stream ended")),
                };
                if let SessionEvent::RunFinished { run_id, .. } = event.event {
                    pending.remove(&run_id);
                }
            }
        }
        let snapshot = sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .map_err(|error| Failure::harness(error.to_string()))?;
        let goal = snapshot
            .focused
            .and_then(|session| session.goal)
            .filter(|goal| {
                goal_identity(goal) == expected && goal.status != qq_protocol::GoalStatus::Active
            })
            .ok_or_else(|| Failure::harness("goal changed while terminal work drained"))?;
        Ok((goal, snapshot.cursor))
    })
    .await
    .map_err(|_| Failure::harness("goal cancellation did not drain within the shutdown period"))?
}

fn goal_identity(goal: &qq_protocol::GoalSnapshot) -> qq_protocol::GoalIdentity {
    qq_protocol::GoalIdentity {
        goal_id: goal.goal_id,
        revision: goal.revision,
    }
}

fn goal_receipt(receipt: CommandReceipt) -> Result<Box<qq_protocol::GoalSnapshot>, Failure> {
    match receipt.outcome {
        CommandOutcome::GoalUpdated { goal, .. } => Ok(goal),
        _ => Err(Failure::harness(
            "goal command returned an unexpected receipt",
        )),
    }
}

fn finish_goal(
    sink: &mut RecordSink,
    stdout: &mut impl Write,
    options: &HeadlessOptions,
    workspace_id: WorkspaceId,
    goal: Box<qq_protocol::GoalSnapshot>,
    forced: Option<qq_protocol::HeadlessGoalStatus>,
) -> Result<u8, Failure> {
    let status = forced
        .or_else(|| qq_protocol::HeadlessGoalStatus::from_goal(&goal.status))
        .ok_or_else(|| Failure::harness("cannot finish an active goal"))?;
    let record = qq_protocol::HeadlessGoalOutcome {
        workspace_id,
        goal,
        status,
        exit_code: status.code(),
    };
    if !matches!(sink.goal_record, GoalRecordState::Pending) {
        return Err(Failure::harness("final goal output was already attempted"));
    }
    // A failed write can have emitted a prefix; never retry it. Once stdout
    // is flushed, its selected exit remains authoritative despite trace errors.
    sink.goal_record = GoalRecordState::Attempted;
    if options.format == HeadlessFormat::Text {
        writeln!(
            stdout,
            "goal {}: {:?}",
            record.goal.goal_id, record.goal.status
        )
        .and_then(|()| stdout.flush())
        .map_err(|error| Failure::harness(error.to_string()))?;
        sink.goal_record = GoalRecordState::Emitted(status.code());
    }
    sink.record(stdout, HeadlessRecordRef::Goal(&record))
        .map_err(|error| Failure::harness(error.to_string()))?;
    if options.format == HeadlessFormat::Jsonl {
        sink.goal_record = GoalRecordState::Emitted(status.code());
    }
    sink.finish()
        .map_err(|error| Failure::harness(error.to_string()))?;
    Ok(status.code())
}

struct AcceptedGoalGuard {
    handle: SessionHandle,
    cleanup: Option<(SessionRuntime, SessionId)>,
    expected: qq_protocol::GoalIdentity,
}
impl AcceptedGoalGuard {
    fn new(
        sessions: SessionRuntime,
        goal: &qq_protocol::GoalSnapshot,
        handle: &SessionHandle,
    ) -> Self {
        Self {
            handle: handle.clone(),
            cleanup: (goal.status == qq_protocol::GoalStatus::Active)
                .then_some((sessions, goal.session_id)),
            expected: goal_identity(goal),
        }
    }
    async fn settle(
        &mut self,
    ) -> Result<Option<(Box<qq_protocol::GoalSnapshot>, qq_protocol::EventCursor)>, Failure> {
        if let Some((sessions, session_id)) = &self.cleanup {
            let receipt = match sessions
                .pause_goal_if_active(*session_id, self.expected)
                .await
            {
                Ok(receipt) => receipt,
                Err(
                    SessionRuntimeError::GoalNotFound | SessionRuntimeError::GoalConflict { .. },
                ) => {
                    let drained = settle_goal(sessions, &self.handle, self.expected).await;
                    self.disarm();
                    return drained.map(Some);
                }
                Err(error) => return Err(Failure::harness(error.to_string())),
            };
            let goal = goal_receipt(receipt)?;
            self.expected = goal_identity(&goal);
            let (goal, through) = settle_goal(sessions, &self.handle, self.expected).await?;
            self.disarm();
            return Ok(Some((goal, through)));
        }
        Ok(None)
    }
    fn disarm(&mut self) {
        self.cleanup = None;
    }
}
impl Drop for AcceptedGoalGuard {
    fn drop(&mut self) {
        if let Some((sessions, session_id)) = self.cleanup.take() {
            let expected = self.expected;
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                report_detached_cleanup_failure("accepted goal cleanup requires a Tokio runtime");
                return;
            };
            drop(runtime.spawn(async move {
                if let Err(error) = sessions.pause_goal_if_active(session_id, expected).await {
                    report_detached_cleanup_failure(&format!(
                        "accepted goal cleanup failed: {}",
                        error
                    ));
                }
            }));
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
    let mut interrupt = pin!(interrupt);
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
    let handle = match submit(sessions, &options, input, interrupt.as_mut()).await {
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
        // Explain read-only denials and name the mode allowing workspace
        // edits. JSONL consumers already see each denied call's state.
        if handle.approval == HeadlessApproval::ReadOnly && denied_calls > 0 {
            let _ = writeln!(
                stderr,
                "held calls were denied under --approval read-only; rerun with --approval auto to allow workspace edits"
            );
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
    interrupt: impl Future<Output = ()>,
) -> Result<RunHandle, Failure> {
    let SessionHandle {
        approval,
        workspace_id,
        session_id,
        subscribe_after,
        wait_for_delegate,
        child_waits_for_delegate,
    } = prepare_session_with_interrupt(sessions, options, SessionIntent::Prompt, interrupt).await?;
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
        approval,
        workspace_id,
        session_id,
        run_id,
        // Subscribing from the cursor just before submission replays the
        // queued prompt and everything after it, so no event is lost to the
        // gap between submission and subscription. For a resumed session that
        // is the snapshot cursor (or the last settings write), not the
        // session's history.
        subscribe_after,
        wait_for_delegate,
        child_waits_for_delegate,
    })
}

#[cfg(test)]
async fn prepare_session(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    intent: SessionIntent<'_>,
) -> Result<SessionHandle, Failure> {
    prepare_session_with_interrupt(sessions, options, intent, std::future::pending()).await
}

async fn prepare_session_with_interrupt(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    intent: SessionIntent<'_>,
    interrupt: impl Future<Output = ()>,
) -> Result<SessionHandle, Failure> {
    let mut interrupt = pin!(interrupt);
    if options.session.is_none()
        && let SessionIntent::Goal(request) = intent
    {
        let validation_target =
            SessionId::generate().map_err(|error| Failure::harness(error.to_string()))?;
        prepare_goal_request(request, options, validation_target, None)?;
    }
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

    let (session_id, subscribe_after, session_delegate, approval) = match options.session {
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
            (
                session_id,
                created.committed_through,
                None,
                options.approval,
            )
        }
        Some(session_id) => {
            let mut snapshot = match sessions
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
            if let Some(run_id) = snapshot
                .focused
                .as_ref()
                .and_then(|focused| focused.summary.active_run_id)
                && sessions.is_recovered_run(run_id)
            {
                tokio::select! {
                    biased;
                    () = interrupt.as_mut() => {
                        if let Some(goal) = snapshot.focused.as_ref().and_then(|focused| focused.goal.as_ref())
                            && sessions.goal_owns_run(session_id, goal_identity(goal), run_id).await.map_err(|error| Failure::harness(error.to_string()))?
                        {
                            sessions.pause_goal_if_active(session_id, goal_identity(goal)).await.map_err(|error| Failure::harness(error.to_string()))?;
                        }
                        let handle = RunHandle {
                            approval: options.approval, workspace_id, session_id, run_id,
                            subscribe_after: snapshot.cursor, wait_for_delegate: false, child_waits_for_delegate: false,
                        };
                        cancel_and_settle(sessions, &handle).await?;
                        return Err(Failure { status: HeadlessStatus::Interrupted, message: "interrupted while waiting for inherited commands".to_owned() });
                    }
                    recovered = sessions.await_recovered_commands(workspace_id, session_id) => {
                        recovered.map_err(|error| Failure::harness(error.to_string()))?;
                    }
                }
                snapshot = sessions
                    .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 1, 1))
                    .await
                    .map_err(|error| Failure::harness(error.to_string()))?;
            }
            let Some(focused) = snapshot.focused else {
                return Err(Failure::harness(
                    "workspace snapshot omitted the focused session",
                ));
            };
            if let SessionIntent::Goal(request) = intent {
                prepare_goal_request(request, options, session_id, focused.goal.as_deref())?;
            }
            let active_goal = focused
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == qq_protocol::GoalStatus::Active);
            if matches!(intent, SessionIntent::Goal(_))
                && let Some(goal) = &focused.goal
                && sessions
                    .has_preempting_goal_work(session_id, goal.goal_id)
                    .await
                    .map_err(|error| Failure::harness(error.to_string()))?
            {
                return Err(Failure::invalid(
                    "session has preempting user work; settle it with an interactive client before resuming the goal",
                ));
            }
            let summary = focused.summary;
            if summary.parent_id.is_some() {
                return Err(Failure::invalid(format!(
                    "session {session_id} is a spawned sub-agent session; \
                     resume its root session instead"
                )));
            }
            if (summary.status != qq_protocol::SessionStatus::Idle
                || summary.active_run_id.is_some()
                || summary.queued_prompts > 0)
                && !(matches!(intent, SessionIntent::Goal(_)) && active_goal)
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
            // Preserve the session's approval policy unless the operator
            // explicitly selects another mode. The model selection is always
            // written (the summary omits its output cap and organization).
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
            (
                session_id,
                if matches!(intent, SessionIntent::Goal(_)) {
                    sessions
                        .startup_goal_cursor(workspace_id)
                        .await
                        .map_err(|error| Failure::harness(error.to_string()))?
                        .unwrap_or(snapshot.cursor)
                } else {
                    after
                },
                summary.approval_delegate,
                approval,
            )
        }
    };
    if matches!(approval, HeadlessApproval::Auto | HeadlessApproval::Full) {
        sessions
            .seed_session_grants(
                session_id,
                qq_core::WorkspaceGrantSeed {
                    tools: options.allow_tools.clone(),
                    shell_prefixes: options.allow_shell_prefixes.clone(),
                    hosts: options.allow_hosts.clone(),
                },
            )
            .await
            .map_err(|error| Failure::harness(error.to_string()))?;
    }
    // Whether a held call waits for a delegate: one exists and the effective
    // delegate setting (the session's override, else the configured one)
    // consults it under the call's approval mode. Without that, waiting only
    // delays a denial nobody else will issue. Owned children copy the root
    // session's override at spawn and run `supervised`, so their check uses
    // the same setting under that mode.
    let effective_delegate = session_delegate.unwrap_or(options.configured_delegate);
    let wait_for_delegate = options.reviewer_configured
        && effective_delegate.consults_reviewer(approval_mode(approval));
    let child_waits_for_delegate = approval != HeadlessApproval::Ask
        && options.reviewer_configured
        && effective_delegate.consults_reviewer(qq_protocol::ApprovalMode::Auto);

    Ok(SessionHandle {
        approval,
        workspace_id,
        session_id,
        subscribe_after,
        wait_for_delegate,
        child_waits_for_delegate,
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
                                if hold.delegated { continue; }
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
                                } else if let Some(preview) = preview {
                                    let client = SessionHandle { approval: handle.approval, workspace_id: handle.workspace_id, session_id: handle.session_id, subscribe_after: snapshot.cursor, wait_for_delegate: false, child_waits_for_delegate: false };
                                    unattended_call(sessions, options, &client, call, preview.shell.as_ref(), preview.fetch.as_ref(), stderr).await;
                                } else {
                                    // Missing legacy inputs convey no authority.
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
                    // A delegate settled a held call: say which one, so the
                    // operator reading the log can tell the reviewer model's
                    // decision from their own allowlist.
                    SessionEvent::ToolApprovalResolved {
                        tool_call,
                        resolution:
                            resolution @ (ApprovalResolution::ApprovedByReviewer
                            | ApprovalResolution::DeniedByReviewer),
                        delegate,
                    } if ours && text => {
                        let _ = write_delegate_resolution(
                            stderr,
                            &tool_call.name,
                            *resolution,
                            *delegate,
                        );
                    }
                    // The delegate passed: say why, so the unattended deny that
                    // follows (or the allowlist answer) reads as a consequence.
                    // Unattended `auto` has no human to wait for, so the
                    // escalation is answered at once instead of at the end
                    // of the reviewer grace. Under `ask` a real client owns
                    // the prompt and headless never waits there.
                    SessionEvent::ToolApprovalEscalated {
                        tool_call_id, delegate, reason,
                    } => {
                        if ours && text {
                            let _ = match delegate {
                                Some(delegate) => writeln!(
                                    stderr,
                                    "[tool] {} passed to the human: {}",
                                    delegate.as_str(),
                                    concise(reason)
                                ),
                                None => writeln!(stderr, "[tool] {}", concise(reason)),
                            };
                        }
                        let unattended = !ours
                            || matches!(handle.approval, HeadlessApproval::Auto | HeadlessApproval::Ask);
                        if unattended && let Some(run_id) = envelope.run_id {
                            respond_approval(
                                sessions,
                                run_id,
                                *tool_call_id,
                                ApprovalDecision::Deny,
                                stderr,
                            )
                            .await;
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
                    SessionEvent::ToolApprovalRequested { tool_call, question, .. } => {
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
                        unattended_approval(sessions, options, &SessionHandle {
                            approval: handle.approval,
                            workspace_id: handle.workspace_id, session_id: handle.session_id,
                            subscribe_after: handle.subscribe_after, wait_for_delegate: handle.wait_for_delegate,
                            child_waits_for_delegate: handle.child_waits_for_delegate,
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

/// Answers one pending tool approval. Failures are reported but never fatal:
/// an unanswerable approval resolves by the runtime's own deny-by-timeout,
/// so the run still cannot stall forever.
/// The session grant an allowlist entry earns a held call, if any. Shell
/// prefixes match the server's own preview of the command with the policy's
/// word-boundary rule, so `--allow-shell "cargo test"` covers
/// `cargo test -p qq-core` and never `cargo test | sh`.
fn allowlisted_grant(
    options: &HeadlessOptions,
    approval: HeadlessApproval,
    tool_name: &str,
    shell: Option<&ShellCommandPreview>,
    fetch: Option<&qq_protocol::FetchPreview>,
) -> Option<ApprovalGrant> {
    // Auto's declared grants were stored before dispatch. Any remaining
    // approval is internal supervision, which these flags cannot bypass.
    if approval == HeadlessApproval::Auto {
        return None;
    }
    if recordable_grant(tool_name) && options.allow_tools.iter().any(|name| name == tool_name) {
        return Some(ApprovalGrant::Tool {
            name: tool_name.to_owned(),
        });
    }
    if let Some(fetch) = fetch {
        return options
            .allow_hosts
            .iter()
            .find(|grant| {
                recordable_grant(grant) && qq_core::host_grant_matches(grant, &fetch.host)
            })
            .map(|grant| ApprovalGrant::Host {
                host: grant.clone(),
            });
    }
    let command = shell.map(|preview| preview.command.as_str())?;
    options
        .allow_shell_prefixes
        .iter()
        .find(|prefix| recordable_grant(prefix) && qq_core::shell_prefix_matches(prefix, command))
        .map(|prefix| ApprovalGrant::ShellPrefix {
            prefix: prefix.clone(),
        })
}

/// A grant value the session table will store: non-empty and within
/// [`qq_core::MAX_GRANT_BYTES`]. An allowlist entry past that bound used to
/// make the approval command fail with "approval grant is empty or exceeds the
/// session limit"; such an entry now matches nothing, so the call takes the
/// unattended path instead of erroring.
fn recordable_grant(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= qq_core::MAX_GRANT_BYTES
}

async fn unattended_approval(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    handle: &SessionHandle,
    envelope: &SessionEventEnvelope,
    stderr: &mut impl Write,
) {
    let SessionEvent::ToolApprovalRequested {
        tool_call,
        shell,
        fetch,
        ..
    } = &envelope.event
    else {
        return;
    };
    unattended_call(
        sessions,
        options,
        handle,
        tool_call,
        shell.as_deref(),
        fetch.as_deref(),
        stderr,
    )
    .await;
}

async fn unattended_call(
    sessions: &SessionRuntime,
    options: &HeadlessOptions,
    handle: &SessionHandle,
    tool_call: &qq_protocol::ToolCallSnapshot,
    shell: Option<&ShellCommandPreview>,
    fetch: Option<&qq_protocol::FetchPreview>,
    stderr: &mut impl Write,
) {
    let text = options.format == HeadlessFormat::Text;
    // The headless invocation is the approval client.
    // An explicit allowlist answers first, as a session
    // grant so the same tool or prefix is not held again.
    // Otherwise full approves everything unattended; auto
    // denies whatever the policy escalated (dangerous
    // shell) so the run never stalls waiting for a human —
    // but when a reviewer model is configured the deny is
    // deferred briefly, giving the reviewer its window.
    // The reviewer settles the call itself on approve or
    // deny; the deferred deny is the unattended answer to
    // an escalation. A late deny is harmless: resolution is
    // idempotent, so a reviewer resolution that landed
    // first stands.
    // A child session's held calls belong to a supervised
    // write child: the reviewer adjudicates them, and the
    // headless root only supplies the unattended fallback
    // (a deferred deny), never a blanket approve. The
    // root's predeclared authority still applies through normal dispatch.
    let approval =
        if handle.approval == HeadlessApproval::Ask || tool_call.session_id == handle.session_id {
            handle.approval
        } else {
            HeadlessApproval::Auto
        };
    let granted = allowlisted_grant(options, approval, &tool_call.name, shell, fetch);
    let decision = match (granted, approval) {
        (_, HeadlessApproval::Ask) => Some(ApprovalDecision::Deny),
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
        (None, HeadlessApproval::Auto)
            if (tool_call.session_id == handle.session_id && handle.wait_for_delegate)
                || (tool_call.session_id != handle.session_id
                    && handle.child_waits_for_delegate) =>
        {
            let run_id = tool_call.run_id;
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
            None
        }
        (None, HeadlessApproval::Auto | HeadlessApproval::ReadOnly) => Some(ApprovalDecision::Deny),
    };
    if let Some(decision) = decision {
        let run_id = tool_call.run_id;
        respond_approval(sessions, run_id, tool_call.id, decision, stderr).await;
    }
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
        | SessionRuntimeError::InvalidModelSelection
        | SessionRuntimeError::GoalCheckForbidden
        | SessionRuntimeError::GoalCheckPolicy(_) => HeadlessStatus::InvalidConfiguration,
        _ => HeadlessStatus::HarnessFailure,
    }
}

fn write_delegate_resolution(
    stderr: &mut impl Write,
    name: &str,
    resolution: ApprovalResolution,
    delegate: Option<qq_protocol::DelegateIdentity>,
) -> io::Result<()> {
    let verdict = match resolution {
        ApprovalResolution::ApprovedByReviewer => "approved",
        ApprovalResolution::DeniedByReviewer => "denied",
        _ => return Ok(()),
    };
    writeln!(
        stderr,
        "[tool] {name} {verdict} by {}",
        delegate.unwrap_or_default().as_str()
    )
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
    #[cfg(target_os = "linux")]
    mod recovery;
    use std::{
        path::Path,
        sync::{Arc, Mutex},
    };

    use futures_util::stream;
    use qq_core::{
        LoadedRuntime, Runtime, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadProgress,
        RuntimeLoadRequest, RuntimeLoader, SessionRuntimeOptions,
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
            // Spawn preflight validates before admission; hold only the queued
            // child's real execution preparation, which has a cancellation owner.
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
            // The slice checkpoint keeps tools declared (RR1); its report
            // notice, the request's last message, marks that turn.
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
            // Reads alone stall: every 64 calls without output the run is
            // asked for a report, answers it, and keeps reading.
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

    async fn retire_delivery_history(fixture: &Fixture) {
        let database = fixture
            ._directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        tokio::task::spawn_blocking(move || {
            rusqlite::Connection::open(database)
                .unwrap()
                .execute("DELETE FROM events", [])
                .unwrap();
        })
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
            reviewer_configured: false,
            configured_delegate: qq_protocol::ApprovalDelegate::ByMode,
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

    struct GoalVerdictProvider {
        blocked: bool,
        turns: Mutex<usize>,
    }
    impl Provider for GoalVerdictProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut turns = self.turns.lock().unwrap();
            let mut events = if request.tools().is_empty() {
                assert!(!self.blocked);
                vec![Ok(ProviderEvent::OutputTextDelta {
                    text: r#"{"achieved":true,"evidence":"independently verified the persisted work and evidence"}"#.to_owned(),
                })]
            } else if *turns == 0 {
                assert!(
                    request
                        .tools()
                        .iter()
                        .any(|tool| tool.name() == "update_goal")
                );
                vec![
                    Ok(ProviderEvent::ToolCallStarted { id: "goal-update".into(), name: "update_goal".into() }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta { id: "goal-update".into(),
                        json: if self.blocked {
                            r#"{"status":{"type":"blocked","reason":"missing external input"}}"#
                        } else {
                            r#"{"status":{"type":"achieved","evidence":"the requested work is ready"}}"#
                        }.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id: "goal-update".into() }),
                ]
            } else {
                vec![Ok(ProviderEvent::OutputTextDelta {
                    text: "work ready".into(),
                })]
            };
            *turns += 1;
            events.push(Ok(ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 5,
                    output_tokens: 3,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
            }));
            Box::pin(stream::iter(events))
        }
    }

    #[tokio::test]
    async fn foreground_goal_achievement_and_blocking_emit_every_direct_run_before_final_state() {
        for blocked in [true, false] {
            let fixture = fixture(move || GoalVerdictProvider {
                blocked,
                turns: Mutex::new(0),
            })
            .await;
            let mut request = goal_request();
            request.max_runs = Some(3);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = tokio::time::timeout(
                Duration::from_secs(10),
                run_goal(
                    &fixture.sessions,
                    options(&fixture.workspace),
                    request,
                    std::future::pending(),
                    &mut stdout,
                    &mut stderr,
                ),
            )
            .await
            .unwrap();
            assert_eq!(
                code,
                if blocked { 7 } else { 0 },
                "{}",
                String::from_utf8_lossy(&stderr)
            );
            let records = parse_records(&String::from_utf8(stdout).unwrap());
            let final_record = records.last().unwrap();
            assert_eq!(final_record["type"], "goal");
            assert_eq!(final_record["exit_code"], code);
            let runs: Vec<_> = records
                .iter()
                .filter(|record| record["type"] == "goal_run")
                .collect();
            assert_eq!(runs.len(), if blocked { 1 } else { 2 }, "{records:?}");
            for run in runs {
                assert!(run["run"]["outcome"].is_object());
                assert_eq!(run["goal"], run["run"]["goal"]);
                assert_eq!(run["session_id"], final_record["goal"]["session_id"]);
            }
            assert_eq!(
                final_record["goal"]["status"]["type"],
                if blocked { "blocked" } else { "achieved" }
            );
            if !blocked {
                assert_eq!(final_record["goal"]["counters"]["audits"], 1);
            }
            fixture.sessions.close().await.unwrap();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_goal_reopen_replays_work_before_client_adoption() {
        struct CheckAuthority;
        impl qq_core::WorkspaceGrantAuthority for CheckAuthority {
            fn goal_policy(&self, _: &Path) -> qq_core::GoalPolicyFuture {
                Box::pin(std::future::ready(Ok(qq_core::GoalPolicy::default())))
            }
            fn seed_grants(&self, _: &Path) -> qq_core::GrantSeedFuture {
                Box::pin(std::future::ready(qq_core::WorkspaceGrantSeed::default()))
            }
            fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> qq_core::GrantPromotionFuture {
                panic!("check consent must not promote tool grants")
            }
            fn validate_goal_check(
                &self,
                _: &Path,
                command: &str,
            ) -> qq_core::GoalCheckPolicyFuture {
                assert_eq!(command, "printf checked");
                Box::pin(std::future::ready(Ok(())))
            }
        }
        for asking in [false, true] {
            let loader: Arc<dyn RuntimeLoader> = if asking {
                Arc::new(ProviderLoader(|| AskingProvider {
                    turn: Mutex::new(0),
                }))
            } else {
                Arc::new(ProviderLoader(|| GoalVerdictProvider {
                    blocked: false,
                    turns: Mutex::new(0),
                }))
            };
            let mut fixture = fixture_with_loader(Arc::clone(&loader)).await;
            let mut opts = options(&fixture.workspace);
            opts.approval = HeadlessApproval::Full;
            let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
                .await
                .unwrap();
            opts.session = Some(handle.session_id);
            let mut request = goal_request();
            request.check = Some("printf checked".into());
            request.max_runs = Some(3);
            assert_eq!(
                run_goal(
                    &fixture.sessions,
                    opts.clone(),
                    request,
                    std::future::pending(),
                    &mut Vec::new(),
                    &mut Vec::new()
                )
                .await,
                6
            );
            fixture.sessions.close().await.unwrap();
            let path = fixture
                ._directory
                .path()
                .canonicalize()
                .unwrap()
                .join("sessions.sqlite3");
            fixture.sessions = SessionRuntime::open(
                SessionRuntimeOptions::new(path).with_grant_authority(Arc::new(CheckAuthority)),
                loader,
            )
            .await
            .unwrap();
            let snapshot = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap();
            let goal = snapshot.focused.unwrap().goal.unwrap();
            let mut events = fixture
                .sessions
                .subscribe(SubscribeRequest {
                    workspace_id: handle.workspace_id,
                    after: snapshot.cursor,
                })
                .unwrap();
            send(
                &fixture.sessions,
                SessionCommand::GoalControl {
                    session_id: handle.session_id,
                    expected: goal_identity(&goal),
                    action: qq_protocol::GoalControl::ApproveCheck {
                        scope: qq_protocol::GoalCheckScope::ForGoal,
                        command: "printf checked".into(),
                    },
                },
            )
            .await
            .unwrap();
            // Let startup work reach a held question or achievement before
            // the foreground client takes its first adoption snapshot.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = events.next().await.unwrap().unwrap().event;
                    if (asking && matches!(event, SessionEvent::ToolApprovalRequested { question: Some(_), .. }))
                        || matches!(event, SessionEvent::GoalUpdated { ref goal } if goal.status == qq_protocol::GoalStatus::Achieved) {
                        break;
                    }
                }
            }).await.unwrap();
            drop(events);
            retire_delivery_history(&fixture).await;
            let mut resume = goal_request();
            resume.objective = None;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = tokio::time::timeout(
                Duration::from_secs(5),
                run_goal(
                    &fixture.sessions,
                    opts,
                    resume,
                    std::future::pending(),
                    &mut stdout,
                    &mut stderr,
                ),
            )
            .await
            .expect("adoption must consume an already-held input request");
            assert_eq!(
                code,
                if asking { 6 } else { 0 },
                "{}",
                String::from_utf8_lossy(&stderr)
            );
            let records = parse_records(&String::from_utf8(stdout).unwrap());
            let direct: Vec<_> = records.iter().filter(|r| r["type"] == "goal_run").collect();
            assert_eq!(
                direct.len(),
                if asking { 1 } else { 2 },
                "every startup run must precede the final record"
            );
            if asking {
                assert_eq!(
                    records.last().unwrap()["goal"]["status"]["reason"]["cause"]["type"],
                    "approval"
                );
            }
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_rejects_preempting_user_work_before_adoption() {
        let fixture = fixture(|| HangingProvider).await;
        let base = options(&fixture.workspace);
        let owner = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
            .await
            .unwrap();
        send(
            &fixture.sessions,
            SessionCommand::SubmitPrompt {
                session_id: owner.session_id,
                input: vec![InputPart::text("question")],
                limits: RunLimits::default(),
                correlation: Default::default(),
                output: None,
            },
        )
        .await
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        send(
            &fixture.sessions,
            SessionCommand::SetGoal {
                check_approval: None,
                session_id: owner.session_id,
                objective: "later objective".into(),
                check: None,
                budget: Box::new(qq_protocol::GoalBudget {
                    max_goal_runs: 2,
                    deadline_ms: now + 60000,
                    max_total_tokens: None,
                    max_cost_usd_nanos: None,
                    per_run: RunLimits::default(),
                }),
                expected: None,
                replace: false,
            },
        )
        .await
        .unwrap();
        let mut opts = base;
        opts.session = Some(owner.session_id);
        let request = goal_request();
        let result = prepare_session(&fixture.sessions, &opts, SessionIntent::Goal(&request)).await;
        assert!(
            matches!(
                result,
                Err(Failure {
                    status: HeadlessStatus::InvalidConfiguration,
                    ..
                })
            ),
            "must reject same-session user work before accepting exclusive goal ownership"
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_invalid_resume_does_not_pause_active_goal() {
        for approval in [None, Some(qq_protocol::GoalCheckScope::Once)] {
            let fixture = fixture(|| HangingProvider).await;
            let base = options(&fixture.workspace);
            let handle = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
                .await
                .unwrap();
            let mut args = goal_request();
            let command = crate::goals::prepare_set(
                &crate::cli::GoalSetArgs {
                    target: crate::cli::GoalTarget {
                        session: handle.session_id,
                        workspace: None,
                        json: true,
                    },
                    objective: "keep working".into(),
                    replace: false,
                    check: None,
                    clear_check: false,
                    goal_deadline: None,
                    goal_max_runs: Some(2),
                    max_cost_usd_nanos: None,
                },
                None,
                &args.defaults,
                u64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                )
                .unwrap(),
            )
            .unwrap();
            let goal = goal_receipt(send(&fixture.sessions, command).await.unwrap()).unwrap();
            args.objective = None;
            args.check = Some("new check".into());
            args.check_approval = approval;
            let mut opts = base;
            opts.model.model = Some("test/changed-model".into());
            opts.approval = HeadlessApproval::Full;
            opts.approval_explicit = true;
            opts.session = Some(handle.session_id);
            let code = run_goal(
                &fixture.sessions,
                opts,
                args,
                std::future::pending(),
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .await;
            assert_eq!(code, 2);
            let snapshot = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap();
            let focused = snapshot.focused.unwrap();
            assert_eq!(
                focused.summary.approval_mode,
                qq_protocol::ApprovalMode::ReadOnly
            );
            assert_eq!(
                focused.summary.profile,
                qq_protocol::AgentProfileId::default()
            );
            assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
            let current = focused.goal.unwrap();
            assert_eq!(current.status, qq_protocol::GoalStatus::Active);
            assert_eq!(current.revision, goal.revision);
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_missing_specification_rejects_before_settings_change() {
        for replace in [false, true] {
            let fixture = fixture(|| HangingProvider).await;
            let base = options(&fixture.workspace);
            let handle = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
                .await
                .unwrap();
            let mut opts = base;
            opts.session = Some(handle.session_id);
            opts.model.model = Some("test/changed-model".into());
            opts.profile = qq_protocol::AgentProfileId::new("reviewer").unwrap();
            opts.approval = HeadlessApproval::Full;
            opts.approval_explicit = true;
            let mut request = goal_request();
            request.replace = replace;
            if !replace {
                request.objective = None;
            }
            let mut stdout = Vec::new();
            assert_eq!(
                run_goal(
                    &fixture.sessions,
                    opts,
                    request,
                    std::future::pending(),
                    &mut stdout,
                    &mut Vec::new()
                )
                .await,
                2
            );
            assert!(stdout.is_empty());
            let focused = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap()
                .focused
                .unwrap();
            assert!(focused.goal.is_none());
            assert_eq!(
                focused.summary.approval_mode,
                qq_protocol::ApprovalMode::ReadOnly
            );
            assert_eq!(
                focused.summary.profile,
                qq_protocol::AgentProfileId::default()
            );
            assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_invalid_new_invocations_do_not_create_sessions() {
        for case in ["replace", "runs", "objective", "missing"] {
            let fixture = fixture(|| HangingProvider).await;
            let opts = options(&fixture.workspace);
            let CommandOutcome::WorkspaceResolved { workspace_id } = send(
                &fixture.sessions,
                SessionCommand::ResolveWorkspace {
                    path: fixture.workspace.display().to_string(),
                },
            )
            .await
            .unwrap()
            .outcome
            else {
                panic!("workspace");
            };
            let mut request = goal_request();
            match case {
                "replace" => request.replace = true,
                "runs" => request.max_runs = Some(request.defaults.max_runs + 1),
                "objective" => request.objective = Some(String::new()),
                _ => request.objective = None,
            }
            assert_eq!(
                run_goal(
                    &fixture.sessions,
                    opts,
                    request,
                    std::future::pending(),
                    &mut Vec::new(),
                    &mut Vec::new()
                )
                .await,
                2,
                "{case}"
            );
            let snapshot = fixture
                .sessions
                .snapshot(SnapshotRequest::new(workspace_id, None, 16, 16))
                .await
                .unwrap();
            assert!(
                snapshot.sessions.is_empty(),
                "{case}: invalid invocation created a visible session"
            );
            fixture.sessions.close().await.unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn foreground_goal_text_trace_failure_preserves_summary_and_exit() {
        let fixture = fixture(|| HangingProvider).await;
        let mut opts = options(&fixture.workspace);
        opts.format = HeadlessFormat::Text;
        opts.trace = Some(PathBuf::from("/dev/full"));
        let mut request = goal_request();
        request.check = Some("printf checked".into());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_goal(
            &fixture.sessions,
            opts,
            request,
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(code, 6);
        let summary = String::from_utf8(stdout).unwrap();
        assert_eq!(summary.lines().count(), 1);
        assert!(summary.starts_with("goal "), "{summary}");
        assert!(!stderr.is_empty());
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_without_check_consent_returns_paused_without_model_work() {
        let fixture = fixture(|| TextProvider).await;
        let mut request = goal_request();
        request.check = Some("printf checked".to_owned());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_goal(
                &fixture.sessions,
                options(&fixture.workspace),
                request,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 6, "{}", String::from_utf8_lossy(&stderr));
        let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let Some(qq_protocol::HeadlessRecord::Goal(final_goal)) = records.last() else {
            panic!("final goal record")
        };
        assert_eq!(final_goal.goal.counters.runs, 0);
        assert_eq!(
            final_goal.goal.status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::NeedsUser {
                    cause: qq_protocol::GoalNeedsUser::CheckApproval,
                },
            }
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, qq_protocol::HeadlessRecord::GoalRun(_)))
        );
        let mut options = options(&fixture.workspace);
        options.session = Some(final_goal.goal.session_id);
        let mut resume = goal_request();
        resume.objective = None;
        resume.max_runs = None;
        let mut resumed = Vec::new();
        assert_eq!(
            run_goal(
                &fixture.sessions,
                options,
                resume,
                std::future::pending(),
                &mut resumed,
                &mut stderr
            )
            .await,
            6
        );
        let final_resumed: qq_protocol::HeadlessRecord =
            serde_json::from_str(String::from_utf8(resumed).unwrap().lines().last().unwrap())
                .unwrap();
        assert!(
            matches!(final_resumed, qq_protocol::HeadlessRecord::Goal(record)
            if record.goal == final_goal.goal)
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_runs_existing_driver_until_budget_and_emits_each_direct_run() {
        let fixture = fixture(|| TextProvider).await;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(10),
            run_goal(
                &fixture.sessions,
                options(&fixture.workspace),
                goal_request(),
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 8, "{}", String::from_utf8_lossy(&stderr));
        let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let runs: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                qq_protocol::HeadlessRecord::GoalRun(run) => Some(run),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 1);
        assert!(matches!(
            runs[0].run.origin,
            Some(qq_protocol::RunOrigin::Goal { .. })
        ));
        assert!(runs[0].run.outcome.is_some());
        assert!(
            matches!(records.last(), Some(qq_protocol::HeadlessRecord::Goal(outcome))
            if outcome.exit_code == 8 && outcome.goal.counters.runs == 1)
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_unanswered_question_retains_needs_user_reason() {
        let fixture = fixture(|| AskingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_goal(
                &fixture.sessions,
                options(&fixture.workspace),
                goal_request(),
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 6, "{}", String::from_utf8_lossy(&stderr));
        let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let Some(qq_protocol::HeadlessRecord::Goal(final_goal)) = records.last() else {
            panic!("final goal record")
        };
        assert!(
            matches!(
                final_goal.goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Approval { .. },
                    },
                }
            ),
            "{:?}",
            final_goal.goal.status
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_cleanup_preserves_an_existing_input_pause() {
        let fixture = fixture(|| AskingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let goal = goal_receipt(
            send(
                &fixture.sessions,
                SessionCommand::SetGoal {
                    check_approval: None,
                    session_id: handle.session_id,
                    objective: "ask for the missing input".into(),
                    check: None,
                    budget: Box::new(qq_protocol::GoalBudget {
                        max_goal_runs: 2,
                        deadline_ms: now + 60000,
                        max_total_tokens: None,
                        max_cost_usd_nanos: None,
                        per_run: RunLimits::default(),
                    }),
                    expected: None,
                    replace: false,
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let mut guard = AcceptedGoalGuard::new(fixture.sessions.clone(), &goal, &handle);
        let call = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let SessionEvent::ToolApprovalRequested {
                    tool_call,
                    question: Some(_),
                    ..
                } = events.next().await.unwrap().unwrap().event
                {
                    break tool_call.id;
                }
            }
        })
        .await
        .unwrap();
        let stopped = goal_receipt(
            send(
                &fixture.sessions,
                SessionCommand::GoalControl {
                    session_id: handle.session_id,
                    expected: goal_identity(&goal),
                    action: qq_protocol::GoalControl::PauseForInput { tool_call_id: call },
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        guard.expected = goal_identity(&stopped);
        guard.settle().await.unwrap();
        let actual = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap()
            .focused
            .unwrap()
            .goal
            .unwrap();
        assert_eq!(
            actual.status, stopped.status,
            "cleanup must preserve the actionable input request"
        );
        assert_eq!(actual.revision, stopped.revision);
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_input_controls_and_run_reads_are_identity_fenced() {
        let fixture = fixture(|| AskingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let options = options(&fixture.workspace);
        let owner = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
            .await
            .unwrap();
        let other = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: owner.workspace_id,
                after: other.subscribe_after,
            })
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let receipt = send(
            &fixture.sessions,
            SessionCommand::SetGoal {
                check_approval: None,
                session_id: owner.session_id,
                objective: "held goal".into(),
                check: None,
                budget: Box::new(qq_protocol::GoalBudget {
                    max_goal_runs: 2,
                    deadline_ms: now + 60000,
                    max_total_tokens: None,
                    max_cost_usd_nanos: None,
                    per_run: RunLimits::default(),
                }),
                expected: None,
                replace: false,
            },
        )
        .await
        .unwrap();
        let goal = goal_receipt(receipt).unwrap();
        let expected = goal_identity(&goal);
        send(
            &fixture.sessions,
            SessionCommand::SubmitPrompt {
                session_id: other.session_id,
                input: vec![InputPart::text("foreign question")],
                limits: RunLimits::default(),
                correlation: Default::default(),
                output: None,
            },
        )
        .await
        .unwrap();
        let (own_call, foreign_call) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut own = None;
            let mut foreign = None;
            loop {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::ToolApprovalRequested {
                    tool_call,
                    question: Some(_),
                    ..
                } = event.event
                {
                    if event.session_id == owner.session_id {
                        own = Some(tool_call);
                    } else if event.session_id == other.session_id {
                        foreign = Some(tool_call);
                    }
                }
                if let (Some(own), Some(foreign)) = (&own, &foreign) {
                    break (own.clone(), foreign.clone());
                }
            }
        })
        .await
        .unwrap();
        let pending = fixture
            .sessions
            .goal_pending_runs(owner.session_id, expected)
            .await
            .unwrap();
        assert!(pending.contains(&own_call.run_id));
        assert!(!pending.contains(&foreign_call.run_id));
        assert!(matches!(
            fixture
                .sessions
                .goal_pending_runs(
                    owner.session_id,
                    qq_protocol::GoalIdentity {
                        revision: expected.revision + 1,
                        ..expected
                    }
                )
                .await,
            Err(SessionRuntimeError::GoalConflict { .. })
        ));
        assert!(
            fixture
                .sessions
                .goal_owns_run(owner.session_id, expected, own_call.run_id)
                .await
                .unwrap()
        );
        assert!(
            !fixture
                .sessions
                .goal_owns_run(owner.session_id, expected, foreign_call.run_id)
                .await
                .unwrap()
        );
        let control = |expected, call| SessionCommand::GoalControl {
            session_id: owner.session_id,
            expected,
            action: qq_protocol::GoalControl::PauseForInput { tool_call_id: call },
        };
        assert!(matches!(
            fixture
                .sessions
                .command(
                    CommandId::generate().unwrap(),
                    control(expected, qq_protocol::ToolCallId::from_bytes([0x99; 16]))
                )
                .await,
            Err(SessionRuntimeError::GoalInputNotPending)
        ));
        assert!(matches!(
            fixture
                .sessions
                .command(
                    CommandId::generate().unwrap(),
                    control(expected, foreign_call.id)
                )
                .await,
            Err(SessionRuntimeError::GoalInputNotPending)
        ));
        assert!(matches!(
            fixture
                .sessions
                .command(
                    CommandId::generate().unwrap(),
                    control(
                        qq_protocol::GoalIdentity {
                            revision: expected.revision + 1,
                            ..expected
                        },
                        own_call.id
                    )
                )
                .await,
            Err(SessionRuntimeError::GoalConflict { .. })
        ));
        let run = fixture
            .sessions
            .run_snapshot(owner.workspace_id, owner.session_id, own_call.run_id)
            .await
            .unwrap();
        assert_eq!(run.id, own_call.run_id);
        assert!(matches!(
            fixture
                .sessions
                .run_snapshot(
                    WorkspaceId::from_bytes([0x90; 16]),
                    owner.session_id,
                    own_call.run_id
                )
                .await,
            Err(SessionRuntimeError::SessionNotFound)
        ));
        assert!(matches!(
            fixture
                .sessions
                .run_snapshot(owner.workspace_id, other.session_id, own_call.run_id)
                .await,
            Err(SessionRuntimeError::RunNotFound)
        ));
        let id = CommandId::generate().unwrap();
        let command = control(expected, own_call.id);
        let receipt = fixture.sessions.command(id, command.clone()).await.unwrap();
        assert_eq!(
            fixture.sessions.command(id, command).await.unwrap(),
            receipt
        );
        let stopped = goal_receipt(receipt).unwrap();
        assert!(matches!(stopped.status, qq_protocol::GoalStatus::Paused {
            reason: qq_protocol::GoalPauseReason::NeedsUser {
                cause: qq_protocol::GoalNeedsUser::Approval { tool_call_id },
            },
        } if tool_call_id == own_call.id));
        assert!(matches!(
            fixture
                .sessions
                .command(
                    CommandId::generate().unwrap(),
                    control(goal_identity(&stopped), own_call.id)
                )
                .await,
            Err(SessionRuntimeError::GoalInputNotPending)
        ));
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_interrupt_pauses_and_returns_distinct_exit() {
        let fixture = fixture(|| HangingProvider).await;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_goal(
            &fixture.sessions,
            options(&fixture.workspace),
            goal_request(),
            std::future::ready(()),
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
        let final_record: qq_protocol::HeadlessRecord =
            serde_json::from_str(String::from_utf8(stdout).unwrap().lines().last().unwrap())
                .unwrap();
        assert!(
            matches!(final_record, qq_protocol::HeadlessRecord::Goal(record)
            if record.exit_code == 130 && record.goal.status == qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::User,
            })
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_interrupt_reconciles_durable_achievement() {
        struct RefusedLoad(qq_protocol::RunFailureKind);
        impl RuntimeLoader for RefusedLoad {
            fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let kind = self.0;
                Box::pin(std::future::ready(Err(RuntimeLoadError {
                    kind,
                    message: "recover this configuration or credential".into(),
                })))
            }
        }
        for failure in [
            None,
            Some(qq_protocol::RunFailureKind::Authentication),
            Some(qq_protocol::RunFailureKind::Configuration),
        ] {
            let loader: Arc<dyn RuntimeLoader> = match failure {
                Some(kind) => Arc::new(RefusedLoad(kind)),
                None => Arc::new(ProviderLoader(|| GoalVerdictProvider {
                    blocked: false,
                    turns: Mutex::new(0),
                })),
            };
            let fixture = fixture_with_loader(loader).await;
            let base = options(&fixture.workspace);
            let handle = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
                .await
                .unwrap();
            let mut observer = fixture
                .sessions
                .subscribe(SubscribeRequest {
                    workspace_id: handle.workspace_id,
                    after: handle.subscribe_after,
                })
                .unwrap();
            let stopped = Arc::new(Mutex::new(None));
            let observed = Arc::clone(&stopped);
            let interrupt = async move {
                loop {
                    let event = observer.next().await.unwrap().unwrap();
                    if let SessionEvent::GoalUpdated { goal } = event.event
                        && goal.status != qq_protocol::GoalStatus::Active
                    {
                        *observed.lock().unwrap() = Some((goal.status.clone(), goal.revision));
                        break;
                    }
                }
            };
            let mut opts = base;
            opts.session = Some(handle.session_id);
            let mut request = goal_request();
            request.max_runs = Some(3);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = tokio::time::timeout(
                Duration::from_secs(5),
                run_goal(
                    &fixture.sessions,
                    opts,
                    request,
                    interrupt,
                    &mut stdout,
                    &mut stderr,
                ),
            )
            .await
            .unwrap();
            assert_eq!(
                code,
                if failure.is_none() { 0 } else { 6 },
                "{failure:?}: {}",
                String::from_utf8_lossy(&stderr)
            );
            let records = parse_records(&String::from_utf8(stdout).unwrap());
            assert_eq!(records.last().unwrap()["type"], "goal");
            let (status, revision) = stopped.lock().unwrap().take().unwrap();
            assert_eq!(
                records.last().unwrap()["goal"]["status"],
                serde_json::to_value(&status).unwrap()
            );
            assert_eq!(records.last().unwrap()["goal"]["revision"], revision);
            match failure {
                None => assert_eq!(status, qq_protocol::GoalStatus::Achieved),
                Some(qq_protocol::RunFailureKind::Authentication) => assert!(matches!(
                    status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::Authentication { .. },
                        }
                    }
                )),
                Some(_) => assert!(matches!(
                    status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::Configuration { .. },
                        }
                    }
                )),
            }
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_refused_check_consent_is_invalid_configuration() {
        struct DeniedCheck;
        impl qq_core::WorkspaceGrantAuthority for DeniedCheck {
            fn seed_grants(&self, _: &Path) -> qq_core::GrantSeedFuture {
                Box::pin(std::future::ready(qq_core::WorkspaceGrantSeed::default()))
            }
            fn goal_policy(&self, _: &Path) -> qq_core::GoalPolicyFuture {
                Box::pin(std::future::ready(Ok(qq_core::GoalPolicy::default())))
            }
            fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> qq_core::GrantPromotionFuture {
                panic!("refused check cannot promote grants")
            }
            fn validate_goal_check(&self, _: &Path, _: &str) -> qq_core::GoalCheckPolicyFuture {
                Box::pin(std::future::ready(Err(
                    qq_core::GoalCheckPolicyError::ManagedDenied,
                )))
            }
        }
        for command in ["sudo printf checked", "printf checked"] {
            let mut fixture = fixture(|| HangingProvider).await;
            fixture.sessions.close().await.unwrap();
            let path = fixture
                ._directory
                .path()
                .canonicalize()
                .unwrap()
                .join("sessions.sqlite3");
            fixture.sessions = SessionRuntime::open(
                SessionRuntimeOptions::new(path).with_grant_authority(Arc::new(DeniedCheck)),
                Arc::new(ProviderLoader(|| HangingProvider)),
            )
            .await
            .unwrap();
            let mut opts = options(&fixture.workspace);
            let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
                .await
                .unwrap();
            opts.session = Some(handle.session_id);
            let mut request = goal_request();
            request.check = Some(command.into());
            request.check_approval = Some(qq_protocol::GoalCheckScope::ForGoal);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = run_goal(
                &fixture.sessions,
                opts,
                request,
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            )
            .await;
            assert_eq!(code, 2, "{command}: {}", String::from_utf8_lossy(&stderr));
            assert!(!stderr.is_empty());
            let snapshot = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap()
                .focused
                .unwrap();
            assert!(
                snapshot.goal.is_none(),
                "refused admission must roll back the goal"
            );
            assert!(
                snapshot.runs.is_empty(),
                "refused admission must not schedule work"
            );
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_interrupt_with_queued_question_replays_to_final_record() {
        let fixture = fixture(|| AskingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let receipt = send(
            &fixture.sessions,
            SessionCommand::ResolveWorkspace {
                path: fixture.workspace.display().to_string(),
            },
        )
        .await
        .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
            panic!("workspace receipt")
        };
        let mut observer = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .unwrap();
        let interrupt = async move {
            loop {
                let event = observer.next().await.unwrap().unwrap();
                if matches!(
                    event.event,
                    SessionEvent::ToolApprovalRequested {
                        question: Some(_),
                        ..
                    }
                ) {
                    break;
                }
            }
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_goal(
                &fixture.sessions,
                options(&fixture.workspace),
                goal_request(),
                interrupt,
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        assert_eq!(records.last().unwrap()["type"], "goal");
        assert_eq!(records.last().unwrap()["exit_code"], 130);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["type"] == "goal_run")
                .count(),
            1
        );
        fixture.sessions.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_goal_check_scopes_control_real_repeated_check_execution() {
        const CHECK: &str = "printf x >> checks.log; if test -e checks.flag; then exit 0; else printf ready > checks.flag; exit 1; fi";
        struct CheckAuthority;
        impl qq_core::WorkspaceGrantAuthority for CheckAuthority {
            fn seed_grants(&self, _: &Path) -> qq_core::GrantSeedFuture {
                Box::pin(std::future::ready(qq_core::WorkspaceGrantSeed::default()))
            }
            fn goal_policy(&self, _: &Path) -> qq_core::GoalPolicyFuture {
                Box::pin(std::future::ready(Ok(qq_core::GoalPolicy::default())))
            }
            fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> qq_core::GrantPromotionFuture {
                panic!("goal check consent must not promote ordinary tool grants")
            }
            fn validate_goal_check(
                &self,
                workspace: &Path,
                command: &str,
            ) -> qq_core::GoalCheckPolicyFuture {
                assert!(workspace.is_absolute());
                assert_eq!(command, CHECK);
                Box::pin(std::future::ready(Ok(())))
            }
        }
        for scope in [
            qq_protocol::GoalCheckScope::Once,
            qq_protocol::GoalCheckScope::ForGoal,
        ] {
            let mut fixture = fixture(|| GoalVerdictProvider {
                blocked: false,
                turns: Mutex::new(0),
            })
            .await;
            fixture.sessions.close().await.unwrap();
            let path = fixture
                ._directory
                .path()
                .canonicalize()
                .unwrap()
                .join("sessions.sqlite3");
            fixture.sessions = SessionRuntime::open(
                SessionRuntimeOptions::new(path).with_grant_authority(Arc::new(CheckAuthority)),
                Arc::new(ProviderLoader(|| GoalVerdictProvider {
                    blocked: false,
                    turns: Mutex::new(0),
                })),
            )
            .await
            .unwrap();
            let mut opts = options(&fixture.workspace);
            opts.approval = HeadlessApproval::Full;
            let mut request = goal_request();
            request.max_runs = Some(3);
            request.check = Some(CHECK.to_owned());
            request.check_approval = Some(scope);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = tokio::time::timeout(
                Duration::from_secs(10),
                run_goal(
                    &fixture.sessions,
                    opts,
                    request,
                    std::future::pending(),
                    &mut stdout,
                    &mut stderr,
                ),
            )
            .await
            .unwrap();
            let repeated = scope == qq_protocol::GoalCheckScope::ForGoal;
            assert_eq!(
                code,
                if repeated { 0 } else { 6 },
                "{}",
                String::from_utf8_lossy(&stderr)
            );
            let records = parse_records(&String::from_utf8(stdout).unwrap());
            let checks: Vec<_> = records
                .iter()
                .filter(|record| {
                    record["type"] == "goal_run" && record["run"]["origin"]["type"] == "goal_check"
                })
                .collect();
            // A consumed one-shot leaves a refused verification attempt;
            // its durable record does not imply a second process execution.
            assert_eq!(checks.len(), 2);
            assert_ne!(checks[0]["run"]["id"], checks[1]["run"]["id"]);
            assert_eq!(
                records.last().unwrap()["goal"]["counters"]["audits"],
                if repeated { 2 } else { 1 }
            );
            assert_eq!(
                std::fs::read_to_string(fixture.workspace.join("checks.log")).unwrap(),
                if repeated { "xx" } else { "x" }
            );
            let final_goal = &records.last().unwrap()["goal"];
            assert_eq!(
                final_goal["check_authorization"],
                if repeated { "for_goal" } else { "consumed" }
            );
            assert_eq!(
                final_goal["status"]["type"],
                if repeated { "achieved" } else { "paused" }
            );
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_adopts_active_work_and_leaves_foreign_runs_running() {
        for interrupt_on_close in [false, true] {
            let fixture = fixture(|| HangingProvider).await;
            let base = options(&fixture.workspace);
            let owner = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
                .await
                .unwrap();
            let other = prepare_session(&fixture.sessions, &base, SessionIntent::Prompt)
                .await
                .unwrap();
            let mut events = fixture
                .sessions
                .subscribe(SubscribeRequest {
                    workspace_id: owner.workspace_id,
                    after: other.subscribe_after,
                })
                .unwrap();
            let foreign = send(
                &fixture.sessions,
                SessionCommand::SubmitPrompt {
                    session_id: other.session_id,
                    input: vec![InputPart::text("independent work")],
                    limits: RunLimits::default(),
                    correlation: Default::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
            let CommandOutcome::PromptQueued {
                run_id: foreign_id, ..
            } = foreign.outcome
            else {
                panic!("foreign prompt")
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let goal = goal_receipt(
                send(
                    &fixture.sessions,
                    SessionCommand::SetGoal {
                        check_approval: None,
                        session_id: owner.session_id,
                        objective: "existing active objective".into(),
                        check: None,
                        budget: Box::new(qq_protocol::GoalBudget {
                            max_goal_runs: 2,
                            deadline_ms: now + 60000,
                            max_total_tokens: None,
                            max_cost_usd_nanos: None,
                            per_run: RunLimits::default(),
                        }),
                        expected: None,
                        replace: false,
                    },
                )
                .await
                .unwrap(),
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut own_started = false;
                let mut other_started = false;
                while !own_started || !other_started {
                    let event = events.next().await.unwrap().unwrap();
                    if matches!(event.event, SessionEvent::RunStarted { .. }) {
                        own_started |= event.session_id == owner.session_id;
                        other_started |= event.session_id == other.session_id;
                    }
                }
            })
            .await
            .unwrap();
            let goal = goal_receipt(
                send(
                    &fixture.sessions,
                    SessionCommand::SetGoal {
                        check_approval: None,
                        session_id: owner.session_id,
                        objective: "edited while running".into(),
                        check: goal.check.clone(),
                        budget: Box::new(goal.budget.clone()),
                        expected: Some(goal_identity(&goal)),
                        replace: false,
                    },
                )
                .await
                .unwrap(),
            )
            .unwrap();
            let mut options = base;
            options.session = Some(owner.session_id);
            let mut request = goal_request();
            request.objective = None;
            request.max_runs = None;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = run_goal(
                &fixture.sessions,
                options,
                request,
                std::future::ready(()),
                &mut stdout,
                &mut stderr,
            )
            .await;
            assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
            let records = parse_records(&String::from_utf8(stdout).unwrap());
            let finished = &records.last().unwrap()["goal"];
            assert_eq!(
                finished["goal_id"],
                serde_json::to_value(goal.goal_id).unwrap()
            );
            assert_eq!(finished["objective"], goal.objective);
            assert_eq!(
                finished["budget"],
                serde_json::to_value(&goal.budget).unwrap()
            );
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record["type"] == "goal_run")
                    .count(),
                1
            );
            let adopted = records
                .iter()
                .find(|record| record["type"] == "goal_run")
                .unwrap();
            assert_eq!(adopted["goal"]["revision"], goal.revision - 1);
            assert_eq!(
                adopted["goal"]["goal_id"],
                serde_json::to_value(goal.goal_id).unwrap()
            );
            assert_eq!(
                fixture
                    .sessions
                    .run_snapshot(owner.workspace_id, other.session_id, foreign_id)
                    .await
                    .unwrap()
                    .status,
                qq_protocol::RunStatus::Running
            );
            let (interrupt, interrupted) = tokio::sync::oneshot::channel();
            let closing = crate::close_headless(&fixture.sessions, true, async {
                if interrupt_on_close {
                    interrupted.await.unwrap();
                } else {
                    std::future::pending::<()>().await;
                }
            });
            tokio::pin!(closing);
            assert!(futures_util::poll!(closing.as_mut()).is_pending());
            let inspect_and_finish = async {
                assert_eq!(
                    fixture
                        .sessions
                        .run_snapshot(owner.workspace_id, other.session_id, foreign_id)
                        .await
                        .unwrap()
                        .status,
                    qq_protocol::RunStatus::Running
                );
                if interrupt_on_close {
                    interrupt.send(()).unwrap();
                    loop {
                        let event = events.next().await.unwrap().unwrap();
                        if let SessionEvent::RunFinished {
                            run_id, outcome, ..
                        } = event.event
                            && run_id == foreign_id
                        {
                            assert!(matches!(outcome, qq_protocol::RunOutcome::Cancelled));
                            break;
                        }
                    }
                } else {
                    request_cancel(&fixture.sessions, foreign_id).await.unwrap();
                }
            };
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(closing, inspect_and_finish)
            })
            .await;
            if result.is_err() {
                fixture.sessions.close().await.unwrap();
            }
            let (closed, ()) = result
                .expect("runtime close must react to the new interrupt after the goal settles");
            closed.unwrap();
        }
    }

    #[tokio::test]
    async fn closing_goal_host_drains_accepted_work_without_admitting_successors() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct ReleasedRun {
            calls: Arc<AtomicUsize>,
            release: Arc<tokio::sync::Semaphore>,
            started: tokio::sync::mpsc::Sender<usize>,
        }
        impl Provider for ReleasedRun {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.try_send(call).unwrap();
                let release = self.release.clone();
                if call == 0 {
                    Box::pin(
                        stream::iter([Ok(ProviderEvent::OutputTextDelta {
                            text: "accepted work finished".into(),
                        })])
                        .chain(stream::once(async move {
                            release.acquire().await.unwrap().forget();
                            Ok(ProviderEvent::Completed {
                                usage: Some(ProviderUsage {
                                    input_tokens: 1,
                                    output_tokens: 0,
                                    cache_read_input_tokens: 0,
                                    cache_write_input_tokens: 0,
                                    reasoning_tokens: None,
                                }),
                            })
                        })),
                    )
                } else {
                    Box::pin(stream::pending())
                }
            }
        }
        for _ in 0..16 {
            tokio::time::timeout(Duration::from_secs(10), async {
                let calls = Arc::new(AtomicUsize::new(0));
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let (started, mut starts) = tokio::sync::mpsc::channel(8);
                let fixture = fixture({
                    let calls = calls.clone(); let release = release.clone();
                    move || ReleasedRun { calls: calls.clone(), release: release.clone(), started: started.clone() }
                }).await;
                let opts = options(&fixture.workspace);
                let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt).await.unwrap();
                let mut request = goal_request();
                request.max_runs = Some(3);
                let command = prepare_goal_request(&request, &opts, handle.session_id, None).unwrap().unwrap();
                send(&fixture.sessions, command).await.unwrap();
                assert_eq!(starts.recv().await, Some(0));
                let closing = fixture.sessions.close_when_idle(std::future::pending());
                tokio::pin!(closing);
                assert!(futures_util::poll!(closing.as_mut()).is_pending());
                tokio::select! {
                    result = &mut closing => panic!("closed before accepted work settled: {result:?}"),
                    snapshot = fixture.sessions.snapshot(SnapshotRequest::new(handle.workspace_id, Some(handle.session_id), 1, 1)) => { snapshot.unwrap(); }
                }
                release.add_permits(1);
                tokio::select! {
                    biased;
                    call = starts.recv() => panic!("close admitted successor call {call:?}"),
                    result = &mut closing => result.unwrap(),
                }
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }).await.expect("close must drain accepted work without autonomous refill");
        }
    }

    #[tokio::test]
    async fn foreground_goal_adopts_active_edits_for_interrupt_and_cleanup() {
        struct ObservedRevision {
            output: ForegroundOutput,
            seen: Option<tokio::sync::oneshot::Sender<()>>,
            fail_edit: bool,
        }
        impl Write for ObservedRevision {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let edited =
                    serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|record| {
                        record["envelope"]["event"]["type"] == "goal_updated"
                            && record["envelope"]["event"]["goal"]["revision"].as_u64() == Some(2)
                    });
                if edited && self.fail_edit {
                    self.fail_edit = false;
                    return Err(io::Error::other("injected edit-event output failure"));
                }
                let count = self.output.write(bytes)?;
                if edited && let Some(seen) = self.seen.take() {
                    let _ = seen.send(());
                }
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.output.flush()
            }
        }
        for action in ["interrupt", "drop", "output"] {
            let fixture = fixture(|| HangingProvider).await;
            let mut opts = options(&fixture.workspace);
            let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
                .await
                .unwrap();
            opts.session = Some(handle.session_id);
            let mut events = fixture
                .sessions
                .subscribe(SubscribeRequest {
                    workspace_id: handle.workspace_id,
                    after: handle.subscribe_after,
                })
                .unwrap();
            let (seen, observed) = tokio::sync::oneshot::channel();
            let (interrupt, interrupted) = tokio::sync::oneshot::channel();
            let mut writer = ObservedRevision {
                output: ForegroundOutput::default(),
                seen: Some(seen),
                fail_edit: action == "output",
            };
            let mut stderr = Vec::new();
            let mut foreground = Box::pin(run_goal(
                &fixture.sessions,
                opts,
                goal_request(),
                async {
                    interrupted.await.unwrap();
                },
                &mut writer,
                &mut stderr,
            ));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop { tokio::select! {
                    code = &mut foreground => panic!("returned before edit: {code}"),
                    event = events.next() => if matches!(event.unwrap().unwrap().event, SessionEvent::RunStarted { .. }) { break; },
                } }
            }).await.unwrap();
            let current = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap()
                .focused
                .unwrap()
                .goal
                .unwrap();
            let edited = goal_receipt(
                send(
                    &fixture.sessions,
                    SessionCommand::SetGoal {
                        check_approval: None,
                        session_id: handle.session_id,
                        objective: "edited while adopted".into(),
                        check: None,
                        budget: Box::new(current.budget.clone()),
                        expected: Some(goal_identity(&current)),
                        replace: false,
                    },
                )
                .await
                .unwrap(),
            )
            .unwrap();
            if action == "output" {
                let code = tokio::time::timeout(Duration::from_secs(5), foreground)
                    .await
                    .unwrap();
                assert_eq!(code, 4, "{}", String::from_utf8_lossy(&stderr));
                let stopped = fixture
                    .sessions
                    .snapshot(SnapshotRequest::new(
                        handle.workspace_id,
                        Some(handle.session_id),
                        1,
                        1,
                    ))
                    .await
                    .unwrap()
                    .focused
                    .unwrap()
                    .goal
                    .unwrap();
                assert!(
                    matches!(
                        stopped.status,
                        qq_protocol::GoalStatus::Paused {
                            reason: qq_protocol::GoalPauseReason::User,
                        }
                    ),
                    "edit-event output failure left adopted goal running: {stopped:?}; {}",
                    String::from_utf8_lossy(&stderr)
                );
                assert!(stopped.pending_run_id.is_none());
            } else {
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::select! { result = observed => result.unwrap(),
                    code = &mut foreground => panic!("returned while observing edit: {code}") }
                })
                .await
                .unwrap();
                if action == "drop" {
                    drop(foreground);
                } else {
                    interrupt.send(()).unwrap();
                    let code = tokio::time::timeout(Duration::from_secs(5), foreground)
                        .await
                        .unwrap();
                    assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
                }
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let SessionEvent::GoalUpdated { goal } =
                        events.next().await.unwrap().unwrap().event
                        && goal.goal_id == edited.goal_id
                        && goal.revision > edited.revision
                        && matches!(
                            goal.status,
                            qq_protocol::GoalStatus::Paused {
                                reason: qq_protocol::GoalPauseReason::User
                            }
                        )
                    {
                        break;
                    }
                }
            })
            .await
            .expect("edited adopted goal must be paused by its owner");
            fixture.sessions.close().await.unwrap();
        }
    }

    #[derive(Clone, Default)]
    struct ForegroundOutput {
        bytes: Arc<Mutex<Vec<u8>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Write for ForegroundOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(io::Error::other("injected output failure"));
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn foreground_goal_held_family_drains_before_terminal_output() {
        use futures_util::FutureExt;

        for action in [
            "interrupt",
            "pause",
            "cancel",
            "clear",
            "replace",
            "output",
            "abort",
        ] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let (entered, entry) = tokio::sync::oneshot::channel();
                let (release, released) = tokio::sync::oneshot::channel();
                let (exited, exit) = tokio::sync::oneshot::channel();
                let fixture = fixture_with_loader(Arc::new(ParentChildLoader {
                    parent: Arc::new(SpawnsLoopingChildProvider::new()),
                    child: Arc::new(HangingProvider),
                    held_child: Some(Arc::new(HeldChildPreparation {
                        entered: Mutex::new(Some(entered)),
                        release: Mutex::new(Some(released)),
                        exited: Mutex::new(Some(exited)),
                    })),
                })).await;
                let mut opts = options(&fixture.workspace);
                opts.approval = HeadlessApproval::Full;
                let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
                    .await.unwrap();
                opts.session = Some(handle.session_id);
                let mut foreign_opts = opts.clone();
                foreign_opts.session = None;
                foreign_opts.model.model = Some("test/foreign".into());
                let foreign = prepare_session(&fixture.sessions, &foreign_opts, SessionIntent::Prompt)
                    .await.unwrap();
                let receipt = send(&fixture.sessions, SessionCommand::SubmitPrompt {
                    session_id: foreign.session_id,
                    input: vec![InputPart::text("independent work")],
                    limits: RunLimits::default(), correlation: Default::default(), output: None,
                }).await.unwrap();
                let CommandOutcome::PromptQueued { run_id: foreign_run, .. } = receipt.outcome else {
                    panic!("foreign prompt")
                };
                let (interrupt, interrupted) = tokio::sync::oneshot::channel();
                let output = ForegroundOutput::default();
                let mut writer = output.clone();
                let mut stderr = Vec::new();
                let mut foreground = Box::pin(run_goal(
                    &fixture.sessions, opts, goal_request(),
                    async { interrupted.await.unwrap(); }, &mut writer, &mut stderr,
                ));
                let progress = tokio::select! {
                    entry = entry => entry.unwrap(),
                    code = &mut foreground => panic!("{action}: returned before child entry: {code}"),
                };
                let snapshot = fixture.sessions.snapshot(SnapshotRequest::new(
                    handle.workspace_id, Some(handle.session_id), 16, 16,
                )).await.unwrap();
                let owner = snapshot.focused.unwrap();
                let goal = owner.goal.unwrap();
                let expected = goal_identity(&goal);
                let root = owner.summary.active_run_id.expect("parent retains child ownership");
                let pending = fixture.sessions.goal_pending_runs(handle.session_id, expected).await.unwrap();
                assert_eq!(pending.len(), 2, "{action}: root and admitted sponsored child");
                let child = *pending.iter().find(|id| **id != root).unwrap();
                assert!(fixture.sessions.goal_owns_run(handle.session_id, expected, child).await.unwrap());
                let mut events = fixture.sessions.subscribe(SubscribeRequest {
                    workspace_id: handle.workspace_id, after: snapshot.cursor,
                }).unwrap();

                let early_code: Option<u8> = match action {
                    "interrupt" | "clear" | "replace" => {
                        interrupt.send(()).unwrap();
                        None
                    }
                    "pause" => {
                        send(&fixture.sessions, SessionCommand::GoalControl {
                            session_id: handle.session_id, expected, action: qq_protocol::GoalControl::Pause,
                        }).await.unwrap();
                        None
                    }
                    "cancel" => {
                        request_cancel(&fixture.sessions, root).await.unwrap();
                        None
                    }
                    "output" => {
                        output.fail.store(true, std::sync::atomic::Ordering::SeqCst);
                        None
                    }
                    "abort" => None,
                    _ => unreachable!(),
                };
                if action == "abort" || early_code.is_some() {
                    drop(foreground);
                } else {
                    tokio::select! {
                        () = progress.cancelled() => {},
                        code = &mut foreground => panic!("{action}: returned while child preparation held: {code}"),
                    }
                    assert!(foreground.as_mut().now_or_never().is_none(),
                        "{action}: foreground must remain pending after cancellation receipt");
                    let stopped = fixture.sessions.snapshot(SnapshotRequest::new(
                        handle.workspace_id, Some(handle.session_id), 1, 1,
                    )).await.unwrap().focused.unwrap().goal.unwrap();
                    if action != "cancel" {
                        let error = fixture.sessions.command(CommandId::generate().unwrap(), SessionCommand::GoalControl {
                            session_id: handle.session_id, expected: goal_identity(&stopped),
                            action: qq_protocol::GoalControl::Resume,
                        }).await.unwrap_err();
                        assert!(matches!(error, SessionRuntimeError::GoalCancellationPending));
                    }
                    assert_eq!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&stopped)).await.unwrap().len(), 2);
                    if action == "clear" {
                        send(&fixture.sessions, SessionCommand::GoalControl {
                            session_id: handle.session_id, expected: goal_identity(&stopped),
                            action: qq_protocol::GoalControl::Clear,
                        }).await.unwrap();
                    } else if action == "replace" {
                        send(&fixture.sessions, SessionCommand::SetGoal {
                            check_approval: None,
                            session_id: handle.session_id, objective: "replacement must survive".into(),
                            check: Some(qq_protocol::GoalCheck {
                                command: "printf checked".into(), timeout_ms: 1000,
                            }),
                            budget: Box::new(goal.budget.clone()), expected: Some(goal_identity(&stopped)), replace: true,
                        }).await.unwrap();
                    }
                    if matches!(action, "clear" | "replace") {
                        let mut draining = fixture.sessions.cancelled_goal_runs(handle.session_id, expected.goal_id).await.unwrap();
                        let mut family = vec![root, child];
                        draining.sort();
                        family.sort();
                        assert_eq!(draining, family);
                        assert!(fixture.sessions.cancelled_goal_runs(foreign.session_id, expected.goal_id).await.unwrap().is_empty());
                    }
                    assert!(foreground.as_mut().now_or_never().is_none());
                    assert!(!parse_records(&String::from_utf8(output.bytes.lock().unwrap().clone()).unwrap())
                        .iter().any(|record| record["type"] == "goal"));
                    release.send(()).unwrap();
                    exit.await.unwrap();
                    let code = foreground.await;
                    let records = parse_records(&String::from_utf8(output.bytes.lock().unwrap().clone()).unwrap());
                    let finals: Vec<_> = records.iter().filter(|r| r["type"] == "goal").collect();
                    if matches!(action, "clear" | "replace") {
                        assert_eq!(code, 4);
                        assert_eq!(finals.len(), 1, "{action}: {records:?}");
                        assert_eq!(records.last().unwrap()["type"], "goal");
                        assert_eq!(finals[0]["exit_code"], 4);
                        assert_eq!(finals[0]["status"], "harness_failure");
                        assert_eq!(finals[0]["goal"]["goal_id"], serde_json::to_value(expected.goal_id).unwrap());
                        let errors = String::from_utf8_lossy(&stderr);
                        assert!(errors.contains(if action == "clear" { "goal was not found" } else { "goal identity or revision conflicts with the current goal" })
                            || errors.contains("goal changed while terminal work drained"), "{action}: {errors}");
                    } else if action == "output" {
                        assert_eq!(code, 4);
                        assert!(finals.is_empty(), "broken stdout must not be retried");
                    } else {
                        assert_eq!(code, if action == "interrupt" { 130 } else { 6 });
                        assert_eq!(finals.len(), 1, "{action}: {records:?}");
                        assert_eq!(records.last().unwrap()["type"], "goal");
                        assert_eq!(finals[0]["exit_code"], code);
                        assert_eq!(finals[0]["status"], if action == "interrupt" { "interrupted" } else { "paused" });
                        let direct: Vec<_> = records.iter().enumerate().filter(|(_, r)| r["type"] == "goal_run").collect();
                        assert_eq!(direct.len(), 1, "descendants must not produce direct-run records");
                        assert_eq!(direct[0].1["goal"], serde_json::to_value(expected).unwrap());
                        assert_eq!(direct[0].1["run"]["id"], serde_json::to_value(root).unwrap());
                        let finished = records.iter().position(|r| r["envelope"]["event"]["type"] == "run_finished"
                            && r["envelope"]["run_id"] == serde_json::to_value(root).unwrap()).unwrap();
                        assert!(finished < direct[0].0 && direct[0].0 < records.len() - 1);
                        let sequences: Vec<_> = records.iter().filter(|r| r["type"] == "event")
                            .map(|r| r["envelope"]["cursor"]["sequence"].as_u64().expect("event sequence")).collect();
                        assert!(sequences.len() >= 2);
                        assert!(sequences.windows(2).all(|w| w[0] < w[1]));
                    }
                    // Check current identity separately from the old family's finish stream.
                    let durable = fixture.sessions.snapshot(SnapshotRequest::new(
                        handle.workspace_id, Some(handle.session_id), 1, 16,
                    )).await.unwrap().focused.unwrap();
                    if action == "replace" {
                        if let Some(run) = durable.summary.active_run_id {
                            assert_ne!(run, root, "the old family must have drained");
                            let replacement = fixture.sessions.run_snapshot(handle.workspace_id, handle.session_id, run).await.unwrap();
                            assert!(matches!(replacement.origin,
                                Some(qq_protocol::RunOrigin::Goal { goal_id, .. })
                                if goal_id == durable.goal.as_ref().unwrap().goal_id));
                        }
                    } else {
                        assert_eq!(durable.summary.active_run_id, None, "{action}");
                    }
                    if action == "clear" {
                        assert!(durable.goal.is_none());
                    } else {
                        let final_goal = durable.goal.unwrap();
                        if action == "replace" {
                            assert_ne!(final_goal.goal_id, expected.goal_id);
                            assert_eq!(final_goal.objective, "replacement must survive");
                        } else {
                            assert!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&final_goal)).await.unwrap().is_empty());
                        }
                    }
                    let mut finished = Vec::new();
                    while finished.len() < 2 {
                        let event = events.next().await.unwrap().unwrap();
                        if let SessionEvent::RunFinished { run_id, .. } = event.event
                            && [root, child].contains(&run_id) {
                            finished.push(run_id);
                        }
                    }
                    assert_eq!(finished, vec![child, root], "child settles before parent publication");
                    assert_eq!(fixture.sessions.run_snapshot(handle.workspace_id, foreign.session_id, foreign_run).await.unwrap().status, RunStatus::Running);
                    fixture.sessions.close().await.unwrap();
                    return;
                }
                // Failure/owner-drop may return early, but core must retain cleanup ownership.
                progress.cancelled().await;
                assert_eq!(early_code, (action == "output").then_some(4));
                let stopped = fixture.sessions.snapshot(SnapshotRequest::new(
                    handle.workspace_id, Some(handle.session_id), 1, 1,
                )).await.unwrap().focused.unwrap().goal.unwrap();
                assert_eq!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&stopped)).await.unwrap().len(), 2);
                assert!(!parse_records(&String::from_utf8(output.bytes.lock().unwrap().clone()).unwrap())
                    .iter().any(|record| record["type"] == "goal"));
                release.send(()).unwrap();
                exit.await.unwrap();
                let mut finished = Vec::new();
                while finished.len() < 2 {
                    let event = events.next().await.unwrap().unwrap();
                    if let SessionEvent::RunFinished { run_id, .. } = event.event
                        && [root, child].contains(&run_id) { finished.push(run_id); }
                }
                assert_eq!(finished, vec![child, root]);
                let durable = fixture.sessions.snapshot(SnapshotRequest::new(
                    handle.workspace_id, Some(handle.session_id), 1, 16,
                )).await.unwrap().focused.unwrap();
                let durable_goal = durable.goal.unwrap();
                assert_eq!(durable_goal.status, qq_protocol::GoalStatus::Paused { reason: qq_protocol::GoalPauseReason::User });
                assert!(fixture.sessions.goal_pending_runs(handle.session_id, goal_identity(&durable_goal)).await.unwrap().is_empty());
                assert_eq!(fixture.sessions.run_snapshot(handle.workspace_id, foreign.session_id, foreign_run).await.unwrap().status, RunStatus::Running);
                fixture.sessions.close().await.unwrap();
            }).await.unwrap_or_else(|_| panic!("{action}: held-family acceptance hung"));
        }
    }

    #[tokio::test]
    async fn foreground_goal_harness_failure_still_emits_final_record() {
        let fixture = fixture(|| HangingProvider).await;
        let opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        let stop = async {
            loop {
                if matches!(
                    events.next().await.unwrap().unwrap().event,
                    SessionEvent::RunStarted { .. }
                ) {
                    break;
                }
            }
            fixture.sessions.close().await.unwrap();
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut opts = opts;
        opts.session = Some(handle.session_id);
        assert_eq!(
            run_goal(
                &fixture.sessions,
                opts,
                goal_request(),
                stop,
                &mut stdout,
                &mut stderr
            )
            .await,
            4
        );
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        assert_eq!(records.last().unwrap()["type"], "goal");
        assert_eq!(records.last().unwrap()["status"], "harness_failure");
        assert_eq!(records.last().unwrap()["exit_code"], 4);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn foreground_goal_early_trace_failure_keeps_final_stdout_usable() {
        struct LargeText {
            release: Arc<tokio::sync::Notify>,
        }
        impl Provider for LargeText {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let release = Arc::clone(&self.release);
                Box::pin(
                    stream::once(async move {
                        release.notified().await;
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "x".repeat(16384),
                        })
                    })
                    .chain(stream::pending()),
                )
            }
        }
        let release = Arc::new(tokio::sync::Notify::new());
        let provider_release = Arc::clone(&release);
        let fixture = fixture(move || LargeText {
            release: Arc::clone(&provider_release),
        })
        .await;
        let mut opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        let mut request = goal_request();
        request.objective = Some("x".repeat(2048));
        let command = prepare_goal_request(&request, &opts, handle.session_id, None)
            .unwrap()
            .unwrap();
        send(&fixture.sessions, command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    events.next().await.unwrap().unwrap().event,
                    SessionEvent::RunStarted { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        opts.session = Some(handle.session_id);
        opts.trace = Some(PathBuf::from("/dev/full"));
        request.objective = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(10),
            run_goal(
                &fixture.sessions,
                opts,
                request,
                async {
                    // run_goal first polls interruption after subscribing.
                    release.notify_one();
                    std::future::pending::<()>().await;
                },
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .expect("trace failure cleanup must finish");
        assert_eq!(code, 4);
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let finals: Vec<_> = records
            .iter()
            .filter(|record| record["type"] == "goal")
            .collect();
        assert_eq!(
            finals.len(),
            1,
            "usable stdout needs a final harness result"
        );
        assert_eq!(records.last().unwrap()["type"], "goal");
        assert_eq!(finals[0]["status"], "harness_failure");
        assert_eq!(finals[0]["exit_code"], 4);
        assert!(!stderr.is_empty());
        let runs: Vec<_> = records
            .iter()
            .filter(|record| record["type"] == "goal_run")
            .collect();
        assert_eq!(
            runs.len(),
            1,
            "error cleanup must emit the settled direct run"
        );
        assert!(runs[0]["run"]["outcome"].is_object());
        assert!(finals[0]["goal"]["pending_run_id"].is_null());
        let durable = fixture
            .sessions
            .snapshot(SnapshotRequest::new(
                handle.workspace_id,
                Some(handle.session_id),
                1,
                1,
            ))
            .await
            .unwrap()
            .focused
            .unwrap()
            .goal
            .unwrap();
        assert_eq!(
            finals[0]["goal"]["counters"],
            serde_json::to_value(durable.counters).unwrap()
        );
        fixture.sessions.close().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn foreground_goal_trace_flush_does_not_duplicate_the_final_record() {
        let fixture = fixture(|| HangingProvider).await;
        let mut opts = options(&fixture.workspace);
        opts.trace = Some(PathBuf::from("/dev/full"));
        let mut request = goal_request();
        request.check = Some("printf checked".into());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_goal(
            &fixture.sessions,
            opts,
            request,
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        )
        .await;
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let finals: Vec<_> = records
            .iter()
            .filter(|record| record["type"] == "goal")
            .collect();
        assert_eq!(
            finals.len(),
            1,
            "a flushed final record must never be retried"
        );
        assert_eq!(finals[0]["exit_code"], code);
        assert_eq!(code, 6);
        assert!(
            !stderr.is_empty(),
            "the trace failure must still be reported"
        );
        let goal: Box<qq_protocol::GoalSnapshot> =
            serde_json::from_value(finals[0]["goal"].clone()).unwrap();
        let workspace = serde_json::from_value(finals[0]["workspace_id"].clone()).unwrap();
        let mut trace = BufWriter::with_capacity(8192, std::fs::File::create("/dev/full").unwrap());
        trace.write_all(&vec![b' '; 8191]).unwrap();
        let mut sink = RecordSink {
            goal_record: GoalRecordState::Pending,
            stdout_failed: false,
            trace: Some(trace),
            to_stdout: true,
        };
        let mut final_stdout = Vec::new();
        assert!(
            finish_goal(
                &mut sink,
                &mut final_stdout,
                &options(&fixture.workspace),
                workspace,
                goal,
                None
            )
            .is_err()
        );
        let records = parse_records(&String::from_utf8(final_stdout).unwrap());
        assert_eq!(
            records.len(),
            1,
            "final stdout must survive an implicit trace-buffer flush failure"
        );
        assert_eq!(records[0]["exit_code"], 6);
        assert!(matches!(sink.goal_record, GoalRecordState::Emitted(6)));
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_goal_output_failure_pauses_active_work_and_preserves_stopped_reason() {
        for needs_consent in [false, true] {
            let fixture = fixture(|| HangingProvider).await;
            let mut options = options(&fixture.workspace);
            let handle = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
                .await
                .unwrap();
            options.session = Some(handle.session_id);
            let mut request = goal_request();
            if needs_consent {
                request.check = Some("printf checked".into());
            }
            let mut stderr = Vec::new();
            assert_eq!(
                run_goal(
                    &fixture.sessions,
                    options,
                    request,
                    std::future::pending(),
                    &mut BrokenWriter,
                    &mut stderr
                )
                .await,
                4
            );
            assert!(String::from_utf8_lossy(&stderr).contains("injected output failure"));
            let snapshot = fixture
                .sessions
                .snapshot(SnapshotRequest::new(
                    handle.workspace_id,
                    Some(handle.session_id),
                    1,
                    1,
                ))
                .await
                .unwrap();
            let goal = snapshot.focused.unwrap().goal.unwrap();
            assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: if needs_consent {
                        qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::CheckApproval,
                        }
                    } else {
                        qq_protocol::GoalPauseReason::User
                    }
                }
            );
            assert_eq!(goal.revision, if needs_consent { 1 } else { 2 });
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_goal_owner_abort_durably_pauses_accepted_goal() {
        struct AcceptanceSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Write for AcceptanceSignal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                if let Some(entered) = self.0.take() {
                    entered.send(()).unwrap();
                }
                Ok(())
            }
        }
        let fixture = fixture(|| HangingProvider).await;
        let mut options = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &options, SessionIntent::Prompt)
            .await
            .unwrap();
        options.session = Some(handle.session_id);
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        let (entered, accepted) = tokio::sync::oneshot::channel();
        let sessions = fixture.sessions.clone();
        let owner = tokio::spawn(async move {
            run_goal(
                &sessions,
                options,
                goal_request(),
                std::future::pending(),
                &mut AcceptanceSignal(Some(entered)),
                &mut Vec::new(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), accepted)
            .await
            .unwrap()
            .unwrap();
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.next().await.unwrap().unwrap();
                if let SessionEvent::GoalUpdated { goal } = event.event
                    && goal.session_id == handle.session_id
                    && goal.status
                        == (qq_protocol::GoalStatus::Paused {
                            reason: qq_protocol::GoalPauseReason::User,
                        })
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        fixture.sessions.close().await.unwrap();
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
    async fn ask_mode_denies_mutations_even_with_explicit_allowlists() {
        let fixture = fixture(MutatingProvider::new).await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Ask;
        options.allow_tools = vec!["write_file".to_owned(), "shell".to_owned()];
        options.allow_shell_prefixes = vec!["cp".to_owned()];
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

    #[test]
    fn explicit_allowlists_cannot_resolve_auto_supervision() {
        let mut options = options(Path::new("."));
        options.allow_tools = vec!["shell".into()];
        assert_eq!(
            allowlisted_grant(&options, HeadlessApproval::Auto, "shell", None, None),
            None,
            "a root allowlist must not impersonate its child's reviewer"
        );
        assert_eq!(
            allowlisted_grant(&options, HeadlessApproval::Full, "shell", None, None),
            Some(ApprovalGrant::Tool {
                name: "shell".into()
            })
        );
    }

    /// Explicit prefixes are stored before dispatch, including on resume.
    /// A nonmatching prefix never grants authority to the requested command.
    #[tokio::test]
    async fn shell_allowlist_grants_the_session_before_dispatch() {
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

    /// An explicit Auto tool grant is installed before the first call.
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

    /// Public fetch needs no per-host grant; legacy flags never add a hold.
    #[tokio::test]
    async fn auto_fetch_dispatches_public_hosts_with_or_without_host_flags() {
        for with_host_flag in [false, true] {
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
            if with_host_flag {
                options.allow_hosts = vec!["127.0.0.1".into()];
            }
            let (status, stdout, stderr) =
                run_to_end(&fixture, options, std::future::pending()).await;
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
            assert!(event_records(&records).iter().any(
                |record| record["envelope"]["event"]["type"] == "tool_call_started"
                    && record["envelope"]["event"]["tool_call"]["name"] == "fetch"
            ));
            let request = tokio::task::spawn_blocking(move || server.capture())
                .await
                .unwrap();
            assert_eq!(request.request_line(), Some("GET /docs HTTP/1.1"));
            fixture.sessions.close().await.unwrap();
        }
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
    async fn session_resume_preserves_ask_and_its_unattended_policy_without_an_override() {
        let fixture = fixture(MutatingProvider::new).await;
        let mut initial = options(&fixture.workspace);
        initial.approval = HeadlessApproval::Ask;
        let stored = prepare_session(&fixture.sessions, &initial, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut resumed = options(&fixture.workspace);
        resumed.session = Some(stored.session_id);
        resumed.approval = HeadlessApproval::Auto;
        resumed.reviewer_configured = true;
        resumed.configured_delegate = qq_protocol::ApprovalDelegate::On;
        resumed.allow_tools = vec!["write_file".into(), "shell".into()];
        resumed.allow_shell_prefixes = vec!["cp".into()];
        let (status, stdout, stderr) = tokio::time::timeout(
            REVIEWER_DENY_GRACE / 2,
            run_to_end(&fixture, resumed, std::future::pending()),
        )
        .await
        .expect("stored Ask must deny unattended immediately, without waiting for a reviewer");
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        assert!(
            !fixture.workspace.join("note.txt").exists(),
            "stored Ask was widened to Auto"
        );
        assert!(!fixture.workspace.join("shelled.txt").exists());
        let records = parse_records(&stdout);
        assert_eq!(
            records[0]["approval"], "ask",
            "trial must name the effective policy"
        );
        let resolutions: Vec<_> = event_records(&records)
            .into_iter()
            .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
            .collect();
        assert_eq!(resolutions.len(), 2, "{stdout}");
        assert!(
            resolutions
                .iter()
                .all(|record| record["envelope"]["event"]["resolution"] == "denied"),
            "{stdout}"
        );
        assert_eq!(
            workspace_snapshot(&fixture).await.sessions[0].approval_mode,
            ApprovalMode::Ask
        );
    }

    #[tokio::test]
    async fn session_resume_permission_selection_is_shared_by_prompt_and_goal() {
        for stored_mode in [
            HeadlessApproval::ReadOnly,
            HeadlessApproval::Ask,
            HeadlessApproval::Auto,
            HeadlessApproval::Full,
        ] {
            for explicit in [false, true] {
                for goal in [false, true] {
                    let fixture = fixture(|| TextProvider).await;
                    let mut initial = options(&fixture.workspace);
                    initial.approval = stored_mode;
                    let stored =
                        prepare_session(&fixture.sessions, &initial, SessionIntent::Prompt)
                            .await
                            .unwrap();
                    let mut resumed = options(&fixture.workspace);
                    resumed.session = Some(stored.session_id);
                    resumed.approval = if stored_mode == HeadlessApproval::Auto {
                        HeadlessApproval::Ask
                    } else {
                        HeadlessApproval::Auto
                    };
                    resumed.approval_explicit = explicit;
                    resumed.reviewer_configured = true;
                    resumed.configured_delegate = qq_protocol::ApprovalDelegate::On;
                    let request = goal_request();
                    let intent = if goal {
                        SessionIntent::Goal(&request)
                    } else {
                        SessionIntent::Prompt
                    };
                    let effective = prepare_session(&fixture.sessions, &resumed, intent)
                        .await
                        .unwrap();
                    let expected = if explicit {
                        resumed.approval
                    } else {
                        stored_mode
                    };
                    assert_eq!(effective.approval, expected);
                    assert_eq!(
                        effective.wait_for_delegate,
                        expected == HeadlessApproval::Auto
                    );
                    assert_eq!(
                        effective.child_waits_for_delegate,
                        expected != HeadlessApproval::Ask
                    );
                    assert_eq!(
                        workspace_snapshot(&fixture).await.sessions[0].approval_mode,
                        approval_mode(expected)
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn goal_resume_preserves_ask_for_unattended_mutations() {
        let fixture = fixture(MutatingProvider::new).await;
        let mut initial = options(&fixture.workspace);
        initial.approval = HeadlessApproval::Ask;
        let stored = prepare_session(&fixture.sessions, &initial, SessionIntent::Prompt)
            .await
            .unwrap();
        let mut resumed = options(&fixture.workspace);
        resumed.session = Some(stored.session_id);
        resumed.approval = HeadlessApproval::Auto;
        resumed.allow_tools = vec!["write_file".into(), "shell".into()];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_goal(
            &fixture.sessions,
            resumed,
            goal_request(),
            std::future::pending(),
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(
            code,
            qq_protocol::HeadlessGoalStatus::BudgetExhausted.code(),
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(!fixture.workspace.join("note.txt").exists());
        assert!(!fixture.workspace.join("shelled.txt").exists());
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let resolutions: Vec<_> = event_records(&records)
            .into_iter()
            .filter(|record| record["envelope"]["event"]["type"] == "tool_approval_resolved")
            .collect();
        assert_eq!(resolutions.len(), 2);
        assert!(
            resolutions
                .iter()
                .all(|record| record["envelope"]["event"]["resolution"] == "denied")
        );
        assert_eq!(
            workspace_snapshot(&fixture).await.sessions[0].approval_mode,
            ApprovalMode::Ask
        );
    }

    #[tokio::test]
    async fn session_resume_read_only_uses_its_effective_denial_hint() {
        let fixture = fixture(MutatingProvider::new).await;
        let stored = prepare_session(
            &fixture.sessions,
            &options(&fixture.workspace),
            SessionIntent::Prompt,
        )
        .await
        .unwrap();
        let mut resumed = options(&fixture.workspace);
        resumed.session = Some(stored.session_id);
        resumed.approval = HeadlessApproval::Auto;
        resumed.format = HeadlessFormat::Text;
        let (status, _, stderr) = run_to_end(&fixture, resumed, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        assert!(stderr.contains(DENIAL_HINT), "{stderr}");
        assert!(!fixture.workspace.join("note.txt").exists());
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
            approval_explicit: true,
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
                reasoning_effort: None,
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
                reasoning_effort: None,
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
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = root.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(&workspace).unwrap();
        let database = root.join("sessions.sqlite3");

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
            std::future::pending(),
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
        assert_eq!(outcomes[0]["prompt_identity"]["version"], 24);
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

    const DENIAL_HINT: &str = "held calls were denied under --approval read-only; rerun with --approval auto to allow workspace edits";

    #[tokio::test]
    async fn text_read_only_denials_end_with_the_approval_auto_hint_before_the_resume_hint() {
        let fixture = fixture(MutatingProvider::new).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;
        options.resume_hint = true;

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(
            status,
            HeadlessStatus::Completed,
            "the hint never changes the exit status"
        );
        assert_eq!(stdout, "done\n", "the hint stays off stdout");
        assert!(stderr.contains("[tool] write_file denied"), "{stderr}");
        let hint_at = stderr
            .find(DENIAL_HINT)
            .unwrap_or_else(|| panic!("no denial hint in:\n{stderr}"));
        let session_id = workspace_snapshot(&fixture).await.sessions[0].id;
        let resume = crate::cli::resume_hint(session_id);
        assert!(stderr.ends_with(&resume), "{stderr}");
        assert!(
            hint_at < stderr.len() - resume.len(),
            "the denial hint precedes the resume hint:\n{stderr}"
        );
        assert_eq!(stderr.matches(DENIAL_HINT).count(), 1, "{stderr}");
    }

    struct CountingReviewer {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        escalate: bool,
    }

    impl qq_core::ApprovalReviewer for CountingReviewer {
        fn review(&self, _request: qq_core::ReviewRequest) -> qq_core::ReviewFuture {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let decision = if self.escalate {
                qq_core::ReviewDecision::Escalate {
                    reason: "reviewer abstained".to_owned(),
                }
            } else {
                qq_core::ReviewDecision::Approve
            };
            Box::pin(async move { qq_core::ReviewVerdict::free(decision) })
        }
    }

    async fn root_shell_reviewer_fixture(
        escalate: bool,
    ) -> (Fixture, Arc<std::sync::atomic::AtomicUsize>) {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = root.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(&workspace).unwrap();
        for name in ["scratch0", "scratch1"] {
            std::fs::create_dir_all(workspace.join(name)).unwrap();
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime_options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
        runtime_options.approval_reviewer = Some(Arc::new(CountingReviewer {
            calls: Arc::clone(&calls),
            escalate,
        }));
        let sessions = SessionRuntime::open(
            runtime_options,
            Arc::new(ProviderLoader(|| DangerousShellProvider {
                turn: Mutex::new(0),
            })),
        )
        .await
        .unwrap();
        (
            Fixture {
                sessions,
                workspace,
                _directory: directory,
            },
            calls,
        )
    }

    fn assert_root_shell_denied_without_review(
        fixture: &Fixture,
        calls: &std::sync::atomic::AtomicUsize,
        records: &[serde_json::Value],
    ) {
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let finished = finished_tool_calls(records);
        assert_eq!(finished.len(), 2, "{records:?}");
        assert!(finished.iter().all(|call| call["state"] == "denied"));
        assert!(
            event_records(records).iter().all(|record| {
                !record["envelope"]["event"]["type"]
                    .as_str()
                    .unwrap()
                    .starts_with("tool_approval_")
            }),
            "missing authority must not produce approval events: {records:?}"
        );
        for name in ["scratch0", "scratch1"] {
            assert!(fixture.workspace.join(name).is_dir());
        }
    }

    /// A loaded configuration with a `reviewer_model`, read by the real
    /// loader from an explicit document.
    fn reviewer_snapshot(workspace: &Path) -> qq_config::ConfigSnapshot {
        let root = workspace.parent().unwrap();
        let loader = qq_config::ConfigLoader::new(qq_config::ConfigPaths::new(
            root.join("cfg-global"),
            root.join("cfg-data"),
            root.join("cfg-managed"),
        ));
        loader
            .load(&qq_config::LoadRequest::new(workspace).with_explicit_content(
                r#"(version: 1, model: "test/model", reviewer_model: "test/model",
                    providers: { "test": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: { "model": (name: "m") }) })"#,
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn auto_children_fetch_public_hosts_but_explicit_managed_denies_prevent_dispatch() {
        struct ChildFetchProvider {
            child: bool,
            first: Mutex<bool>,
            url: String,
        }
        impl Provider for ChildFetchProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                let first = std::mem::replace(&mut *self.first.lock().unwrap(), false);
                if !first {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".into(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let (name, arguments) = if self.child {
                    ("fetch", serde_json::json!({"url": self.url}).to_string())
                } else {
                    (
                        "spawn_agent",
                        r#"{"task":"read docs","model":"test/child","authority":"write"}"#
                            .to_owned(),
                    )
                };
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "probe".into(),
                        name: name.into(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "probe".into(),
                        json: arguments,
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id: "probe".into() }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        struct ChildFetchLoader {
            deny_host: bool,
            url: String,
        }
        impl RuntimeLoader for ChildFetchLoader {
            fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let deny_host = self.deny_host;
                let url = self.url.clone();
                Box::pin(async move {
                    let runtime = Runtime::new(
                        ChildFetchProvider {
                            child: request.model.model.as_deref() == Some("test/child"),
                            first: Mutex::new(true),
                            url,
                        },
                        "test-model",
                        256,
                    )
                    .unwrap()
                    .with_network_policy(qq_core::NetworkPolicy {
                        deny_hosts: if deny_host {
                            Arc::from(["docs.invalid".to_owned()])
                        } else {
                            Arc::from([])
                        },
                        allow_private_for_tests: true,
                    })
                    .with_spawn_model_routes(vec!["test/child".into()])
                    .with_delegation(qq_protocol::DelegationRoster {
                        roster: Vec::new(),
                        default_role: qq_protocol::DelegationRole::Balanced,
                        max_depth: 1,
                        write_children: true,
                    });
                    Ok(loaded_runtime(runtime, &request.workspace, None))
                })
            }
        }
        struct DenyChildFetch(Arc<std::sync::atomic::AtomicUsize>);
        impl qq_core::ApprovalReviewer for DenyChildFetch {
            fn review(&self, _request: qq_core::ReviewRequest) -> qq_core::ReviewFuture {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    qq_core::ReviewVerdict::free(qq_core::ReviewDecision::Deny {
                        reason: "stop before network dispatch".into(),
                    })
                })
            }
        }
        for (approval, deny_host) in [
            (HeadlessApproval::Full, false),
            (HeadlessApproval::Auto, false),
            (HeadlessApproval::Full, true),
            (HeadlessApproval::Auto, true),
        ] {
            let server = (!deny_host).then(|| {
                qq_provider::test_support::LoopbackServer::respond(
                    200,
                    "text/plain",
                    "fixture-child-docs",
                )
            });
            let url = server
                .as_ref()
                .map(|server| format!("{}/docs", server.base_url))
                .unwrap_or_else(|| "https://docs.invalid/page".into());
            let directory = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(directory.path()).unwrap();
            let workspace = root.join("work");
            std::fs::create_dir(&workspace).unwrap();
            let consulted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut runtime_options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
            runtime_options.max_active_runs = 2;
            runtime_options.approval_reviewer =
                Some(Arc::new(DenyChildFetch(Arc::clone(&consulted))));
            let fixture = Fixture {
                sessions: SessionRuntime::open(
                    runtime_options,
                    Arc::new(ChildFetchLoader { deny_host, url }),
                )
                .await
                .unwrap(),
                workspace,
                _directory: directory,
            };
            let mut options = options(&fixture.workspace);
            options.approval = approval;
            options.allow_hosts = vec!["docs.invalid".into()];
            options.reviewer_configured = true;
            let (status, stdout, _) = run_to_end(&fixture, options, std::future::pending()).await;
            assert_eq!(status, HeadlessStatus::Completed, "{approval:?}: {stdout}");
            let records = parse_records(&stdout);
            let events = event_records(&records);
            let child = events
                .iter()
                .find_map(|record| {
                    let event = &record["envelope"]["event"];
                    (event["type"] == "session_created"
                        && event["session"]["parent_id"].is_string())
                    .then(|| event["session"].clone())
                })
                .expect("actual write child must be created");
            assert_eq!(child["approval_mode"], "auto");
            assert_eq!(
                consulted.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{approval:?}: public research never needs a reviewer: {stdout}"
            );
            let fetch = events
                .iter()
                .find_map(|record| {
                    let envelope = &record["envelope"];
                    let event = &envelope["event"];
                    (envelope["session_id"] == child["id"]
                        && event["type"] == "tool_call_finished"
                        && event["tool_call"]["name"] == "fetch")
                        .then(|| event["tool_call"].clone())
                })
                .expect("the child fetch settles");
            assert_eq!(
                fetch["state"],
                if deny_host { "denied" } else { "completed" },
                "{stdout}"
            );
            assert!(
                fetch["result"]
                    .as_str()
                    .is_some_and(|result| result.contains(if deny_host {
                        "denied by managed policy"
                    } else {
                        "fixture-child-docs"
                    })),
                "{stdout}"
            );
            let started = events.iter().any(|record| {
                let event = &record["envelope"]["event"];
                event["type"] == "tool_call_started" && event["tool_call"]["name"] == "fetch"
            });
            assert_eq!(
                started, !deny_host,
                "managed denies precede dispatch even with host flags: {stdout}"
            );
            assert!(
                !events
                    .iter()
                    .any(|record| record["envelope"]["event"]["type"]
                        .as_str()
                        .is_some_and(|kind| kind.starts_with("tool_approval_"))),
                "{stdout}"
            );
            assert!(
                events.iter().any(|record| {
                    let envelope = &record["envelope"];
                    envelope["session_id"] == child["id"]
                        && envelope["event"]["type"] == "run_finished"
                        && envelope["event"]["outcome"]["type"] == "completed"
                }),
                "child must settle: {stdout}"
            );
            if let Some(server) = server {
                let request = tokio::task::spawn_blocking(move || server.capture())
                    .await
                    .unwrap();
                assert_eq!(request.request_line(), Some("GET /docs HTTP/1.1"));
            }
            fixture.sessions.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn root_auto_without_authority_does_not_consult_a_configured_reviewer() {
        let (fixture, calls) = root_shell_reviewer_fixture(false).await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        let snapshot = reviewer_snapshot(&fixture.workspace);
        let (reviewer_configured, configured_delegate) = crate::headless_delegate_options(
            &snapshot,
            &qq_protocol::AgentProfileId::default(),
            None,
        );
        assert!(reviewer_configured);
        options.reviewer_configured = reviewer_configured;
        options.configured_delegate = configured_delegate;
        let (status, stdout, _) =
            run_to_end(&fixture, options.clone(), std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed);
        assert_root_shell_denied_without_review(&fixture, &calls, &parse_records(&stdout));

        options.format = HeadlessFormat::Text;
        let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed);
        assert!(stderr.contains("[tool] shell denied"), "{stderr}");
        assert!(!stderr.contains("approved by reviewer"), "{stderr}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        for name in ["scratch0", "scratch1"] {
            assert!(fixture.workspace.join(name).is_dir());
        }
    }

    #[tokio::test]
    async fn root_auto_denies_without_waiting_for_an_escalating_reviewer() {
        let (fixture, calls) = root_shell_reviewer_fixture(true).await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        options.reviewer_configured = true;
        let (status, stdout, _) = tokio::time::timeout(
            REVIEWER_DENY_GRACE / 2,
            run_to_end(&fixture, options, std::future::pending()),
        )
        .await
        .expect("missing authority must deny without waiting for reviewer escalation");
        assert_eq!(status, HeadlessStatus::Completed);
        assert_root_shell_denied_without_review(&fixture, &calls, &parse_records(&stdout));
    }

    #[tokio::test]
    async fn a_delegate_that_is_configured_but_switched_off_does_not_delay_the_denial() {
        let (fixture, calls) = root_shell_reviewer_fixture(false).await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Auto;
        options.reviewer_configured = true;
        options.configured_delegate = qq_protocol::ApprovalDelegate::Off;
        let (status, stdout, _) = tokio::time::timeout(
            REVIEWER_DENY_GRACE / 2,
            run_to_end(&fixture, options.clone(), std::future::pending()),
        )
        .await
        .expect("the denial must not wait for a delegate that is switched off");
        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        assert_root_shell_denied_without_review(&fixture, &calls, &records);

        let session_id: SessionId = records[0]["session_id"].as_str().unwrap().parse().unwrap();
        send(
            &fixture.sessions,
            SessionCommand::SetApprovalDelegate {
                session_id,
                delegate: Some(qq_protocol::ApprovalDelegate::Off),
            },
        )
        .await
        .unwrap();
        let resumed = HeadlessOptions {
            session: Some(session_id),
            approval: HeadlessApproval::Auto,
            reviewer_configured: true,
            configured_delegate: qq_protocol::ApprovalDelegate::ByMode,
            ..options
        };
        let (status, stdout, _) = tokio::time::timeout(
            REVIEWER_DENY_GRACE / 2,
            run_to_end(&fixture, resumed, std::future::pending()),
        )
        .await
        .expect("the session's off override must not wait either");
        assert_eq!(status, HeadlessStatus::Completed);
        assert_root_shell_denied_without_review(&fixture, &calls, &parse_records(&stdout));
    }

    #[test]
    fn delegated_resolution_retains_reviewer_identity_in_jsonl_and_text() {
        // Real child supervision is covered in qq-core; this adapter must
        // preserve its typed result without treating a root denial as review.
        for (resolution, expected) in [
            (ApprovalResolution::ApprovedByReviewer, "approved"),
            (ApprovalResolution::DeniedByReviewer, "denied"),
        ] {
            let tool_call = qq_protocol::ToolCallSnapshot {
                id: qq_protocol::ToolCallId::from_bytes([1; 16]),
                session_id: SessionId::from_bytes([2; 16]),
                run_id: RunId::from_bytes([3; 16]),
                turn_ordinal: 0,
                call_ordinal: 0,
                provider_call_id: "child-call".into(),
                name: "write_file".into(),
                arguments: r#"{"path":"note.txt","content":"child work"}"#.into(),
                state: if resolution == ApprovalResolution::DeniedByReviewer {
                    ToolCallState::Denied
                } else {
                    ToolCallState::Completed
                },
                result: None,
                is_error: resolution == ApprovalResolution::DeniedByReviewer,
                display: None,
            };
            let envelope = SessionEventEnvelope {
                cursor: qq_protocol::EventCursor {
                    store_id: qq_protocol::StoreId::from_bytes([4; 16]),
                    workspace_id: WorkspaceId::from_bytes([5; 16]),
                    sequence: 1,
                },
                session_id: tool_call.session_id,
                run_id: Some(tool_call.run_id),
                caused_by: None,
                occurred_at_ms: 1,
                event: SessionEvent::ToolApprovalResolved {
                    tool_call,
                    resolution,
                    delegate: Some(qq_protocol::DelegateIdentity::Reviewer),
                },
            };
            let mut sink = RecordSink {
                goal_record: GoalRecordState::Pending,
                stdout_failed: false,
                trace: None,
                to_stdout: true,
            };
            let mut stdout = Vec::new();
            sink.record(
                &mut stdout,
                HeadlessRecordRef::Event {
                    envelope: &envelope,
                },
            )
            .unwrap();
            let records = parse_records(&String::from_utf8(stdout).unwrap());
            assert_eq!(records[0]["envelope"]["event"]["delegate"], "reviewer");
            assert_eq!(
                records[0]["envelope"]["event"]["resolution"],
                serde_json::to_value(resolution).unwrap()
            );
            let SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution,
                delegate,
            } = &envelope.event
            else {
                unreachable!()
            };
            let mut stderr = Vec::new();
            write_delegate_resolution(&mut stderr, &tool_call.name, *resolution, *delegate)
                .unwrap();
            assert_eq!(
                String::from_utf8(stderr).unwrap(),
                format!("[tool] write_file {expected} by reviewer\n")
            );
            assert!(
                write_delegate_resolution(
                    &mut BrokenWriter,
                    &tool_call.name,
                    *resolution,
                    *delegate,
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn the_denial_hint_is_silent_under_auto() {
        // `auto` executes the same calls: nothing to explain.
        let fixture = fixture(MutatingProvider::new).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;
        options.approval = HeadlessApproval::Auto;

        let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        assert!(!stderr.contains(DENIAL_HINT), "{stderr}");
    }

    #[tokio::test]
    async fn the_denial_hint_is_silent_in_jsonl_where_records_carry_the_denials() {
        let fixture = fixture(MutatingProvider::new).await;
        let options = options(&fixture.workspace);

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        let records = parse_records(&stdout);
        let calls = finished_tool_calls(&records);
        assert!(!calls.is_empty());
        assert!(calls.iter().all(|call| call["state"] == "denied"));
        assert!(!stderr.contains(DENIAL_HINT), "{stderr}");
    }

    #[tokio::test]
    async fn the_denial_hint_is_silent_when_read_only_denied_nothing() {
        let fixture = fixture(|| TextProvider).await;
        let mut options = options(&fixture.workspace);
        options.format = HeadlessFormat::Text;

        let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(status, HeadlessStatus::Completed);
        assert!(!stderr.contains(DENIAL_HINT), "{stderr}");
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
        struct AcceptanceSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Write for AcceptanceSignal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                if let Some(accepted) = self.0.take() {
                    accepted.send(()).unwrap();
                }
                Ok(())
            }
        }
        let fixture = fixture(|| HangingProvider).await;
        let options = options(&fixture.workspace);
        let before = workspace_snapshot(&fixture).await;
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: before.workspace.id,
                after: before.cursor,
            })
            .unwrap();
        let (entered, accepted) = tokio::sync::oneshot::channel();
        let sessions = fixture.sessions.clone();
        let owner = tokio::spawn(async move {
            let mut stdout = AcceptanceSignal(Some(entered));
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

        // The first output flush follows installation of the accepted-run
        // guard. A snapshot can observe acceptance before that guard exists.
        tokio::time::timeout(Duration::from_secs(5), accepted)
            .await
            .expect("the headless owner must accept a run before it is aborted")
            .unwrap();
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let SessionEvent::RunFinished { outcome, .. } =
                    events.next().await.unwrap().unwrap().event
                {
                    assert_eq!(outcome, RunOutcome::Cancelled);
                    break;
                }
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
        let fixture = fixture_with_loader(Arc::new(ParentChildLoader {
            parent,
            child,
            held_child: None,
        }))
        .await;
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
    #[tokio::test]
    async fn settled_goal_run_records_survive_delivery_history_retirement() {
        let fixture = fixture(|| TextProvider).await;
        let mut original = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run_goal(
                &fixture.sessions,
                options(&fixture.workspace),
                goal_request(),
                std::future::pending(),
                &mut original,
                &mut stderr
            )
            .await,
            8
        );
        let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(original)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let terminal = records
            .iter()
            .find_map(|record| match record {
                qq_protocol::HeadlessRecord::Goal(goal) => Some(goal),
                _ => None,
            })
            .unwrap();
        let expected_runs: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                qq_protocol::HeadlessRecord::GoalRun(run) => Some(run.run.id),
                _ => None,
            })
            .collect();
        assert_eq!(expected_runs.len(), 1);
        let workspace = terminal.workspace_id;
        let session = terminal.goal.session_id;
        let through = fixture
            .sessions
            .snapshot(SnapshotRequest::new(workspace, Some(session), 1, 1))
            .await
            .unwrap()
            .cursor;
        let after = qq_protocol::EventCursor {
            sequence: 0,
            ..through
        };
        let database = fixture._directory.path().join("sessions.sqlite3");
        tokio::task::spawn_blocking(move || {
            let connection = rusqlite::Connection::open(database).unwrap();
            connection.execute("DELETE FROM events", []).unwrap();
        })
        .await
        .unwrap();
        let handle = SessionHandle {
            approval: HeadlessApproval::ReadOnly,
            workspace_id: workspace,
            session_id: session,
            subscribe_after: after,
            wait_for_delegate: false,
            child_waits_for_delegate: false,
        };
        let mut sink = RecordSink::open(&options(&fixture.workspace)).unwrap();
        let mut replayed = Vec::new();
        let result = replay_settled_goal_runs(
            &fixture.sessions,
            &mut sink,
            &mut replayed,
            &handle,
            goal_identity(&terminal.goal),
            after,
            through,
        )
        .await;
        assert!(
            result.is_ok(),
            "completed task receipts must not require an unbounded delivery log: {result:?}"
        );
        let replayed_runs: Vec<RunId> = String::from_utf8(replayed)
            .unwrap()
            .lines()
            .map(
                |line| match serde_json::from_str::<qq_protocol::HeadlessRecord>(line).unwrap() {
                    qq_protocol::HeadlessRecord::GoalRun(run) => run.run.id,
                    other => panic!("unexpected record: {other:?}"),
                },
            )
            .collect();
        assert_eq!(replayed_runs, expected_runs);
        fixture.sessions.close().await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_run_recovers_its_answer_after_delivery_history_expires() {
        struct StalledOutput {
            sessions: SessionRuntime,
            database: PathBuf,
            retired: bool,
            bytes: Vec<u8>,
        }
        impl Write for StalledOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.retired
                    && let Ok(qq_protocol::HeadlessRecord::Trial(trial)) =
                        serde_json::from_slice(bytes)
                {
                    // Model a consumer blocked at its first record while the
                    // independent runtime completes and delivery history expires.
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                        let snapshot = recovery_snapshot(&self.sessions,trial.workspace_id,trial.session_id).await.unwrap();
                        let mut events = subscribe_current(&self.sessions,&snapshot).unwrap();
                        if self.sessions.run_snapshot(trial.workspace_id,trial.session_id,trial.run_id).await.unwrap().outcome.is_none() {
                            loop {
                                let envelope = events.next().await.unwrap().unwrap();
                                if matches!(envelope.event,SessionEvent::RunFinished { run_id,.. } if run_id==trial.run_id) { break; }
                            }
                        }
                        drop(events);
                        let database = self.database.clone();
                        tokio::task::spawn_blocking(move || {
                            rusqlite::Connection::open(database).unwrap().execute("DELETE FROM events",[]).unwrap();
                        }).await.unwrap();
                    })
                    });
                    self.retired = true;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        struct CountedTextProvider(Arc<std::sync::atomic::AtomicUsize>);
        impl Provider for CountedTextProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                TextProvider.stream(request)
            }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let fixture = fixture(move || CountedTextProvider(Arc::clone(&observed))).await;
        let mut output = StalledOutput {
            sessions: fixture.sessions.clone(),
            database: fixture
                ._directory
                .path()
                .canonicalize()
                .unwrap()
                .join("sessions.sqlite3"),
            retired: false,
            bytes: Vec::new(),
        };
        let mut stderr = Vec::new();
        let status = run(
            &fixture.sessions,
            options(&fixture.workspace),
            std::future::pending(),
            None,
            &mut output,
            &mut stderr,
        )
        .await;
        assert_eq!(
            status,
            HeadlessStatus::Completed,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(output.retired);
        let records = parse_records(&String::from_utf8(output.bytes).unwrap());
        assert_eq!(records.last().unwrap()["recovered_answer"], "hello");
        assert_eq!(records.last().unwrap()["status"], "completed");
        assert_eq!(records.last().unwrap()["exit_code"], 0);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "recovering a finished task must not send another provider request"
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_auto_goal_denies_without_human_wait_with_a_reviewer_configured() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workspace = root.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut runtime_options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
        runtime_options.approval_reviewer = Some(Arc::new(CountingReviewer {
            escalate: true,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        let sessions = SessionRuntime::open(
            runtime_options,
            Arc::new(ProviderLoader(|| DangerousShellProvider {
                turn: Mutex::new(0),
            })),
        )
        .await
        .unwrap();
        let fixture = Fixture {
            sessions,
            workspace,
            _directory: directory,
        };
        let mut opts = options(&fixture.workspace);
        opts.approval = HeadlessApproval::Auto;
        opts.reviewer_configured = true;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = tokio::time::timeout(
            REVIEWER_DENY_GRACE / 2,
            run_goal(
                &fixture.sessions,
                opts,
                goal_request(),
                std::future::pending(),
                &mut stdout,
                &mut stderr,
            ),
        )
        .await
        .expect("the reviewer has passed; no human is attached to this goal");
        assert_eq!(code, 8, "{}", String::from_utf8_lossy(&stderr));
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        let terminal = records.last().unwrap();
        let workspace = serde_json::from_value(terminal["workspace_id"].clone()).unwrap();
        let session = serde_json::from_value(terminal["goal"]["session_id"].clone()).unwrap();
        let body = recovery_snapshot(&fixture.sessions, workspace, session)
            .await
            .unwrap()
            .focused
            .unwrap();
        assert_eq!(body.tool_calls.len(), 2);
        assert!(
            body.tool_calls
                .iter()
                .all(|call| call.state == ToolCallState::Denied)
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupt_keeps_its_exit_status_when_delivery_history_expires() {
        struct EnteredProvider(Arc<tokio::sync::Notify>);
        impl Provider for EnteredProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                self.0.notify_one();
                Box::pin(stream::pending())
            }
        }
        let entered = Arc::new(tokio::sync::Notify::new());
        let fixture = fixture({
            let entered = entered.clone();
            move || EnteredProvider(entered.clone())
        })
        .await;
        let mut opts = options(&fixture.workspace);
        let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
            .await
            .unwrap();
        opts.session = Some(handle.session_id);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let interrupted = async {
            // Retire the initial backlog before the foreground subscription is
            // first polled, as if the CLI was descheduled during startup.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    entered.notified().await;
                    retire_delivery_history(&fixture).await;
                });
            });
        };
        let code = run_goal(
            &fixture.sessions,
            opts,
            goal_request(),
            interrupted,
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
        let records = parse_records(&String::from_utf8(stdout).unwrap());
        assert_eq!(records.last().unwrap()["status"], "interrupted");
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn an_unanswered_question_survives_a_delivery_gap_without_an_invented_answer() {
        let fixture = fixture(|| AskingProvider {
            turn: Mutex::new(0),
        })
        .await;
        let mut options = options(&fixture.workspace);
        options.approval = HeadlessApproval::Full;
        let handle = submit(
            &fixture.sessions,
            &options,
            vec![InputPart::text("finish")],
            std::future::pending(),
        )
        .await
        .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        loop {
            let event = events.next().await.unwrap().unwrap();
            if matches!(
                event.event,
                SessionEvent::ToolApprovalRequested {
                    question: Some(_),
                    ..
                }
            ) {
                break;
            }
        }
        drop(events);
        retire_delivery_history(&fixture).await;
        let mut sink = RecordSink::open(&options).unwrap();
        let end = stream_run(
            &fixture.sessions,
            &options,
            &handle,
            &mut sink,
            std::future::pending(),
            None,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(end.status, HeadlessStatus::NeedsInput);
        assert_eq!(
            end.message.as_deref(),
            Some("the model asked the user a question and no client could answer: Which crate?")
        );
        assert!(end.answer.is_empty());
        assert_eq!(
            fixture
                .sessions
                .run_snapshot(handle.workspace_id, handle.session_id, handle.run_id)
                .await
                .unwrap()
                .outcome,
            Some(RunOutcome::Cancelled)
        );
        fixture.sessions.close().await.unwrap();
    }

    #[tokio::test]
    async fn canonical_replies_preserve_long_utf8_and_truncations_without_text_messages() {
        struct ContinuedProvider(Mutex<usize>);
        impl Provider for ContinuedProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let mut turn = self.0.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                let mut events = Vec::new();
                if current == 0 {
                    events.push(Ok(ProviderEvent::OutputTextDelta {
                        text: "prefix:".to_owned(),
                    }));
                }
                if current == 2 {
                    events.push(Ok(ProviderEvent::OutputTextDelta {
                        text: "答".repeat(24_000),
                    }));
                }
                events.push(Ok(if current < 2 {
                    ProviderEvent::Incomplete {
                        usage: None,
                        reason: if current == 1 {
                            qq_provider::IncompleteReason::Paused
                        } else {
                            qq_provider::IncompleteReason::OutputTokens
                        },
                    }
                } else {
                    ProviderEvent::Completed { usage: None }
                }));
                Box::pin(stream::iter(events))
            }
        }
        let fixture = fixture(|| ContinuedProvider(Mutex::new(0))).await;
        let mut opts = options(&fixture.workspace);
        opts.format = HeadlessFormat::Text;
        let (status, stdout, stderr) = run_to_end(&fixture, opts, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
        let expected = format!("prefix:{}\n", "答".repeat(24_000));
        assert_eq!(stdout.len(), expected.len());
        assert!(
            stdout == expected,
            "the exact final reply must retain its prefix and complete UTF-8 tail"
        );
        fixture.sessions.close().await.unwrap();
    }
    #[tokio::test]
    async fn canonical_reply_channel_order_survives_database_maintenance() {
        struct MixedProvider;
        impl Provider for MixedProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "one".to_owned(),
                    }),
                    Ok(ProviderEvent::RefusalDelta {
                        text: "!".to_owned(),
                    }),
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "two".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
        let mut fixture = fixture(|| MixedProvider).await;
        let handle = submit(
            &fixture.sessions,
            &options(&fixture.workspace),
            vec![InputPart::text("finish")],
            std::future::pending(),
        )
        .await
        .unwrap();
        let mut events = fixture
            .sessions
            .subscribe(SubscribeRequest {
                workspace_id: handle.workspace_id,
                after: handle.subscribe_after,
            })
            .unwrap();
        loop {
            if matches!(events.next().await.unwrap().unwrap().event,SessionEvent::RunFinished { run_id,.. } if run_id==handle.run_id)
            {
                break;
            }
        }
        drop(events);
        let result = fixture
            .sessions
            .run_result(handle.workspace_id, handle.session_id, handle.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.answer, "one!two");
        fixture.sessions.close().await.unwrap();
        let path = fixture
            ._directory
            .path()
            .canonicalize()
            .unwrap()
            .join("sessions.sqlite3");
        let database = path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = rusqlite::Connection::open(database).unwrap();
            connection.execute("DELETE FROM events", []).unwrap();
            connection.execute_batch("VACUUM;").unwrap();
        })
        .await
        .unwrap();
        fixture.sessions = SessionRuntime::open(
            SessionRuntimeOptions::new(path),
            Arc::new(ProviderLoader(|| MixedProvider)),
        )
        .await
        .unwrap();
        assert_eq!(
            fixture
                .sessions
                .run_result(handle.workspace_id, handle.session_id, handle.run_id)
                .await
                .unwrap()
                .unwrap(),
            result
        );
        fixture.sessions.close().await.unwrap();
    }
}
