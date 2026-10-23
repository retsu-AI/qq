//! In-run compaction: summarizing a run's own earlier turns at a safe turn
//! boundary so the run continues in one window instead of failing.
//!
//! Between runs the session layer compacts before a prompt starts. Within a
//! run, once stubbing stale read-only results no longer fits the window, the
//! loop asks its installed compactor to summarize every turn but the most
//! recent few. The compactor runs the summarizer as an internal run and
//! commits a marker scoped to this run (`session_compactions.scope_run_id`,
//! `turn_cutoff`) so replay renders the summary where the replaced turns
//! stood. The loop then splices the summary into its live transcript and
//! continues. Direct runs have no compactor and fail as before.

use std::{future::Future, pin::Pin, sync::Arc};

use qq_protocol::RunFailureKind;
use qq_provider::{Message, ToolSpec};

/// Bounded hierarchical summarization. Splitting changes representation, not
/// coverage: every byte of transcript text is queued until a summary consumes
/// it. Tool calls/results are inert evidence, not executable messages.
pub(crate) struct SummaryFold {
    pending: Vec<Vec<Message>>,
    splits: u8,
    instruction_at: usize,
}

impl SummaryFold {
    /// The caller appends the instruction before any summary fragments or
    /// continuation notices. Its position is control data, not model text.
    pub(crate) fn new(instruction_at: usize) -> Self {
        Self {
            pending: Vec::new(),
            splits: 0,
            instruction_at,
        }
    }

    pub(crate) fn split(&mut self, messages: &mut Vec<Message>) -> bool {
        if self.splits >= 32 {
            return false;
        }
        let Some(instruction) = messages.get(self.instruction_at) else {
            return false;
        };
        if instruction.role() != qq_provider::Role::User {
            return false;
        }
        let transcript = &messages[..self.instruction_at];
        let mut text = String::new();
        for message in transcript {
            text.push_str(&format!("\n[{:?}]\n", message.role()));
            for block in message.content() {
                use qq_provider::ContentBlock;
                match block {
                    ContentBlock::Text { text: value } => text.push_str(value),
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        text.push_str(&format!("[Tool call {name}] {}", arguments.get()));
                    }
                    ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } => {
                        text.push_str(&format!(
                            "[Tool result {call_id}; error={is_error}] {content}"
                        ));
                    }
                }
                text.push('\n');
            }
        }
        let mut cut = text.len() / 2;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut < 1024 {
            return false;
        }
        let next = vec![Message::user(text[cut..].to_owned()), instruction.clone()];
        let first = vec![Message::user(text[..cut].to_owned()), instruction.clone()];
        if crate::measure_messages(&first) >= crate::measure_messages(messages) {
            return false;
        }
        self.splits += 1;
        self.pending.push(next);
        *messages = first;
        self.instruction_at = 1;
        true
    }

    /// Consume a completed portion only when another portion is queued. The
    /// caller keeps the final response without cloning it to probe the fold.
    pub(crate) fn advance(
        &mut self,
        summary: &mut String,
    ) -> Result<Option<Vec<Message>>, SummarizeError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        if summary.trim().is_empty() {
            return Err(SummarizeError::Rejected(
                "compaction produced an empty summary before all portions were incorporated"
                    .to_owned(),
            ));
        }
        let mut next = self
            .pending
            .pop()
            .expect("a pending portion was just checked");
        let mut preceding = std::mem::take(summary);
        preceding.insert_str(
            0,
            "Summary of the preceding portion; fold it together with the following evidence:\n",
        );
        next.insert(0, Message::user(preceding));
        self.instruction_at = next.len() - 1;
        Ok(Some(next))
    }
}

/// What the loop hands the compactor: the run's live request cut after the
/// last turn to replace, and the ordinal of that turn. The compactor sees no
/// retained turns, so it cannot summarize what stays verbatim. The request
/// keeps the run's system prompt, tools, and message prefix so it reads the
/// provider cache the run's own turns wrote (ADR-0056 § 5).
pub(crate) struct InRunCompactionRequest {
    /// The session context before the prompt, the prompt, and everything the
    /// run appended through `turn_cutoff`: assistant turns, tool results,
    /// applied steering, runtime notices.
    pub(crate) transcript: Vec<Message>,
    /// The last model turn ordinal the summary replaces. Turns after it stay
    /// verbatim in the live transcript and in replay.
    pub(crate) turn_cutoff: u32,

    /// Replace the inherited session prefix, not turns of the active prompt.
    pub(crate) inherited_prefix: bool,
    /// The run's system prompt (without a budget-final notice).
    pub(crate) system: Arc<str>,
    /// The run's declared tools. Calls are rejected, never run.
    pub(crate) tools: Arc<[ToolSpec]>,
    /// The live request's weight without the replaced span and without the
    /// summary: system prompt, tool schemas, session context before the
    /// prompt, the prompt, and the retained turns. The store adds the framed
    /// summary it commits and re-bases the run's context reservation to the
    /// total in the marker's transaction (ADR-0048 § 1).
    pub(crate) retained_bytes: u64,
    pub(crate) goal_notice: Option<Arc<str>>,
    pub(crate) budget: super::ChildBudget,
}

