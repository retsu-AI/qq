use std::{collections::VecDeque, future::Future, pin::Pin};

use qq_protocol::ToolCallId;
use qq_provider::{ContentBlock, Message, Role};
use sha2::{Digest, Sha256};

pub const MAX_CHECKPOINT_TEXT_BYTES: usize = 24 * 1024;
const MAX_EVIDENCE_BYTES: usize = 16 * 1024;
const MAX_EVIDENCE_ITEM_BYTES: usize = 2048;
const MAX_EVIDENCE_ITEMS: usize = 32;

/// A bounded selection of observations, never a claim that omitted history was
/// assessed. Only enabled review runs allocate or maintain this projection.
pub(crate) struct CheckpointContext {
    pub(crate) task: String,
    pub(crate) task_overflow: bool,
    evidence: VecDeque<String>,
    evidence_bytes: usize,
    omitted: usize,
    reviews: u16,
    repairs: u8,
    pub(crate) verification: Option<qq_protocol::VerificationRecord>,
    last_observation: Option<String>,
    correction_generation: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointAdmissionError {
    #[error("Jev reached the per-run limit of 32 review requests")]
    Requests,
    #[error("Jev cannot run under a hard cost budget without a maximum request price")]
    UnknownPrice,
    #[error("Jev maximum request price exceeds the remaining run budget")]
    Cost,
}

impl CheckpointAdmissionError {
    pub(crate) fn budget_kind(&self) -> Option<qq_protocol::BudgetLimitKind> {
        match self {
            Self::Requests => None,
            Self::UnknownPrice => Some(qq_protocol::BudgetLimitKind::CostUnknown),
            Self::Cost => Some(qq_protocol::BudgetLimitKind::Cost),
        }
    }
}

impl CheckpointContext {
    pub(crate) fn new(messages: &[Message]) -> Self {
        let latest_user = messages
            .iter()
            .rposition(|message| message.role() == Role::User);
        let mut context = Self {
            task: String::new(),
            task_overflow: false,
            evidence: VecDeque::new(),
            evidence_bytes: 0,
            omitted: 0,
            reviews: 0,
            repairs: 0,
            verification: None,
            last_observation: None,
            correction_generation: 0,
        };
        for (index, message) in messages.iter().enumerate() {
            for block in message.content() {
                match block {
                    ContentBlock::Text { text } if Some(index) == latest_user => {
                        context.steer(text)
                    }
                    ContentBlock::Text { text } if message.role() == Role::User => {
                        context.record(format!("prior user context {index}: {text}"));
                    }
                    ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } => {
                        context.record(format!(
                            "prior tool observation {index}/{call_id}, error={is_error}: {content}"
                        ));
                    }
                    _ => {}
                }
            }
        }
        context
    }

    pub(crate) fn admit(
        &mut self,
        cost_limit: Option<u64>,
        maximum_cost: Option<u64>,
    ) -> Result<(), CheckpointAdmissionError> {
        if self.verification.is_none() && self.reviews >= 32 {
            return Err(CheckpointAdmissionError::Requests);
        }
        if let Some(limit) = cost_limit {
            match maximum_cost {
                None => {
                    return Err(CheckpointAdmissionError::UnknownPrice);
                }
                Some(cost) if cost > limit => {
                    return Err(CheckpointAdmissionError::Cost);
                }
                Some(_) => {}
            }
        }
        self.reviews = self.reviews.saturating_add(1);
        Ok(())
    }

    pub(crate) fn repair(&mut self) -> bool {
        if self.verification.is_some() {
            return true;
        }
        if self.repairs_exhausted() {
            return false;
        }
        self.repairs += 1;
        true
    }

    /// Both correction attempts are spent: later RED verdicts are recorded
    /// as evidence and no longer redirect the run.
    pub(crate) const fn repairs_exhausted(&self) -> bool {
        self.repairs >= 2
    }

    pub(crate) fn enable_strict(&mut self, reviewer: &str) {
        self.verification = Some(qq_protocol::VerificationRecord::pending(
            reviewer.to_owned(),
        ));
    }

    pub(crate) fn has_obligation(&self) -> bool {
        self.verification
            .as_ref()
            .is_some_and(|v| v.open_correction.is_some())
    }

    pub(crate) fn observe(&mut self, evidence: &str, is_error: bool) -> bool {
        let digest = format!("{:x}", Sha256::digest(format!("{is_error}:{evidence}")));
        if self.last_observation.as_ref() != Some(&digest) {
            if let Some(v) = &mut self.verification {
                v.evidence_generation = v.evidence_generation.saturating_add(1);
            }
            self.last_observation = Some(digest);
            true
        } else {
            false
        }
    }

    pub(crate) fn start(
        &mut self,
        request: &CheckpointRequest,
    ) -> Option<Box<qq_protocol::VerificationRecord>> {
        let v = self.verification.as_mut()?;
        // Length-prefixed canonical typed fields; only masked request bytes enter the hash.
        let basis = serde_json::to_vec(&(
            "qq-strict-checkpoint-v1",
            &request.correlation,
            match request.phase {
                CheckpointPhase::ToolResult => "tool_result",
                CheckpointPhase::FinalCandidate => "final_candidate",
            },
            request.tool_call_id,
            &request.tool,
            &request.task,
            &request.evidence,
            request.is_error,
            v.evidence_generation,
        ))
        .expect("checkpoint scalar fields serialize");
        v.state = qq_protocol::VerificationState::Pending;
        v.phase = Some(match request.phase {
            CheckpointPhase::ToolResult => qq_protocol::CheckpointPhase::ToolResult,
            CheckpointPhase::FinalCandidate => qq_protocol::CheckpointPhase::FinalCandidate,
        });
        v.correlation = Some(request.correlation.clone());
        v.tool_call_id = request.tool_call_id;
        v.outcome = None;
        v.reason.clear();
        v.basis_sha256 = Some(format!("{:x}", Sha256::digest(basis)));
        v.review_count = v.review_count.saturating_add(1);
        Some(Box::new(v.clone()))
    }

