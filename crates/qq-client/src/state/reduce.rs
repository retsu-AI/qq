use std::{collections::HashSet, sync::Arc};

use qq_protocol::{
    AuditOutcome, MessageRole, MessageState, RunOutcome, ServerCapabilities, SessionEvent,
    SessionEventEnvelope, SessionId, SideQuestionState, SnapshotRequest, TextChannel,
    ToolCallState, WorkspaceId,
};

use super::{
    ApprovalPreview, ApprovalSettlement, ModelOption, SessionStore, SessionView, body_request,
    plan_label, tool_call_state_is_terminal,
};

const MAX_COMPACTION_EXCERPT_CHARS: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attention {
    ApprovalRequested { session_title: String },
    RunFinished { session_title: String },
}

impl Attention {
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::ApprovalRequested { session_title } => {
                format!("qq: {session_title} needs approval")
            }
            Self::RunFinished { session_title } => format!("qq: {session_title} finished"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReduceContext<'a> {
    pub focused: Option<SessionId>,
    pub attentive: bool,
    pub workspace_id: Option<WorkspaceId>,
    pub capabilities: Option<&'a Arc<ServerCapabilities>>,
    pub models: &'a [ModelOption],
    pub caused_by_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEffect {
    CheckInterruptionChanged(Option<qq_protocol::CheckInterruption>),
    Notice {
        session: Option<SessionId>,
        level: NoticeLevel,
        text: String,
    },
    Attention(Attention),
    RequestSnapshot(SnapshotRequest),
    Refocus(Option<SessionId>),
    AdoptCreated(SessionId),
    SubmitDraft {
        session_id: SessionId,
        text: String,
    },
    SessionRemoved {
        session_id: SessionId,
        tool_call_ids: Vec<qq_protocol::ToolCallId>,
    },
}

pub type StateEffects = Vec<StateEffect>;

impl SessionStore {
    pub fn replay_event(
        &mut self,
        envelope: &SessionEventEnvelope,
        context: ReduceContext<'_>,
    ) -> StateEffects {
        let local = self.get_mut(&envelope.session_id).map(|view| {
            (
                view.unread,
                view.finished_unread,
                std::mem::take(&mut view.drafts),
            )
        });
        let effects = self.reduce_event(envelope, context);
        if let Some((unread, finished_unread, drafts)) = local
            && let Some(view) = self.get_mut(&envelope.session_id)
        {
            view.unread = unread;
            view.finished_unread = finished_unread;
            view.drafts = drafts;
        }
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                effect @ (StateEffect::RequestSnapshot(_)
                | StateEffect::Refocus(_)
                | StateEffect::SessionRemoved { .. }) => Some(effect),
                StateEffect::CheckInterruptionChanged(_)
                | StateEffect::Notice { .. }
                | StateEffect::Attention(_)
                | StateEffect::AdoptCreated(_)
                | StateEffect::SubmitDraft { .. } => None,
            })
            .collect()
    }