/// A committed in-run summary the loop splices in.
pub(crate) struct InRunCompaction {
    pub(crate) summary: String,
    /// Observed spend belongs to the internal run in storage, and is also
    /// charged to the prompt's caller allowance in memory.
    pub(crate) usage: Option<qq_protocol::TokenUsage>,
    pub(crate) estimated_cost_usd_nanos: Option<u64>,
}

pub(crate) type InRunCompactionFuture =
    Pin<Box<dyn Future<Output = Result<InRunCompaction, InRunCompactionError>> + Send + 'static>>;

/// Why an in-run compaction did not commit. The loop retries a transient
/// summarizer fault under the run's turn recovery and pauses the run when
/// that is spent (ADR-0048 § 2); it fails the run with the context diagnosis
/// for a rejected summary or an availability failure, and as a server
/// failure when the store could not record the compaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InRunCompactionError {
    /// The summarizer's provider faulted transiently (overload, rate limit,
    /// transport loss). The compaction run settled failed; the prompt run's
    /// turns are untouched, so the step can be retried.
    #[error("the summarizer's provider is unavailable: {message}")]
    SummarizerUnavailable {
        kind: RunFailureKind,
        message: String,
        usage: Option<qq_protocol::TokenUsage>,
        estimated_cost_usd_nanos: Option<u64>,
    },
    #[error("summarizer budget exhausted: {kind:?}")]
    BudgetExhausted {
        kind: qq_protocol::BudgetLimitKind,
        usage: Option<qq_protocol::TokenUsage>,
        estimated_cost_usd_nanos: Option<u64>,
    },
    /// The summarizer ran and settled failed (rejected, malformed, did not
    /// shrink). Its outcome is durable on its own run.
    #[error("the summarizer failed: {0}")]
    SummarizerFailed(String),
    /// The session layer could not start the compaction (the prompt run
    /// ended, it was cancelled, or the runtime has failed).
    #[error("compaction could not run: {0}")]
    Unavailable(String),
    /// The store failed to start, record, or commit the compaction. Not a
    /// context problem: `/compact` would not help.
    #[error("compaction could not be persisted: {0}")]
    Persistence(String),
}

pub(crate) trait InRunCompactor: Send + Sync {
    fn compact(&self, request: InRunCompactionRequest) -> InRunCompactionFuture;
}

/// Pull-driven summarizer boundaries. A request cannot be dispatched until
/// its starting step has been admitted, and each completed turn can be
/// persisted before the generator is polled for more provider work.
pub(crate) enum SummarizeStep {
    RequestStarting {
        input_tokens: Option<u64>,
    },
    TurnCompleted {
        model_work: crate::runtime::ModelWorkDisposition,
        ordinal: u32,
        message: Message,
        usage: Option<qq_protocol::TokenUsage>,
        truncated: bool,
    },
    Summary(String),
}

/// Why one summarizer request produced no summary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SummarizeError {
    /// A typed provider rejection before output, reasoning, replay or calls.
    #[error("{message}")]
    RejectedBeforeWork {
        kind: RunFailureKind,
        message: String,
    },
    /// A provider fault worth re-issuing the request for, as for a model turn.
    #[error("{message}")]
    Transient {
        kind: RunFailureKind,
        message: String,
    },
    /// The reply itself is unusable, or the provider refused the request in
    /// a way a retry would repeat.
    #[error("{0}")]
    Rejected(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting_a_continuation_keeps_the_user_instruction_instead_of_an_echo() {
        fn text(message: &Message) -> &str {
            match &message.content()[0] {
                qq_provider::ContentBlock::Text { text } => text,
                _ => panic!("expected inert text evidence"),
            }
        }
        let evidence = "original evidence 多字节 ".repeat(1024);
        let instruction = Message::user(crate::sessions::COMPACTION_INSTRUCTION);
        let mut messages = vec![Message::user(evidence.clone()), instruction.clone()];
        let mut fold = SummaryFold::new(messages.len() - 1);
        messages.push(Message::assistant(format!(
            "partial reply echoing {}",
            crate::sessions::COMPACTION_INSTRUCTION
        )));
        messages.push(Message::user("continue the partial reply"));
        assert!(fold.split(&mut messages));
        assert_eq!(messages.last(), Some(&instruction));
        let mut empty = " \n\t".to_owned();
        assert!(fold.advance(&mut empty).is_err());
        assert_eq!(empty, " \n\t");
        let mut summary = "summary of the first portion".to_owned();
        let next = fold.advance(&mut summary).unwrap().unwrap();
        assert_eq!(next.last(), Some(&instruction));
        assert_eq!(
            format!("{}{}", text(&messages[0]), text(&next[1])),
            format!("\n[User]\n{evidence}\n")
        );
    }
}
