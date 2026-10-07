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
}

impl Drop for SideAdmission {
    fn drop(&mut self) {
        self.inner
            .side_sessions
            .lock()
            .expect("side admission lock")
            .remove(&self.session);
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
            runtime
                .execute_side_question(session, question, cancellation, new_thread)
                .await
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
        let _admission = SideAdmission {
            inner: Arc::clone(&self.inner),
            session,
        };
        let id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        let started = tokio::time::Instant::now();
        // Admission must finish before cancellation can settle it: a queued
        // SQLite job may commit even if its receiving future is dropped.
        let (request, mut messages) = self
            .inner
            .store
            .side_source(session, id, question.clone(), new_thread)
            .await?;
        let deadline = started + Duration::from_secs(120);
        let work = tokio::time::timeout_at(deadline, async {
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
            while let Some(event) = events.next().await {
                match event {
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
        let persisted = result
            .as_ref()
            .ok()
            .map(|answer| (answer.text.clone(), answer.usage));
        self.inner.store.finish_side_question(id, persisted).await?;
        result
    }
}