    pub(crate) fn reviewed(&mut self, outcome: CheckpointOutcome) {
        let Some(v) = &mut self.verification else {
            return;
        };
        if outcome == CheckpointOutcome::Supported {
            if v.evidence_generation > self.correction_generation {
                v.open_correction = None;
                v.correction_generation = None;
            }
        } else if outcome == CheckpointOutcome::Unavailable {
            v.state = qq_protocol::VerificationState::Unavailable;
        } else {
            v.state = qq_protocol::VerificationState::Unresolved;
            v.open_correction.clone_from(&v.correlation);
            self.correction_generation = v.evidence_generation;
            v.correction_generation = Some(v.evidence_generation);
        }
    }

    pub(crate) fn steer(&mut self, text: &str) {
        if self.task_overflow {
            return;
        }
        let text = crate::tools::output::mask_secrets(text.to_owned());
        if self
            .task
            .len()
            .saturating_add(text.len())
            .saturating_add(if self.task.is_empty() { 0 } else { 2 })
            > MAX_CHECKPOINT_TEXT_BYTES
        {
            self.task_overflow = true;
            return;
        }
        if !self.task.is_empty() {
            self.task.push_str("\n\n");
        }
        self.task.push_str(&text);
    }

    pub(crate) fn record(&mut self, text: String) {
        let mut text = crate::tools::output::mask_secrets(text);
        if text.len() > MAX_EVIDENCE_ITEM_BYTES {
            let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
            let original_bytes = text.len();
            let mut end = MAX_EVIDENCE_ITEM_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str(&format!("\n[excerpt only; remaining bytes omitted; full masked observation {original_bytes} bytes sha256:{digest}]"));
        }
        self.evidence_bytes += text.len();
        self.evidence.push_back(text);
        while self.evidence_bytes > MAX_EVIDENCE_BYTES || self.evidence.len() > MAX_EVIDENCE_ITEMS {
            let removed = self
                .evidence
                .pop_front()
                .expect("over budget implies evidence");
            self.evidence_bytes -= removed.len();
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    pub(crate) fn final_evidence(&self, answer: String) -> Option<String> {
        let answer = crate::tools::output::mask_secrets(answer);
        let mut evidence = format!(
            "final candidate:\n{answer}\n\nSelected recent evidence only; {omitted} earlier observations omitted from retention. Additional retained items may be omitted to fit this request. Excerpts and omissions are not proof. Missing required evidence must be reported.\n",
            omitted = self.omitted
        );
        if evidence.len() > MAX_CHECKPOINT_TEXT_BYTES {
            return None;
        }
        // Most recent observations first; whole retained items only. Their IDs
        // let the agent retrieve fresh evidence if a criterion is unsupported.
        for item in self.evidence.iter().rev() {
            if evidence.len() + item.len() + 1 > MAX_CHECKPOINT_TEXT_BYTES {
                break;
            }
            evidence.push_str(item);
            evidence.push('\n');
        }
        Some(evidence)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckpointPhase {
    ToolResult,
    FinalCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    pub spend: qq_protocol::CheckpointSpend,
}

pub type CheckpointFuture = Pin<Box<dyn Future<Output = CheckpointVerdict> + Send>>;

pub trait CheckpointReviewer: Send + Sync {
    /// Strict policy is part of the pinned identity inherited by child plans.
    fn requires_supported_completion(&self) -> bool {
        self.identity().ends_with("/strict")
    }
    fn identity(&self) -> &'static str {
        "custom/enforce"
    }
    /// Whether tool results require assessment before the next model turn.
    fn reviews_tools(&self) -> bool {
        true
    }
    /// Maximum estimated charge for one request. A hard cost allowance cannot
    /// admit a reviewer without a bound, even if previous calls were cheap.
    fn max_cost_usd_nanos(&self) -> Option<u64> {
        None
    }
    fn review(&self, request: CheckpointRequest) -> CheckpointFuture;
}

/// The runtime bounds even custom embedded reviewers. Dropping the future on
/// timeout stops local work; remote billing remains unknown until reported.
pub(crate) async fn assess_checkpoint(
    reviewer: &dyn CheckpointReviewer,
    request: CheckpointRequest,
    deadline: Option<tokio::time::Instant>,
) -> CheckpointVerdict {
    let limit = std::time::Duration::from_secs(5);
    let timeout = deadline.map_or(limit, |d| {
        d.saturating_duration_since(tokio::time::Instant::now())
            .min(limit)
    });
    match tokio::time::timeout(timeout, reviewer.review(request)).await {
        Ok(verdict) => verdict,
        Err(_) => CheckpointVerdict {
            outcome: CheckpointOutcome::Unavailable,
            confidence: None,
            feedback: if timeout < limit {
                "Jev assessment reached the run deadline; remote spend is unknown"
            } else {
                "Jev assessment timed out after five seconds; remote spend is unknown"
            }
            .to_owned(),
            spend: qq_protocol::CheckpointSpend::default(),
        },
    }
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
