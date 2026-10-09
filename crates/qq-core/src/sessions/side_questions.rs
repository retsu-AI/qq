use super::*;
use crate::tools::output::escaped_len;
use std::collections::hash_map::Entry;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

/// In-memory side tasks (admitting, executing or settling) across the runtime.
const MAX_SIDE_TASKS: usize = 64;
const MAX_SIDE_DURATION_MS: u64 = 120_000;
const MAX_SIDE_MODEL_TURNS: u32 = 8;
const MAX_SIDE_OUTPUT_TOKENS: u64 = 16_384;
const MAX_SIDE_TOOL_OUTPUT_BYTES: u64 = 96 * 1024;
const MAX_SIDE_ANSWER_BYTES: usize = 128 * 1024;
/// Smallest growth of a streamed turn between persisted partial updates.
const SIDE_PUBLISH_MIN_BYTES: usize = 1024;

/// A side answer is independent of the source session's transcript and spend.
#[derive(Debug)]
pub struct SideAnswer {
    pub id: RunId,
    pub text: String,
    pub usage: Option<TokenUsage>,
    pub estimated_cost_usd_nanos: Option<u64>,
    pub model_turns: u32,
}

/// Per-question ceilings an embedder may lower but never raise. `None`
/// keeps the shipped maximum: 120 s wall time (from submission, including
/// permit and ownership waits), 8 model turns, 16,384 output tokens and
/// 96 KiB of tool output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SideQueryLimits {
    pub max_duration_ms: Option<u64>,
    pub max_model_turns: Option<u32>,
    pub max_output_tokens: Option<u64>,
    pub max_tool_output_bytes: Option<u64>,
}

/// Validated [`SideQueryLimits`] with every ceiling resolved.
#[derive(Debug, Clone, Copy)]
pub(super) struct SideCeilings {
    pub(super) duration_ms: u64,
    model_turns: u32,
    output_tokens: u64,
    tool_output_bytes: u64,
}

impl SideQueryLimits {
    pub(super) fn resolve(self) -> Result<SideCeilings, SessionRuntimeError> {
        fn lower<T: Copy + PartialOrd + Default>(
            value: Option<T>,
            maximum: T,
        ) -> Result<T, SessionRuntimeError> {
            match value {
                None => Ok(maximum),
                Some(value) if value > T::default() && value <= maximum => Ok(value),
                Some(_) => Err(SessionRuntimeError::InvalidRunLimit),
            }
        }
        Ok(SideCeilings {
            duration_ms: lower(self.max_duration_ms, MAX_SIDE_DURATION_MS)?,
            model_turns: lower(self.max_model_turns, MAX_SIDE_MODEL_TURNS)?,
            output_tokens: lower(self.max_output_tokens, MAX_SIDE_OUTPUT_TOKENS)?,
            tool_output_bytes: lower(self.max_tool_output_bytes, MAX_SIDE_TOOL_OUTPUT_BYTES)?,
        })
    }
}

impl SideCeilings {
    pub(super) const fn duration(self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }
}

/// A durably admitted side question awaiting execution.
pub(super) struct SideLaunch {
    pub(super) session: SessionId,
    pub(super) id: RunId,
    pub(super) question: String,
    pub(super) request: RuntimeLoadRequest,
    pub(super) messages: Vec<Message>,
}

/// One reserved `side_tasks` slot; shutdown waits for every slot to drop.
struct SideTaskSlot(Arc<runtime::SessionRuntimeInner>);