    pub fn reduce_event(
        &mut self,
        envelope: &SessionEventEnvelope,
        context: ReduceContext<'_>,
    ) -> StateEffects {
        let mut effects = StateEffects::new();
        let session_id = envelope.session_id;
        match &envelope.event {
            SessionEvent::SideQuestionUpdated { side_question } => {
                if let Some(view) = self.body_mut(&session_id) {
                    let running = side_question.state == SideQuestionState::Running;
                    match view.summary.running_side_question {
                        _ if running => view.summary.running_side_question = Some(side_question.id),
                        Some(id) if id == side_question.id => {
                            view.summary.running_side_question = None;
                        }
                        Some(_) | None => {}
                    }
                }
                if let Some(view) = self.get_mut(&session_id)
                    && view.is_warm()
                {
                    if let Some(existing) = view
                        .side_questions
                        .iter_mut()
                        .find(|item| item.id == side_question.id)
                    {
                        *existing = side_question.as_ref().clone();
                    } else {
                        view.side_questions.push(side_question.as_ref().clone());
                        if view.side_questions.len() > 64 {
                            view.side_questions.remove(0);
                        }
                    }
                }
            }
            SessionEvent::SessionCreated { session } => {
                self.upsert_summary((**session).clone(), context.models, 0);
                if context.caused_by_me {
                    self.warm_empty(session.id);
                    effects.push(StateEffect::AdoptCreated(session.id));
                }
            }
            SessionEvent::SessionUpdated { session } => {
                self.upsert_summary((**session).clone(), context.models, 0);
            }
            SessionEvent::CheckInterruptionChanged { interruption } => {
                effects.push(StateEffect::CheckInterruptionChanged(*interruption));
            }
            SessionEvent::GoalUpdated { goal } => {
                if let Some(view) = self.get_mut(&session_id) {
                    view.goal = Some(goal.clone());
                }
            }
            SessionEvent::GoalCleared { goal_id, .. } => {
                if let Some(view) = self.get_mut(&session_id)
                    && view
                        .goal
                        .as_ref()
                        .is_some_and(|goal| goal.goal_id == *goal_id)
                {
                    view.goal = None;
                }
            }
            SessionEvent::SessionDeleted { session_id } => {
                effects.extend(self.remove_session(*session_id, context));
            }
            SessionEvent::PromptQueued {
                session, message, ..
            } => {
                self.upsert_summary((**session).clone(), context.models, 0);
                self.push_message(message.clone());
            }
            SessionEvent::RunStarted {
                session,
                run_id,
                plan,
                origin,
            } => {
                self.upsert_summary((**session).clone(), context.models, 0);
                if let Some(view) = self.get_mut(&session_id) {
                    let stats = view.runs.entry(*run_id).or_default();
                    stats.started_at_ms = Some(envelope.occurred_at_ms);
                    stats.origin = *origin;
                    stats.owned_step = origin
                        .is_some_and(qq_protocol::RunOrigin::is_in_run_compaction)
                        || session
                            .active_run_id
                            .is_some_and(|active| active != *run_id);
                    stats.plan = plan.as_deref().map(plan_label);
                    if let Some(messages) = view.messages.as_mut() {
                        for message in messages.iter_mut().filter(|message| {
                            message.run_id == *run_id && message.role == MessageRole::User
                        }) {
                            message.state = MessageState::Complete;
                        }
                    }
                }
            }
            SessionEvent::CancellationRequested { session, .. } => {
                self.upsert_summary((**session).clone(), context.models, 0);
            }
            SessionEvent::RunActivityChanged { run_id, activity } => {
                if let Some(session) = self.body_mut(&session_id)
                    && session
                        .summary
                        .active_run_id
                        .is_none_or(|active| active == *run_id)
                {
                    session.activity = Some((*run_id, *activity));
                }
            }
            SessionEvent::ReasoningStarted { run_id, .. } => {
                if let Some(session) = self.body_mut(&session_id).filter(|s| s.is_warm()) {
                    session.reasoning.entry(*run_id).or_default().streaming = true;
                }
            }
            SessionEvent::ReasoningDelta { run_id, text, .. } => {
                if let Some(session) = self.body_mut(&session_id).filter(|s| s.is_warm()) {
                    let reasoning = session.reasoning.entry(*run_id).or_default();
                    reasoning.streaming = true;
                    reasoning.append(text);
                }
            }
            SessionEvent::ReasoningCompleted { run_id, .. } => {
                if let Some(reasoning) = self
                    .body_mut(&session_id)
                    .and_then(|session| session.reasoning.get_mut(run_id))
                {
                    reasoning.streaming = false;
                }
            }
            SessionEvent::AssistantMessageStarted { message } => {
                let shown = context.focused == Some(session_id);
                self.complete_streamed_turns(
                    session_id,
                    message.run_id,
                    message.turn_ordinal.saturating_sub(1),
                );
                let sanitizer = self.sanitizer;
                if let Some(view) = self.body_mut(&session_id) {
                    if !shown {
                        view.unread += 1;
                    }
                    view.live.set_tail(&message.output, sanitizer);
                }
                self.push_message(message.clone());
            }
            SessionEvent::TextAppended {
                message_id,
                channel,
                text,
            } => {
                let sanitizer = self.sanitizer;
                if let Some(view) = self.body_mut(&session_id) {
                    if let Some(run_id) = envelope.run_id {
                        let stats = view.runs.entry(run_id).or_default();
                        if stats.first_token_at_ms.is_none() {
                            stats.first_token_at_ms = Some(envelope.occurred_at_ms);
                        }
                    }
                    if *channel == TextChannel::Output {
                        view.live.append_tail(text, sanitizer);
                    }
                }
                if let Some(message) = self.message_mut(session_id, *message_id) {
                    match channel {
                        TextChannel::Output => message.output.push_str(text),
                        TextChannel::Refusal => message.refusal.push_str(text),
                    }
                }
            }
            SessionEvent::ToolApprovalRequested {
                tool_call,
                shell,
                edit,
                question,
                fetch,
            } => {
                if let Some(session) = self.get_mut(&session_id) {
                    if tool_call.state == ToolCallState::AwaitingApproval && !context.attentive {
                        effects.push(StateEffect::Attention(Attention::ApprovalRequested {
                            session_title: session.summary.title.clone(),
                        }));
                    }
                    if shell.is_some() || edit.is_some() || question.is_some() || fetch.is_some() {
                        session.approval_previews.insert(
                            tool_call.id,
                            ApprovalPreview {
                                shell: shell.as_deref().cloned(),
                                edit: edit.clone(),
                                question: question.as_deref().cloned(),
                                fetch: fetch.as_deref().cloned(),
                            },
                        );
                    }
                    session.live.note_tool_call(tool_call);
                }
                self.upsert_tool_call(tool_call.clone());
            }
            SessionEvent::ToolCallOutputDelta {
                tool_call_id,
                chunk,
            } => {
                if let Some(session) = self.body_mut(&session_id) {
                    session.append_live_tool_output(*tool_call_id, chunk);
                    session
                        .tool_timing
                        .entry(*tool_call_id)
                        .or_default()
                        .last_output_at_ms = Some(envelope.occurred_at_ms);
                }
            }
            SessionEvent::ToolCallRequested { tool_call }
            | SessionEvent::ToolApprovalResolved { tool_call, .. }
            | SessionEvent::ToolCallStarted { tool_call }
            | SessionEvent::ToolCallFinished { tool_call } => {
                if let Some(view) = self.body_mut(&session_id) {
                    let timing = view.tool_timing.entry(tool_call.id).or_default();
                    match &envelope.event {
                        SessionEvent::ToolCallStarted { .. } => {
                            timing.started_at_ms = Some(envelope.occurred_at_ms);
                        }
                        SessionEvent::ToolCallFinished { .. } => {
                            timing.finished_at_ms = Some(envelope.occurred_at_ms);
                            view.runs.entry(tool_call.run_id).or_default().tool_calls += 1;
                        }
                        SessionEvent::ToolApprovalResolved { resolution, .. } => {
                            timing.settled = Some(ApprovalSettlement {
                                resolution: *resolution,
                            });
                        }
                        _ => {}
                    }
                }
                if matches!(envelope.event, SessionEvent::ToolCallRequested { .. }) {
                    self.complete_streamed_turns(
                        session_id,
                        tool_call.run_id,
                        tool_call.turn_ordinal,
                    );
                }
                if let Some(session) = self.get_mut(&session_id) {
                    if tool_call.state != ToolCallState::AwaitingApproval {
                        session.approval_previews.remove(&tool_call.id);
                    }
                    if tool_call_state_is_terminal(tool_call.state) {
                        session.live_tool_output.remove(&tool_call.id);
                    }
                    session.live.note_tool_call(tool_call);
                }
                self.upsert_tool_call(tool_call.clone());
            }
            SessionEvent::SteeringQueued { message, .. } => {
                self.push_message(message.clone());
            }
            SessionEvent::SteeringApplied { message_id, .. }
            | SessionEvent::SteeringSuperseded { message_id, .. } => {
                let state = match &envelope.event {
                    SessionEvent::SteeringApplied { .. } => MessageState::Complete,
                    _ => MessageState::Cancelled,
                };
                if let Some(messages) = self
                    .body_mut(&session_id)
                    .and_then(|session| session.messages.as_mut())
                    && let Some(message) = messages
                        .iter_mut()
                        .find(|message| message.id == *message_id)
                {
                    message.state = state;
                }
            }
            SessionEvent::RunInterrupted { .. } => {}
            SessionEvent::RunAuditStarted { .. } => {}
            SessionEvent::RunAuditCompleted { audit, .. } => {
                let text = match audit.outcome {
                    AuditOutcome::Pass => "audit passed".to_owned(),
                    AuditOutcome::Revised => format!(
                        "audit asked for a revision ({} finding{})",
                        audit.findings.len(),
                        if audit.findings.len() == 1 { "" } else { "s" }
                    ),
                    AuditOutcome::Unavailable => "audit unavailable; answer stands".to_owned(),
                };
                effects.push(StateEffect::Notice {
                    session: Some(session_id),
                    level: NoticeLevel::Info,
                    text,
                });
            }
            SessionEvent::Retired => {}
            SessionEvent::RunOutputTruncated {
                run_id,
                turn_ordinal,
                continuation,
            } => {
                if let Some(messages) = self
                    .body_mut(&session_id)
                    .and_then(|session| session.messages.as_mut())
                    && let Some(message) = messages.iter_mut().rev().find(|message| {
                        message.run_id == *run_id
                            && message.turn_ordinal == *turn_ordinal
                            && message.role == MessageRole::Assistant
                    })
                {
                    message.truncated = true;
                }
                let cap = context
                    .capabilities
                    .map(|capabilities| capabilities.limits.max_output_continuations)
                    .filter(|cap| *cap > 0);
                effects.push(StateEffect::Notice {
                    session: Some(session_id),
                    level: NoticeLevel::Info,
                    text: match cap {
                        Some(cap) => {
                            format!("output limit reached; continuing ({continuation}/{cap})")
                        }
                        None => format!("output limit reached; continuing ({continuation})"),
                    },
                });
            }
            SessionEvent::RunTurnRetrying {
                attempt,
                delay_ms,
                message,
                ..
            } => {
                effects.push(StateEffect::Notice {
                    session: Some(session_id),
                    level: NoticeLevel::Warning,
                    text: format!(
                        "provider fault; retrying turn in {}s ({attempt}/{}): {message}",
                        delay_ms.div_ceil(1_000),
                        qq_protocol::MAX_TURN_RETRIES
                    ),
                });
            }
            SessionEvent::SessionCompacted {
                session,
                summary,
                before_bytes,
                after_bytes,
            } => {
                self.upsert_summary((**session).clone(), context.models, 0);
                let mut text = format!(
                    "compacted: {} -> {}",
                    format_bytes(*before_bytes),
                    format_bytes(*after_bytes)
                );
                if let Some(summary) = summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    text.push_str("; ");
                    text.push_str(&excerpt(summary, MAX_COMPACTION_EXCERPT_CHARS));
                }
                effects.push(StateEffect::Notice {
                    session: Some(session_id),
                    level: NoticeLevel::Info,
                    text,
                });
            }
            SessionEvent::SessionCompactionRolledBack { session, remaining } => {
                self.upsert_summary((**session).clone(), context.models, 0);
                effects.push(StateEffect::Notice {
                    session: Some(session_id),
                    level: NoticeLevel::Info,
                    text: format!("compaction rolled back; {remaining} retained"),
                });
            }
            SessionEvent::ModelTurnCompleted {
                run_id,
                turn_ordinal,
                model,
                estimated_cost_usd_nanos,
                ..
            } => {
                if let Some(view) = self.body_mut(&session_id) {
                    let stats = view.runs.entry(*run_id).or_default();
                    if let Some(route) = &model.model {
                        stats.resolved_route = Some(route.clone());
                    }
                    if *turn_ordinal > stats.turns
                        && let Some(cost) = estimated_cost_usd_nanos
                    {
                        stats.live_cost_usd_nanos =
                            Some(stats.live_cost_usd_nanos.unwrap_or(0).saturating_add(*cost));
                    }
                    stats.turns = stats.turns.max(*turn_ordinal);
                }
            }
            SessionEvent::RunContextUpdated { .. } => {}
            SessionEvent::SessionContextUpdated { context_tokens, .. } => {
                if let Some(session) = self.get_mut(&session_id) {
                    session.summary.context_tokens = *context_tokens;
                }
            }
            SessionEvent::RunFinished {
                session,
                run_id,
                outcome,
                usage,
                origin,
                ..
            } => {
                let shown = context.focused == Some(session_id);
                let owned_step = origin.is_some_and(qq_protocol::RunOrigin::is_in_run_compaction)
                    || session
                        .active_run_id
                        .is_some_and(|active| active != *run_id)
                    || self
                        .get(&session_id)
                        .and_then(|view| view.runs.get(run_id))
                        .is_some_and(|stats| stats.owned_step);
                let owned_step = owned_step
                    || self.get(&session_id).is_some_and(|view| {
                        let finished_queued = view.summary.queued_prompts > session.queued_prompts;
                        !finished_queued
                            && view
                                .foreground_run_id
                                .is_some_and(|foreground| foreground != *run_id)
                    });
                self.upsert_summary((**session).clone(), context.models, 0);
                if !context.attentive && !owned_step {
                    effects.push(StateEffect::Attention(Attention::RunFinished {
                        session_title: session.title.clone(),
                    }));
                }
                let Some(view) = self.get_mut(&session_id) else {
                    return effects;
                };
                if !shown && !owned_step {
                    view.finished_unread = true;
                    view.unread += 1;
                }
                let stats = view.runs.entry(*run_id).or_default();
                stats.origin = (*origin).or(stats.origin);
                stats.owned_step |= owned_step;
                stats.finished_at_ms = Some(envelope.occurred_at_ms);
                stats.outcome = Some(outcome.clone());
                stats.usage = *usage;
                stats.cost_usd_nanos = None;
                if view.activity.is_some_and(|(active, _)| active == *run_id) {
                    view.activity = None;
                }
                if !owned_step {
                    view.live.active_tool = None;
                    view.live.awaiting_approval.clear();
                }
                if let Some(reasoning) = view.reasoning.get_mut(run_id) {
                    reasoning.streaming = false;
                }
                if let Some(messages) = view.messages.as_mut() {
                    let retained: HashSet<_> =
                        messages.iter().map(|message| message.run_id).collect();
                    view.reasoning.retain(|run, _| retained.contains(run));
                    view.runs.retain(|run, stats| {
                        retained.contains(run) || (stats.owned_step && stats.outcome.is_none())
                    });
                    let state = match outcome {
                        RunOutcome::Completed => MessageState::Complete,
                        RunOutcome::Cancelled => MessageState::Cancelled,
                        RunOutcome::Interrupted
                        | RunOutcome::BudgetExhausted { .. }
                        | RunOutcome::Paused { .. } => MessageState::Interrupted,
                        RunOutcome::Failed { .. } => MessageState::Failed,
                    };
                    for message in messages
                        .iter_mut()
                        .filter(|message| message.run_id == *run_id)
                    {
                        let settled = message.role == MessageRole::Assistant
                            && !matches!(
                                message.state,
                                MessageState::Queued | MessageState::Streaming
                            );
                        if !settled
                            && (message.role == MessageRole::Assistant
                                || message.state == MessageState::Queued)
                        {
                            message.state = state;
                        }
                    }
                }
                if owned_step {
                    return effects;
                }
                if (shown || view.is_warm())
                    && let Some(workspace_id) = context.workspace_id
                {
                    let mut request = body_request(workspace_id, session_id);
                    if !shown {
                        request.focused_session_id = None;
                        request.include_sessions.push(session_id);
                    }
                    effects.push(StateEffect::RequestSnapshot(request));
                }
                if session.active_run_id.is_none()
                    && session.queued_prompts == 0
                    && let Some(text) = view.drafts.pop_front()
                {
                    effects.push(StateEffect::SubmitDraft { session_id, text });
                }
                match outcome {
                    RunOutcome::Failed { failure } => effects.push(StateEffect::Notice {
                        session: Some(session_id),
                        level: NoticeLevel::Error,
                        text: failure.message.clone(),
                    }),
                    RunOutcome::BudgetExhausted { exhaustion } => {
                        effects.push(StateEffect::Notice {
                            session: Some(session_id),
                            level: NoticeLevel::Error,
                            text: exhaustion.message.clone(),
                        });
                    }
                    RunOutcome::Paused { pause } => {
                        let text = match pause.as_ref() {
                            qq_protocol::RunPause::ProviderRetry {
                                attempts,
                                turn_ordinal,
                                message,
                                ..
                            } => format!(
                                "paused after {attempts} retries of turn {turn_ordinal}: {message}"
                            ),
                            qq_protocol::RunPause::NoProgress { .. } => pause.to_string(),
                        };
                        effects.push(StateEffect::Notice {
                            session: Some(session_id),
                            level: NoticeLevel::Warning,
                            text: format!("{text}; send a prompt to continue"),
                        });
                    }
                    RunOutcome::Completed | RunOutcome::Cancelled | RunOutcome::Interrupted => {}
                }
            }
        }
        effects
    }

    fn remove_session(
        &mut self,
        session_id: SessionId,
        context: ReduceContext<'_>,
    ) -> StateEffects {
        let mut effects = StateEffects::new();
        if !self.contains_key(&session_id) {
            return effects;
        }
        let showing = context.focused == Some(session_id);
        let refocus = if showing {
            let order = self.thread_order();
            order
                .iter()
                .position(|candidate| *candidate == session_id)
                .and_then(|index| {
                    order
                        .get(index + 1)
                        .or_else(|| {
                            index
                                .checked_sub(1)
                                .and_then(|previous| order.get(previous))
                        })
                        .copied()
                })
        } else {
            None
        };
        let Some(removed) = self.remove(&session_id) else {
            return effects;
        };
        effects.push(StateEffect::SessionRemoved {
            session_id,
            tool_call_ids: removed
                .tool_calls
                .iter()
                .flatten()
                .map(|call| call.id)
                .collect(),
        });
        for session in self.values_mut() {
            if session.summary.parent_id == Some(session_id) {
                session.summary.parent_id = None;
            }
        }
        if showing {
            effects.push(StateEffect::Refocus(refocus));
            let warm = refocus
                .and_then(|next| self.get(&next))
                .is_some_and(SessionView::is_warm);
            if let (Some(next), Some(workspace_id), false) = (refocus, context.workspace_id, warm) {
                effects.push(StateEffect::RequestSnapshot(body_request(
                    workspace_id,
                    next,
                )));
            }
        }
        effects
    }
}

#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            #[expect(clippy::cast_precision_loss, reason = "display rounding only")]
            let value = bytes as f64 / scale as f64;
            return format!("{value:.1} {unit}");
        }
    }
    format!("{bytes} B")
}

fn excerpt(text: &str, width: usize) -> String {
    let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if plain.chars().count() <= width {
        plain
    } else {
        format!(
            "{}...",
            plain
                .chars()
                .take(width.saturating_sub(3))
                .collect::<String>()
        )
    }
}
