use super::*;

/// A side answer is independent of the source session's transcript and spend.
#[derive(Debug)]
pub struct SideAnswer {
    pub id: RunId,
    pub text: String,
    pub usage: Option<TokenUsage>,
    pub estimated_cost_usd_nanos: Option<u64>,
    pub model_turns: u32,
}

struct SideAdmission {
    inner: Arc<runtime::SessionRuntimeInner>,
    session: SessionId,
    id: RunId,
}

impl Drop for SideAdmission {
    fn drop(&mut self) {
        self.inner
            .side_sessions
            .lock()
            .expect("side admission lock")
            .remove(&self.session);
        self.inner
            .side_cancellations
            .lock()
            .expect("side cancellation lock")
            .remove(&self.id);
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
    pub(super) fn launch_side_question(
        &self,
        source: (SessionId, RunId, String, RuntimeLoadRequest, Vec<Message>),
    ) {
        let runtime = self.clone();
        self.inner.side_tasks.fetch_add(1, Ordering::AcqRel);
        tokio::spawn(async move {
            struct TaskGuard(Arc<runtime::SessionRuntimeInner>);
            impl Drop for TaskGuard {
                fn drop(&mut self) {
                    self.0.side_tasks.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _task = TaskGuard(Arc::clone(&runtime.inner));
            let (session, id, question, request, messages) = source;
            let outcome = AssertUnwindSafe(runtime.execute_side_question(
                session,
                question,
                RunCancellation::new(),
                false,
                Some((id, request, messages)),
            ))
            .catch_unwind()
            .await;
            let state = match outcome {
                Ok(Ok(_)) => return,
                Ok(Err(_)) => qq_protocol::SideQuestionState::Failed,
                Err(_) => qq_protocol::SideQuestionState::Interrupted,
            };
            if runtime
                .inner
                .store
                .finish_side_question(id, state)
                .await
                .is_err()
            {
                runtime.inner.failed.send_replace(true);
            }
        });
    }
    /// Runs without claiming the main session slot. This foundation returns
    /// only a completed answer; durable side-thread/event admission follows.
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
        let runtime = self.clone();
        match tokio::spawn(async move {
            let id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let (request, messages) = runtime
                .inner
                .store
                .side_source(session, id, question.clone(), new_thread)
                .await?;
            match AssertUnwindSafe(runtime.execute_side_question(
                session,
                question,
                cancellation,
                new_thread,
                Some((id, request, messages)),
            ))
            .catch_unwind()
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    runtime
                        .inner
                        .store
                        .finish_side_question(id, qq_protocol::SideQuestionState::Interrupted)
                        .await?;
                    Err(SessionRuntimeError::Unavailable)
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(SessionRuntimeError::Unavailable),
        }
    }

    async fn execute_side_question(
        &self,
        session: SessionId,
        question: String,
        cancellation: RunCancellation,
        new_thread: bool,
        admitted: Option<(RunId, RuntimeLoadRequest, Vec<Message>)>,
    ) -> Result<SideAnswer, SessionRuntimeError> {
        if question.trim().is_empty() || question.len() > 8192 {
            return Err(SessionRuntimeError::InvalidSideQuestion);
        }
        if *self.inner.failed.borrow() || *self.inner.shutdown.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        {
            let mut sessions = self
                .inner
                .side_sessions
                .lock()
                .expect("side admission lock");
            if !sessions.insert(session) {
                return Err(SessionRuntimeError::SideQuestionBusy);
            }
        }
        let id = match &admitted {
            Some((id, _, _)) => *id,
            None => RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?,
        };
        self.inner
            .side_cancellations
            .lock()
            .expect("side cancellation lock")
            .insert(id, cancellation.clone());
        let _admission = SideAdmission {
            inner: Arc::clone(&self.inner),
            session,
            id,
        };
        let started = tokio::time::Instant::now();
        // Admission must finish before cancellation can settle it: a queued
        // SQLite job may commit even if its receiving future is dropped.
        let (request, mut messages) = match admitted {
            Some((_, request, messages)) => (request, messages),
            None => {
                self.inner
                    .store
                    .side_source(session, id, question.clone(), new_thread)
                    .await?
            }
        };
        let deadline = started + Duration::from_secs(120);
        let work = tokio::time::timeout_at(deadline, async {
            let stored = self
                .inner
                .store
                .call(store::Priority::AwaitControl, move |connection| {
                    load_side_snapshot(connection, id)
                })
                .await?;
            if stored.state != qq_protocol::SideQuestionState::Running {
                return Err(SessionRuntimeError::SideQuestionCancelled);
            }
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
            let capabilities = RunCapabilities::restricted()
                .with_limits(
                    RunLimits {
                        max_duration_ms: Some(120_000),
                        max_model_turns: Some(8),
                        max_output_tokens: Some(16_384),
                        max_tool_output_bytes: Some(96 * 1024),
                        max_children: Some(0),
                        max_concurrent_children: Some(0),
                        ..RunLimits::default()
                    },
                    plan.resolved_model().pricing.clone(),
                )
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
                estimated_cost_usd_nanos: plan.resolved_model().pricing.as_ref().map(|_| 0),
                model_turns: 0,
            };
            let mut published_bytes = 0_usize;
            while let Some(event) = events.next().await {
                match event {
                    RuntimeEvent::OutputTextDelta { text } => {
                        if text.is_empty() {
                            continue;
                        }
                        if answer.text.len().saturating_add(text.len()) > 128 * 1024 {
                            return Err(SessionRuntimeError::CONSTRAINT);
                        }
                        answer.text.push_str(&text);
                        if answer.text.len().saturating_sub(published_bytes) >= 1024
                            || published_bytes == 0
                        {
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
                            published_bytes = answer.text.len();
                        }
                    }
                    RuntimeEvent::AssistantTurnCompleted { message, usage, .. } => {
                        answer.model_turns += 1;
                        answer.text.clear();
                        for block in message.content() {
                            if let ContentBlock::Text { text } = block {
                                if answer.text.len().saturating_add(text.len()) > 128 * 1024 {
                                    return Err(SessionRuntimeError::CONSTRAINT);
                                }
                                answer.text.push_str(text);
                            }
                        }
                        answer.usage = match (answer.usage, usage) {
                            (Some(total), Some(usage)) => execution::add_usage(total, usage),
                            _ => None,
                        };
                        answer.estimated_cost_usd_nanos =
                            match (answer.estimated_cost_usd_nanos, usage) {
                                (Some(total), Some(usage)) => plan
                                    .resolved_model()
                                    .pricing
                                    .as_ref()
                                    .and_then(|pricing| run_cost(usage, pricing))
                                    .and_then(|cost| total.checked_add(cost)),
                                _ => None,
                            };
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
                    }
                    RuntimeEvent::Completed { .. } => return Ok(answer),
                    RuntimeEvent::Failed { .. } | RuntimeEvent::BudgetExhausted { .. } => {
                        return Err(SessionRuntimeError::Unavailable);
                    }
                    _ => {}
                }
            }
            Err(SessionRuntimeError::Unavailable)
        });
        tokio::pin!(work);
        let mut shutdown = self.inner.shutdown.subscribe();
        let result = tokio::select! {
            result = &mut work => match result {
                Ok(result) => result,
                Err(_) => { cancellation.cancel(); Err(SessionRuntimeError::SideQuestionTimedOut) }
            },
            () = cancellation.cancelled() => Err(SessionRuntimeError::SideQuestionCancelled),
            _ = shutdown.changed() => {
                cancellation.cancel();
                Err(SessionRuntimeError::SideQuestionCancelled)
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
        self.inner.store.finish_side_question(id, state).await?;
        result
    }
}

pub(super) fn load_side_snapshot(
    connection: &Connection,
    id: RunId,
) -> Result<qq_protocol::SideQuestionSnapshot, SessionRuntimeError> {
    let row = connection.query_row(
        "SELECT session_id, thread_id, question, answer, state, usage_json,
            estimated_cost_usd_nanos, model_turns, created_at_ms, finished_at_ms FROM side_questions WHERE id = ?1",
        [id.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?, row.get::<_, Option<u64>>(6)?, row.get::<_, u32>(7)?,
            row.get::<_, u64>(8)?, row.get::<_, Option<u64>>(9)?)),
    )?;
    let state = match row.4.as_str() {
        "running" => qq_protocol::SideQuestionState::Running,
        "completed" => qq_protocol::SideQuestionState::Completed,
        "failed" => qq_protocol::SideQuestionState::Failed,
        "interrupted" => qq_protocol::SideQuestionState::Interrupted,
        "cancelled" => qq_protocol::SideQuestionState::Cancelled,
        "timed_out" => qq_protocol::SideQuestionState::TimedOut,
        _ => return Err(SessionRuntimeError::CODEC),
    };
    Ok(qq_protocol::SideQuestionSnapshot {
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
    })
}

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
    let mut questions = Vec::new();
    for id in ids {
        let item = load_side_snapshot(connection, parse_id(&id)?)?;
        if !budget.admit(
            item.question
                .len()
                .saturating_add(item.answer.len())
                .saturating_mul(6),
        ) {
            break;
        }
        questions.push(item);
    }
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