impl Drop for SideTaskSlot {
    fn drop(&mut self) {
        self.0.side_tasks.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Routes a cancel command to the executing question.
struct SideRegistration {
    inner: Arc<runtime::SessionRuntimeInner>,
    id: RunId,
}

impl Drop for SideRegistration {
    fn drop(&mut self) {
        self.inner
            .side_cancellations
            .lock()
            .expect("side cancellation lock")
            .remove(&self.id);
    }
}

/// In-memory execution ownership of a source session. A cancelled question
/// releases its durable `running` row before it has drained, so a durably
/// admitted replacement waits here for the handoff instead of failing busy.
struct SideOwnership {
    inner: Arc<runtime::SessionRuntimeInner>,
    session: SessionId,
}

impl Drop for SideOwnership {
    fn drop(&mut self) {
        let released = self
            .inner
            .side_sessions
            .lock()
            .expect("side admission lock")
            .remove(&self.session);
        if let Some(released) = released {
            released.notify_waiters();
        }
    }
}

struct InspectionGate;

impl ToolGate for InspectionGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let allowed = matches!(call.name.as_str(), "read_file" | "search" | "tree");
        Box::pin(std::future::ready(if allowed {
            GateDecision::Execute
        } else {
            GateDecision::Deny {
                message: "side questions permit built-in inspection only".to_owned(),
            }
        }))
    }
}

