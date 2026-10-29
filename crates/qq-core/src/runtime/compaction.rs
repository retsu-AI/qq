use std::{future::Future, pin::Pin, sync::Arc};

use qq_protocol::RunFailureKind;
use qq_provider::{Message, ToolSpec};

pub(crate) struct SummaryFold {
    pending: Vec<Vec<Message>>,
    splits: u8,
    instruction_at: usize,
}

impl SummaryFold {
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

pub(crate) struct InRunCompactionRequest {
    pub(crate) transcript: Vec<Message>,
    pub(crate) turn_cutoff: u32,

    pub(crate) inherited_prefix: bool,
    pub(crate) system: Arc<str>,
    pub(crate) tools: Arc<[ToolSpec]>,
    pub(crate) retained_bytes: u64,
    pub(crate) goal_notice: Option<Arc<str>>,
    pub(crate) budget: super::ChildBudget,
}

pub(crate) struct InRunCompaction {
    pub(crate) summary: String,
    pub(crate) usage: Option<qq_protocol::TokenUsage>,
    pub(crate) estimated_cost_usd_nanos: Option<u64>,
}

pub(crate) type InRunCompactionFuture =
    Pin<Box<dyn Future<Output = Result<InRunCompaction, InRunCompactionError>> + Send + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InRunCompactionError {
    #[error("the summarizer's provider is unavailable: {message}")]
    SummarizerUnavailable {
        kind: RunFailureKind,
        message: String,
        retry_after: Option<std::time::Duration>,
        usage: Option<qq_protocol::TokenUsage>,
        estimated_cost_usd_nanos: Option<u64>,
    },
    #[error("summarizer budget exhausted: {kind:?}")]
    BudgetExhausted {
        kind: qq_protocol::BudgetLimitKind,
        usage: Option<qq_protocol::TokenUsage>,
        estimated_cost_usd_nanos: Option<u64>,
    },
    #[error("the summarizer failed: {0}")]
    SummarizerFailed(String),
    #[error("compaction could not run: {0}")]
    Unavailable(String),
    #[error("compaction could not be persisted: {0}")]
    Persistence(String),
}

pub(crate) trait InRunCompactor: Send + Sync {
    fn compact(&self, request: InRunCompactionRequest) -> InRunCompactionFuture;
}

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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SummarizeError {
    #[error("{message}")]
    RejectedBeforeWork {
        kind: RunFailureKind,
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    #[error("{message}")]
    Transient {
        kind: RunFailureKind,
        message: String,
        retry_after: Option<std::time::Duration>,
    },
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
