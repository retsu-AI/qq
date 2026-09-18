use std::{future::Future, pin::Pin};

use qq_protocol::ToolCallId;

pub const MAX_CHECKPOINT_TEXT_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointPhase {
    ToolResult,
    FinalCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRequest {
    pub correlation: String,
    pub phase: CheckpointPhase,
    pub tool_call_id: Option<ToolCallId>,
    pub tool: Option<String>,
    pub task: String,
    pub evidence: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointOutcome {
    Supported,
    PartiallySupported,
    Contradicted,
    InsufficientEvidence,
    Unavailable,
}

impl CheckpointOutcome {
    pub const fn allows_progress(self) -> bool {
        matches!(self, Self::Supported)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::PartiallySupported => "partially_supported",
            Self::Contradicted => "contradicted",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointVerdict {
    pub outcome: CheckpointOutcome,
    pub confidence: Option<f64>,
    pub feedback: String,
}

pub type CheckpointFuture = Pin<Box<dyn Future<Output = CheckpointVerdict> + Send>>;

pub trait CheckpointReviewer: Send + Sync {
    fn identity(&self) -> &'static str {
        "custom/enforce"
    }
    fn review(&self, request: CheckpointRequest) -> CheckpointFuture;
}

pub(crate) fn bounded_checkpoint_text(text: &str) -> String {
    if text.len() <= MAX_CHECKPOINT_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_CHECKPOINT_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

pub(crate) fn checkpoint_text_fits(text: &str) -> bool {
    text.len() <= MAX_CHECKPOINT_TEXT_BYTES
}

#[cfg(test)]
mod tests {
    use super::{MAX_CHECKPOINT_TEXT_BYTES, checkpoint_text_fits};

    #[test]
    fn checkpoint_bound_accepts_boundary_and_rejects_first_byte_over() {
        assert!(checkpoint_text_fits(&"x".repeat(MAX_CHECKPOINT_TEXT_BYTES)));
        assert!(!checkpoint_text_fits(
            &"x".repeat(MAX_CHECKPOINT_TEXT_BYTES + 1)
        ));
    }

    #[test]
    fn final_wrapper_counts_against_the_checkpoint_bound() {
        let evidence = "x".repeat(MAX_CHECKPOINT_TEXT_BYTES);
        let payload = format!("final candidate:\nok\n\nretained tool evidence:\n{evidence}");
        assert!(!checkpoint_text_fits(&payload));
    }
}
