//! Reduces durable `SessionEvent`s into the store: session summaries, loaded
//! transcript bodies, tool calls, live output tails, and the effects a
//! transition warrants. Every arm is idempotent against replay because the
//! caller has already deduplicated by cursor.

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

/// Characters of the compaction summary shown in its notice.
const MAX_COMPACTION_EXCERPT_CHARS: usize = 96;

/// Severity of a transient notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// An event worth interrupting the user for while the surface is not being
/// looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attention {
    /// A tool call is waiting for the user to approve it.
    ApprovalRequested { session_title: String },
    /// A run finished in a session; the user may want to read the result.
    RunFinished { session_title: String },
}

impl Attention {
    /// One-line text for a desktop notification.
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

/// What the surface is showing and knows, read by the reducer to decide
/// what is unread, who is watching, and whose command an event answers.
#[derive(Debug, Clone, Copy)]
pub struct ReduceContext<'a> {
    /// The session whose transcript is on screen, if any. Events for other
    /// sessions count as unread.
    pub focused: Option<SessionId>,
    /// Whether the user is looking at the surface. When false, approvals and
    /// finishes also produce [`StateEffect::Attention`].
    pub attentive: bool,
    /// The workspace this store follows, once known; needed to request a
    /// replacement body after a focused session is deleted.
    pub workspace_id: Option<WorkspaceId>,
    /// The server's capability document, for limit-aware notice text.
    pub capabilities: Option<&'a Arc<ServerCapabilities>>,
    /// Model catalog, for each session's context window.
    pub models: &'a [ModelOption],
    /// Whether this surface issued the command the event answers, when the
    /// surface tracks its own pending commands. `SessionCreated` uses it to
    /// decide whether to adopt the new session.
    pub caused_by_me: bool,
}

/// Something the reducer needs the surface to do. The store is already
/// updated when these are returned; the surface applies them in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEffect {
    CheckInterruptionChanged(Option<qq_protocol::CheckInterruption>),
    /// A transient notice for `session` (or the focused session).
    Notice {
        session: Option<SessionId>,
        level: NoticeLevel,
        text: String,
    },
    /// Ring for something that happened while the surface was inattentive.
    Attention(Attention),
    /// Fetch this body from the server (the refocus target is cold).
    RequestSnapshot(SnapshotRequest),
    /// Show this session (or the empty prompt): a shown session was deleted
    /// and its neighbour in thread order takes its place.
    Refocus(Option<SessionId>),
    /// This surface created the session; it is warm and should be shown.
    AdoptCreated(SessionId),
    /// The session's run finished idle and a queued draft is ready: submit it.
    SubmitDraft {
        session_id: SessionId,
        text: String,
    },
    /// A session left the store; drop anything keyed by it or its calls.
    SessionRemoved {
        session_id: SessionId,
        tool_call_ids: Vec<qq_protocol::ToolCallId>,
    },
}

/// The effects of one reduction, in application order.
pub type StateEffects = Vec<StateEffect>;

impl SessionStore {
    /// Rebuild a replaced body from retained events already observed live.
    /// Keep local unread state and drafts; preserve receipt refresh and deletion effects.
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