impl SessionRuntime {
    fn reserve_side_task(&self) -> Result<SideTaskSlot, SessionRuntimeError> {
        match self
            .inner
            .side_tasks
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_SIDE_TASKS).then_some(count + 1)
            }) {
            Ok(_) => Ok(SideTaskSlot(Arc::clone(&self.inner))),
            Err(_) => Err(SessionRuntimeError::Overloaded),
        }
    }

    /// The wire submission. A spawned task owns the store command and the
    /// launch from before the transaction can commit, so dropping the
    /// caller's future never strands a durable `running` row without an
    /// executor.
    pub(super) async fn submit_side_question(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let lifecycle = self.inner.lifecycle.read().await;
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let slot = self.reserve_side_task()?;
        let runtime = self.clone();
        let (reply, response) = oneshot::channel();
        tokio::spawn(async move {
            let _slot = slot;
            let started = tokio::time::Instant::now();
            let applied = match runtime
                .inner
                .store
                .command_with_seed(
                    command_id,
                    command,
                    WorkspaceGrantSeed::default(),
                    Some(runtime.inner.grant_promotions.clone()),
                )
                .await
            {
                Ok(applied) => applied,
                Err(error) => {
                    // The caller may have gone away; nothing was committed.
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            runtime.inner.notify(applied.receipt.committed_through);
            // A dropped caller does not stop the committed launch below.
            let _ = reply.send(Ok(applied.receipt));
            if let Some(launch) = applied.side_launch {
                runtime.run_launched_side_question(launch, started).await;
            }
        });
        drop(lifecycle);
        match response.await {
            Ok(result) => result,
            Err(_) => Err(SessionRuntimeError::Unavailable),
        }
    }

    /// Executes a committed launch to durable settlement, including after a
    /// panic.
    pub(super) async fn run_launched_side_question(
        &self,
        launch: SideLaunch,
        started: tokio::time::Instant,
    ) {
        let id = launch.id;
        let outcome =
            AssertUnwindSafe(self.execute_side_question(launch, RunCancellation::new(), started))
                .catch_unwind()
                .await;
        let state = match outcome {
            Ok(Ok(_)) => return,
            // Most errors were already settled; settlement is idempotent.
            Ok(Err(_)) => qq_protocol::SideQuestionState::Failed,
            Err(_) => qq_protocol::SideQuestionState::Interrupted,
        };
        if self
            .inner
            .store
            .finish_side_question(id, state, false, self.inner.side_limits.duration_ms)
            .await
            .is_err()
        {
            self.inner.failed.send_replace(true);
        }
    }

    /// Runs without claiming the main session slot. Captures and updates are
    /// persisted separately; success returns only after durable settlement.
    pub async fn answer_side_question(
        &self,
        session: SessionId,
        question: String,
        cancellation: RunCancellation,
    ) -> Result<SideAnswer, SessionRuntimeError> {
        self.answer_side_question_in_thread(session, question, cancellation, false)
            .await
    }

    /// Explicitly starts a new side thread when `new_thread` is true.
    pub async fn answer_side_question_in_thread(
        &self,
        session: SessionId,
        question: String,
        cancellation: RunCancellation,
        new_thread: bool,
    ) -> Result<SideAnswer, SessionRuntimeError> {
        let lifecycle = self.inner.lifecycle.read().await;
        if *self.inner.shutdown.borrow() || *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let slot = self.reserve_side_task()?;
        let runtime = self.clone();
        let started = tokio::time::Instant::now();
        let (reply, response) = oneshot::channel();
        tokio::spawn(async move {
            let _slot = slot;
            let limit = runtime.inner.side_limits.duration_ms;
            let id = match RunId::generate() {
                Ok(id) => id,
                Err(_) => {
                    let _ = reply.send(Err(SessionRuntimeError::Unavailable));
                    return;
                }
            };
            let deadline = started + runtime.inner.side_limits.duration();
            let mut pending = match runtime
                .inner
                .store
                .enqueue_side_admission(session, id, question.clone(), new_thread, Some(deadline))
                .await
            {
                Ok(pending) => pending,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let admitted = tokio::select! {
                result = &mut pending => result,
                () = tokio::time::sleep_until(deadline) => {
                    cancellation.cancel();
                    let _ = reply.send(Err(SessionRuntimeError::SideQuestionTimedOut));
                    // Accepted admission may still commit; settle what it wrote.
                    if matches!(pending.await, Ok(Ok(_)))
                        && runtime
                            .inner
                            .store
                            .finish_side_question(id, qq_protocol::SideQuestionState::TimedOut, true, limit)
                            .await
                            .is_err()
                    {
                        runtime.inner.failed.send_replace(true);
                    }
                    return;
                }
            };
            let (request, messages) = match admitted {
                Ok(Ok(source)) => source,
                Ok(Err(error)) => {
                    let _ = reply.send(Err(error));
                    return;
                }
                Err(_) => {
                    let _ = reply.send(Err(SessionRuntimeError::Unavailable));
                    return;
                }
            };
            let launch = SideLaunch {
                session,
                id,
                question,
                request,
                messages,
            };
            let outcome =
                AssertUnwindSafe(runtime.execute_side_question(launch, cancellation, started))
                    .catch_unwind()
                    .await;
            let (result, fallback) = match outcome {
                Ok(Ok(answer)) => (Ok(answer), None),
                Ok(Err(error)) => (Err(error), Some(qq_protocol::SideQuestionState::Failed)),
                Err(_) => (
                    Err(SessionRuntimeError::Unavailable),
                    Some(qq_protocol::SideQuestionState::Interrupted),
                ),
            };
            if let Some(state) = fallback
                && runtime
                    .inner
                    .store
                    .finish_side_question(id, state, false, limit)
                    .await
                    .is_err()
            {
                runtime.inner.failed.send_replace(true);
            }
            let _ = reply.send(result);
        });
        drop(lifecycle);
        match response.await {
            Ok(result) => result,
            Err(_) => Err(SessionRuntimeError::Unavailable),
        }
    }

    /// Waits until no other execution owns `session`. Registers for the
    /// release before re-checking ownership so a handoff is never missed.
    async fn own_side_session(&self, session: SessionId) -> SideOwnership {
        loop {
            let owner = {
                let mut sessions = self
                    .inner
                    .side_sessions
                    .lock()
                    .expect("side admission lock");
                match sessions.entry(session) {
                    Entry::Vacant(entry) => {
                        entry.insert(Arc::new(Notify::new()));
                        return SideOwnership {
                            inner: Arc::clone(&self.inner),
                            session,
                        };
                    }
                    Entry::Occupied(entry) => Arc::clone(entry.get()),
                }
            };
            let released = owner.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            let current = self
                .inner
                .side_sessions
                .lock()
                .expect("side admission lock")
                .get(&session)
                .is_some_and(|current| Arc::ptr_eq(current, &owner));
            if current {
                released.await;
            }
        }
    }

    async fn execute_side_question(
        &self,
        launch: SideLaunch,
        cancellation: RunCancellation,
        started: tokio::time::Instant,
    ) -> Result<SideAnswer, SessionRuntimeError> {
        let SideLaunch {
            session,
            id,
            question,
            request,
            mut messages,
        } = launch;
        if question.trim().is_empty() || question.len() > 8192 {
            return Err(SessionRuntimeError::InvalidSideQuestion);
        }
        // Subscribe before reading so a shutdown after the check still wakes.
        let mut shutdown = self.inner.shutdown.subscribe();
        if *self.inner.failed.borrow() || *shutdown.borrow_and_update() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let limits = self.inner.side_limits;
        self.inner
            .side_cancellations
            .lock()
            .expect("side cancellation lock")
            .insert(id, cancellation.clone());
        let _registration = SideRegistration {
            inner: Arc::clone(&self.inner),
            id,
        };
        let deadline = started + limits.duration();
        let tool_tasks = crate::tools::ToolTasks::default();
        // Whether a provider request may have been sent and not committed:
        // its spend is then unknown rather than the last committed total.
        let in_flight = AtomicBool::new(false);
        let acquired = tokio::select! {
            ownership = self.own_side_session(session) => Ok(ownership),
            () = tokio::time::sleep_until(deadline) => Err(SessionRuntimeError::SideQuestionTimedOut),
            () = cancellation.cancelled() => Err(SessionRuntimeError::SideQuestionCancelled),
            _ = shutdown.changed() => Err(SessionRuntimeError::SideQuestionCancelled),
        };
        let (ownership, result) = match acquired {
            Err(error) => (None, Err(error)),
            Ok(ownership) => {
                let work = Box::pin(async {
                    let stored = self
                        .inner
                        .store
                        .call(store::Priority::AwaitControl, move |connection| {
                            find_side_snapshot(connection, id)
                        })
                        .await?;
                    // A cancelled (or since deleted) row is not executed.
                    let Some(stored) = stored else {
                        return Err(SessionRuntimeError::SideQuestionCancelled);
                    };
                    if stored.state != qq_protocol::SideQuestionState::Running {
                        return Err(SessionRuntimeError::SideQuestionCancelled);
                    }
                    let remaining = limits
                        .duration_ms
                        .saturating_sub(now_ms().saturating_sub(stored.created_at_ms));
                    if remaining == 0 {
                        return Err(SessionRuntimeError::SideQuestionTimedOut);
                    }
                    let durable_deadline =
                        tokio::time::Instant::now() + Duration::from_millis(remaining);
                    let execution = async {
                        let _permit = self
                            .inner
                            .side_permits
                            .acquire()
                            .await
                            .map_err(|_| SessionRuntimeError::Unavailable)?;
                        if *self.inner.shutdown.borrow() {
                            return Err(SessionRuntimeError::SideQuestionCancelled);
                        }
                        let workspace = request.workspace.clone();
                        let loaded = self
                            .inner
                            .loader
                            .load_with_progress(request, RuntimeLoadProgress::default())
                            .await
                            .map_err(|_| SessionRuntimeError::Unavailable)?;
                        if loaded.plan.workspace_path() != Path::new(&workspace) {
                            return Err(SessionRuntimeError::CONSTRAINT);
                        }
                        let plan = loaded
                            .plan
                            .side_question_plan()
                            .await
                            .map_err(|_| SessionRuntimeError::Unavailable)?;
                        messages.push(Message::user("This is an isolated side question. The preceding context was captured from committed session history; live file inspection is not a filesystem snapshot. Do not steer or modify the main task."));
                        messages.push(Message::user(question));
                        let pricing = plan.resolved_model().pricing.clone();
                        let capabilities = RunCapabilities::restricted()
                            .with_limits(
                                RunLimits {
                                    max_duration_ms: Some(limits.duration_ms),
                                    max_model_turns: Some(limits.model_turns),
                                    max_output_tokens: Some(limits.output_tokens),
                                    max_tool_output_bytes: Some(limits.tool_output_bytes),
                                    max_children: Some(0),
                                    max_concurrent_children: Some(0),
                                    ..RunLimits::default()
                                },
                                pricing.clone(),
                            )
                            .with_tool_tasks(tool_tasks.clone())
                            .with_execution_started(started);
                        let mut events = plan.execute(
                            messages,
                            cancellation.clone(),
                            Arc::new(InspectionGate),
                            Arc::new(FileState::default()),
                            capabilities,
                        );
                        let mut answer = SideAnswer {
                            id,
                            text: String::new(),
                            usage: Some(TokenUsage::default()),
                            estimated_cost_usd_nanos: pricing.as_ref().map(|_| 0),
                            model_turns: 0,
                        };
                        // The streaming turn is kept apart from the last
                        // completed answer; partial updates carry only it.
                        let mut turn_text = String::new();
                        let mut published_bytes = 0_usize;
                        while let Some(event) = events.next().await {
                            match event {
                                RuntimeEvent::Prepared { .. } => {
                                    in_flight.store(true, Ordering::Release);
                                    // A cancel command settles the row without
                                    // this task, so the durable spend must
                                    // already read unknown. Before the first
                                    // commit it is still the insert's NULL.
                                    if answer.model_turns > 0 {
                                        self.inner
                                            .store
                                            .record_side_turn(
                                                id,
                                                answer.text.clone(),
                                                None,
                                                None,
                                                answer.model_turns,
                                            )
                                            .await?;
                                    }
                                }
                                RuntimeEvent::OutputTextDelta { text } => {
                                    if text.is_empty() {
                                        continue;
                                    }
                                    if turn_text.len().saturating_add(text.len())
                                        > MAX_SIDE_ANSWER_BYTES
                                    {
                                        return Err(SessionRuntimeError::CONSTRAINT);
                                    }
                                    turn_text.push_str(&text);
                                    // Geometric checkpoints keep re-persisting
                                    // the growing turn O(n log n), not O(n²).
                                    let threshold =
                                        (published_bytes / 2).max(SIDE_PUBLISH_MIN_BYTES);
                                    if published_bytes == 0
                                        || turn_text.len() - published_bytes >= threshold
                                    {
                                        self.inner
                                            .store
                                            .record_side_turn(
                                                id,
                                                turn_text.clone(),
                                                None,
                                                None,
                                                answer.model_turns,
                                            )
                                            .await?;
                                        published_bytes = turn_text.len();
                                    }
                                }
                                RuntimeEvent::AssistantTurnCompleted { message, usage, .. } => {
                                    answer.model_turns += 1;
                                    answer.text.clear();
                                    for block in message.content() {
                                        if let ContentBlock::Text { text } = block {
                                            if answer.text.len().saturating_add(text.len())
                                                > MAX_SIDE_ANSWER_BYTES
                                            {
                                                return Err(SessionRuntimeError::CONSTRAINT);
                                            }
                                            answer.text.push_str(text);
                                        }
                                    }
                                    answer.usage = match (answer.usage, usage) {
                                        (Some(total), Some(usage)) => {
                                            execution::add_usage(total, usage)
                                        }
                                        _ => None,
                                    };
                                    answer.estimated_cost_usd_nanos =
                                        match (answer.estimated_cost_usd_nanos, usage) {
                                            (Some(total), Some(usage)) => pricing
                                                .as_ref()
                                                .and_then(|pricing| run_cost(usage, pricing))
                                                .and_then(|cost| total.checked_add(cost)),
                                            _ => None,
                                        };
                                    turn_text.clear();
                                    published_bytes = 0;
                                    self.inner
                                        .store
                                        .record_side_turn(
                                            id,
                                            answer.text.clone(),
                                            answer.usage,
                                            answer.estimated_cost_usd_nanos,
                                            answer.model_turns,
                                        )
                                        .await?;
                                    in_flight.store(false, Ordering::Release);
                                }
                                RuntimeEvent::Completed { .. } => return Ok(answer),
                                // The plan's own clock shares this deadline.
                                RuntimeEvent::BudgetExhausted { exhaustion }
                                    if exhaustion.limit
                                        == qq_protocol::BudgetLimitKind::Duration =>
                                {
                                    return Err(SessionRuntimeError::SideQuestionTimedOut);
                                }
                                RuntimeEvent::Failed { .. }
                                | RuntimeEvent::BudgetExhausted { .. } => {
                                    return Err(SessionRuntimeError::Unavailable);
                                }
                                _ => {}
                            }
                        }
                        Err(SessionRuntimeError::Unavailable)
                    };
                    match tokio::time::timeout_at(durable_deadline, execution).await {
                        Ok(result) => result,
                        Err(_) => Err(SessionRuntimeError::SideQuestionTimedOut),
                    }
                });
                let mut work = work;
                let result = tokio::select! {
                    result = &mut work => result,
                    () = tokio::time::sleep_until(deadline) => Err(SessionRuntimeError::SideQuestionTimedOut),
                    () = cancellation.cancelled() => Err(SessionRuntimeError::SideQuestionCancelled),
                    _ = shutdown.changed() => Err(SessionRuntimeError::SideQuestionCancelled),
                };
                if result.is_err() {
                    cancellation.cancel();
                }
                // Dropping the stream stops dispatch; the drain then waits for
                // blocking inspection already running, so no side tool outlives
                // settlement or the task slot shutdown waits on.
                drop(work);
                let result = match tool_tasks.drain().await {
                    Ok(()) => result,
                    Err(_) => Err(SessionRuntimeError::Unavailable),
                };
                (Some(ownership), result)
            }
        };
        let state = match &result {
            Ok(_) => qq_protocol::SideQuestionState::Completed,
            Err(SessionRuntimeError::SideQuestionCancelled) => {
                qq_protocol::SideQuestionState::Cancelled
            }
            Err(SessionRuntimeError::SideQuestionTimedOut) => {
                qq_protocol::SideQuestionState::TimedOut
            }
            Err(_) => qq_protocol::SideQuestionState::Failed,
        };
        let spend_known = result.is_ok() || !in_flight.load(Ordering::Acquire);
        let effective = self
            .inner
            .store
            .finish_side_question(id, state, spend_known, limits.duration_ms)
            .await?;
        drop(ownership);
        match effective {
            qq_protocol::SideQuestionState::Completed => result,
            qq_protocol::SideQuestionState::TimedOut => {
                Err(SessionRuntimeError::SideQuestionTimedOut)
            }
            qq_protocol::SideQuestionState::Cancelled => {
                Err(SessionRuntimeError::SideQuestionCancelled)
            }
            qq_protocol::SideQuestionState::Failed
            | qq_protocol::SideQuestionState::Interrupted => Err(SessionRuntimeError::Unavailable),
            qq_protocol::SideQuestionState::Running => Err(SessionRuntimeError::CONSTRAINT),
        }
    }
}

pub(super) fn load_side_snapshot(
    connection: &Connection,
    id: RunId,
) -> Result<qq_protocol::SideQuestionSnapshot, SessionRuntimeError> {
    // Callers read a row they just wrote or already found.
    find_side_snapshot(connection, id)?.ok_or(SessionRuntimeError::CONSTRAINT)
}

pub(super) fn find_side_snapshot(
    connection: &Connection,
    id: RunId,
) -> Result<Option<qq_protocol::SideQuestionSnapshot>, SessionRuntimeError> {
    let row = connection
        .query_row(
            "SELECT session_id, thread_id, question, answer, state, usage_json,
            estimated_cost_usd_nanos, model_turns, created_at_ms, finished_at_ms FROM side_questions WHERE id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<u64>>(6)?,
                    row.get::<_, u32>(7)?,
                    row.get::<_, u64>(8)?,
                    row.get::<_, Option<u64>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let state = match row.4.as_str() {
        "running" => qq_protocol::SideQuestionState::Running,
        "completed" => qq_protocol::SideQuestionState::Completed,
        "failed" => qq_protocol::SideQuestionState::Failed,
        "interrupted" => qq_protocol::SideQuestionState::Interrupted,
        "cancelled" => qq_protocol::SideQuestionState::Cancelled,
        "timed_out" => qq_protocol::SideQuestionState::TimedOut,
        _ => return Err(SessionRuntimeError::CODEC),
    };
    Ok(Some(qq_protocol::SideQuestionSnapshot {
        id,
        session_id: parse_id(&row.0)?,
        thread_id: parse_id(&row.1)?,
        question: row.2,
        answer: row.3,
        state,
        usage: row
            .5
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .flatten(),
        estimated_cost_usd_nanos: row.6,
        model_turns: row.7,
        created_at_ms: row.8,
        finished_at_ms: row.9,
    }))
}

/// Bytes of a snapshot body reserved for side questions ahead of the
/// transcript, so a long main session cannot starve them on reconnect.
const SIDE_SNAPSHOT_BUDGET_BYTES: usize = 1024 * 1024;

/// The running question, then the newest terminal ones, from a reserved
/// share of `budget`. Only what is admitted is charged.
pub(super) fn load_side_snapshots(
    connection: &Connection,
    session: SessionId,
    budget: &mut snapshots::SnapshotBudget,
) -> Result<Vec<qq_protocol::SideQuestionSnapshot>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT id FROM side_questions WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 64",
    )?;
    let ids = statement
        .query_map([session.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut reserved =
        snapshots::SnapshotBudget::new(budget.remaining().min(SIDE_SNAPSHOT_BUDGET_BYTES));
    let mut questions = Vec::with_capacity(ids.len());
    let mut charged = 0_usize;
    for id in ids {
        let item = load_side_snapshot(connection, parse_id(&id)?)?;
        let bytes = escaped_len(&item.question).saturating_add(escaped_len(&item.answer));
        // Admission refuses a second running row, so the running one is the
        // newest and is charged first. Its escaped worst case (8 KiB question,
        // 128 KiB answer) fits the reserve, so it is kept whenever the body
        // has the reserve left, and always for the focused body.
        if !reserved.admit(bytes) {
            break;
        }
        charged = charged.saturating_add(bytes);
        questions.push(item);
    }
    budget.charge(charged, questions.len());
    questions.reverse();
    Ok(questions)
}

pub(super) fn append_side_event(
    connection: &Connection,
    store_id: StoreId,
    id: RunId,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let question = load_side_snapshot(connection, id)?;
    let workspace = session_workspace(connection, question.session_id)?;
    append_event(
        connection,
        EventContext {
            store_id,
            workspace_id: workspace,
            session_id: question.session_id,
            run_id: None,
            caused_by: None,
            occurred_at_ms: now_ms(),
        },
        SessionEvent::SideQuestionUpdated {
            side_question: Box::new(question),
        },
    )
}

pub(super) fn admit_side_question(
    transaction: &Connection,
    store_id: StoreId,
    session_id: SessionId,
    question_id: RunId,
    question: String,
    new_thread: bool,
) -> Result<(RuntimeLoadRequest, Vec<Message>), SessionRuntimeError> {
    if question.trim().is_empty() || question.len() > 8192 {
        return Err(SessionRuntimeError::InvalidSideQuestion);
    }
    let pending: u64 = transaction.query_row(
        "SELECT COUNT(*) FROM side_questions WHERE state = 'running'",
        [],
        |row| row.get(0),
    )?;
    if pending >= 64 {
        return Err(SessionRuntimeError::Overloaded);
    }
    let active: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM side_questions WHERE session_id = ?1 AND state = 'running')",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    if active {
        return Err(SessionRuntimeError::SideQuestionBusy);
    }
    let row = transaction
        .query_row(
            "SELECT w.path, s.model, s.model_is_fallback, s.max_output_tokens,
                        s.organization, s.profile, s.reasoning_effort
                 FROM sessions s JOIN workspaces w ON w.id = s.workspace_id WHERE s.id = ?1",
            [session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    let request = RuntimeLoadRequest {
        workspace: row.0,
        model: ModelSelection {
            model: row.1,
            model_is_fallback: row.2,
            max_output_tokens: row.3,
            organization: row.4,
        },
        profile: parse_profile(row.5.as_deref())?,
        reasoning_effort: parse_reasoning_effort(row.6.as_deref())?,
        checkpoint: None,
        routing: None,
        approval_delegate: None,
    };
    let mut messages = transcript::capture_side_context(transaction, session_id)?;
    let (status, active): (String, Option<String>) = transaction.query_row(
        "SELECT status, active_run_id FROM sessions WHERE id = ?1",
        [session_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let notice = Message::user(format!(
        "[Captured source session status: {status}; active run: {}]",
        active.as_deref().unwrap_or("none")
    ));
    if transcript::context_bytes(&messages)
        .saturating_add(transcript::context_bytes(std::slice::from_ref(&notice)))
        <= 32768
    {
        messages.push(notice);
    }
    let thread: Option<String> = if new_thread {
        None
    } else {
        transaction.query_row(
                    "SELECT thread_id FROM side_questions WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
                    [session_id.to_string()], |row| row.get(0),
                ).optional()?
    };
    let thread = thread.unwrap_or_else(|| question_id.to_string());
    let mut statement = transaction.prepare_cached(
                "SELECT CASE WHEN length(CAST(question AS BLOB)) + length(CAST(answer AS BLOB)) <= 32768
                    THEN question ELSE NULL END,
                    CASE WHEN length(CAST(question AS BLOB)) + length(CAST(answer AS BLOB)) <= 32768
                    THEN answer ELSE NULL END
                 FROM side_questions WHERE session_id = ?1 AND thread_id = ?2 AND state = 'completed'
                 ORDER BY rowid DESC LIMIT 65",
            )?;
    let history = statement
        .query_map(params![session_id.to_string(), thread], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut retained = Vec::new();
    let mut bytes = 0_usize;
    let mut omitted = history.len() > 64;
    for (question, answer) in history.into_iter().take(64) {
        let (Some(question), Some(answer)) = (question, answer) else {
            omitted = true;
            break;
        };
        if question.len() + answer.len() > (32 * 1024_usize - 128).saturating_sub(bytes) {
            omitted = true;
            break;
        }
        bytes += question.len() + answer.len();
        retained.push((question, answer));
    }
    if omitted {
        messages.push(Message::user("[Older side-thread exchanges omitted.]"));
    }
    for (question, answer) in retained.into_iter().rev() {
        messages.push(Message::user(question));
        messages.push(Message::assistant(answer));
    }
    let stored: Vec<_> = messages
        .iter()
        .map(|message| {
            (
                format!("{:?}", message.role()),
                message
                    .content()
                    .iter()
                    .map(PersistedContentBlock::from)
                    .collect::<Vec<_>>(),
                message.replay(),
            )
        })
        .collect();
    let context = serde_json::to_string(&stored)?;
    if context.len() > 262144 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    transaction.execute(
                "INSERT INTO side_questions(id, session_id, question, captured_context_json, state, created_at_ms, thread_id)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6)",
                params![question_id.to_string(), session_id.to_string(), question, context, now_ms(), thread],
            )?;
    side_questions::append_side_event(transaction, store_id, question_id)?;
    Ok((request, messages))
}