    /// Apply one durable event. The caller has already deduplicated by cursor
    /// and checked the workspace.
    pub fn reduce_event(
        &mut self,
        envelope: &SessionEventEnvelope,
        context: ReduceContext<'_>,
    ) -> StateEffects {
        let mut effects = StateEffects::new();
        let session_id = envelope.session_id;
        match &envelope.event {
            SessionEvent::SideQuestionUpdated { side_question } => {
                // Whether one runs is summary state, kept for every session
                // so the aggregate sees cold sessions too.
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
                // Side bodies follow the transcript body: a summary-only view
                // holds none, so cold sessions cannot accumulate them.
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
                // The activity names the session's active run. An in-run
                // compaction run reports its own while the prompt run still
                // holds the session; the prompt run already said
                // `compacting`, so that report is not the session's.
                if let Some(session) = self.body_mut(&session_id)
                    && session
                        .summary
                        .active_run_id
                        .is_none_or(|active| active == *run_id)
                {
                    session.activity = Some((*run_id, *activity));
                }
            }
            // Reasoning is display-only and must never enter the assistant
            // transcript. It accumulates per run for warm sessions so the
            // collapsed row above the run's message can expand on demand.
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
                // A new turn's message means every earlier turn of the run
                // has committed; the server finalized those messages inside
                // the turn persist without a dedicated event.
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
                    // Live status reduces for every session, warm or cold, so
                    // a sidebar tracks children the user is not looking at.
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
            // Live tool output chunks are display-only: they feed the bounded
            // tail under a running call's line, and the call's authoritative
            // bounded result arrives on ToolCallFinished regardless.
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
                        // How the hold was settled, so the row can say so once
                        // the call has run.
                        SessionEvent::ToolApprovalResolved { resolution, .. } => {
                            timing.settled = Some(ApprovalSettlement {
                                resolution: *resolution,
                            });
                        }
                        _ => {}
                    }
                }
                if matches!(envelope.event, SessionEvent::ToolCallRequested { .. }) {
                    // Calls are persisted with their completed turn, so the
                    // turn's message (same ordinal) is finalized by then.
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
                        // The persisted bounded result takes over from the tail.
                        session.live_tool_output.remove(&tool_call.id);
                    }
                    session.live.note_tool_call(tool_call);
                }
                self.upsert_tool_call(tool_call.clone());
            }
            // Steering rows are user messages of the active run, installed
            // from the event so they are durable before they are shown.
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
            // The interrupted turn's tool calls arrive as ordinary finished
            // events; nothing else changes.
            SessionEvent::RunInterrupted { .. } => {}
            // An audit child announces itself on the parent; its own session
            // arrives through ordinary events. Its verdict is a notice on the
            // audited session.
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
            // A journal row from a removed feature changes nothing.
            SessionEvent::Retired => {}
            // The truncated turn's message was already completed with its
            // `truncated` flag by the turn commit; the next turn's message
            // continues the same answer. Surface the continuation so the
            // user knows why the answer paused.
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
            // The partial turn's message was already completed by the turn
            // commit; the next turn's message continues the same answer.
            // Surface why the run went quiet.
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
                // The excerpt is the model's own statement of what it kept;
                // one bounded line of it tells the user what the compaction
                // preserved without opening anything.
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
            // Each committed model turn advances the run's live audit: how
            // many turns so far and what they have cost. `RunFinished` still
            // settles the authoritative totals.
            SessionEvent::ModelTurnCompleted {
                run_id,
                turn_ordinal,
                model,
                estimated_cost_usd_nanos,
                ..
            } => {
                if let Some(view) = self.body_mut(&session_id) {
                    let stats = view.runs.entry(*run_id).or_default();
                    // The turn names the route it ran on, which a profile may
                    // have overridden away from the session's selection.
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
            // The run-level context audit is not session state: old persisted
            // events may predate the authoritative session field, so replaying
            // one must not repopulate the meter.
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
                // A run that finishes while another run holds the session is
                // bookkeeping, not the end of the visible work: an in-run
                // compaction step (its prompt run keeps the slot; a retried
                // summarizer outage reports through the prompt run's
                // `run_turn_retrying` and final outcome instead), or a queued
                // prompt cancelled or interrupted behind the active run. No
                // attention, unread count or notice, and the active run's
                // live tool and approval state stay.
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
                        // Cancelling the last queued prompt has no RunStarted;
                        // its queue transition still releases the idle slot.
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
                // A session delta includes owned summaries and concurrent
                // routing/review spend. Only the run snapshot is authoritative.
                stats.cost_usd_nanos = None;
                // An in-run compaction run finishes while its prompt run
                // still holds the session; that run's activity stays.
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
                    // Reasoning for runs whose messages were trimmed away is
                    // unreachable from the transcript; drop it.
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
                        // Turns finalized before the run ended keep their own
                        // state; the outcome only settles the still-streaming
                        // current turn (and queued rows).
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
                // One draft per foreground run, in order.
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

    /// Drops a deleted session, mirroring the server's cascade: its children
    /// become roots, and a shown deleted session gives way to its neighbour
    /// in thread order (fetched if cold).
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
        // The server detaches children on delete; mirror it so they stay
        // reachable as roots until the next summary refresh.
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

/// A byte count for a notice: whole bytes below 1 KiB, one decimal of
/// KiB/MiB/GiB above it.
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

/// One line of `text`, whitespace collapsed, cut to `width` characters with
/// an ellipsis.
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
