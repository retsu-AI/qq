//! One-shot decision observations. These types grant no execution authority.

use std::{collections::BTreeSet, future::Future, pin::Pin, time::Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DecisionState {
    Text(String),
    Json(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub state: DecisionState,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub instructions: String,
    pub kind: QuestionKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QuestionKind {
    Boolean {
        true_criterion: String,
        false_criterion: String,
    },
    Choice {
        labels: Vec<Label>,
    },
    Score {
        anchors: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestionType {
    Boolean,
    Choice,
    Score,
}

impl QuestionKind {
    pub fn question_type(&self) -> QuestionType {
        match self {
            Self::Boolean { .. } => QuestionType::Boolean,
            Self::Choice { .. } => QuestionType::Choice,
            Self::Score { .. } => QuestionType::Score,
        }
    }

    pub fn option_count(&self) -> usize {
        match self {
            Self::Boolean { .. } => 2,
            Self::Choice { labels } => labels.len(),
            Self::Score { anchors } => anchors.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub provenance: DecisionProvenance,
    pub usage: DecisionUsage,
    pub answers: Vec<DecisionAnswer>,
}

/// Reported identity is provenance, not attestation or qualification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionProvenance {
    pub requested_model: String,
    pub reported_model: Option<String>,
    pub request_id: Option<String>,
}

/// Missing usage stays unknown. Adapters reject malformed vendor counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    pub question_id: String,
    pub value: AnswerValue,
    /// Raw probabilities in request option order; Boolean uses false, true.
    /// Adapters must check exact vendor label sets before mapping this vector.
    pub probabilities: Option<Vec<f64>>,
    pub vendor_confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnswerValue {
    Boolean { probability_true: f64 },
    Choice { winner: String },
    Score { expected: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistributionFidelity {
    Full,
    WinnerOnly,
    ConfidenceOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelIdentity {
    Exact,
    Alias,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidencePreservation {
    RejectsOversize,
    MayTruncate,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionCapabilities {
    pub kinds: Vec<QuestionType>,
    pub max_questions: usize,
    pub max_options: usize,
    pub max_state_bytes: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub distribution: DistributionFidelity,
    /// Per-option absolute rounding error; not a policy confidence threshold.
    pub rounding_error: f64,
    pub model_identity: ModelIdentity,
    pub evidence_preservation: EvidencePreservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CapabilityError {
    #[error("invalid decision request: {reason}")]
    InvalidRequest { reason: &'static str },
    #[error("unsupported decision question kind: {kind:?}")]
    UnsupportedKind { kind: QuestionType },
    #[error("decision {resource} exceeds limit {limit}")]
    Limit {
        resource: &'static str,
        limit: usize,
    },
    #[error("invalid declared decision capabilities")]
    InvalidCapabilities,
}

impl DecisionCapabilities {
    pub fn validate(&self, request: &DecisionRequest) -> Result<(), CapabilityError> {
        if self.max_questions == 0
            || self.max_options < 2
            || self.max_state_bytes == 0
            || self.max_request_bytes == 0
            || self.max_response_bytes == 0
            || !self.rounding_error.is_finite()
            || !(0.0..0.5).contains(&self.rounding_error)
        {
            return Err(CapabilityError::InvalidCapabilities);
        }
        if request.questions.is_empty() {
            return Err(CapabilityError::InvalidRequest {
                reason: "no questions",
            });
        }
        if request.questions.len() > self.max_questions {
            return Err(CapabilityError::Limit {
                resource: "question count",
                limit: self.max_questions,
            });
        }
        let state_bytes = match &request.state {
            DecisionState::Text(text) => text.len(),
            DecisionState::Json(value) => encoded_size(value, self.max_state_bytes)?,
        };
        if state_bytes > self.max_state_bytes {
            return Err(CapabilityError::Limit {
                resource: "state bytes",
                limit: self.max_state_bytes,
            });
        }
        let mut ids = BTreeSet::new();
        for question in &request.questions {
            if question.id.is_empty() || !ids.insert(&question.id) {
                return Err(CapabilityError::InvalidRequest {
                    reason: "empty or duplicate question id",
                });
            }
            let kind = question.kind.question_type();
            if !self.kinds.contains(&kind) {
                return Err(CapabilityError::UnsupportedKind { kind });
            }
            if question.kind.option_count() < 2 {
                return Err(CapabilityError::InvalidRequest {
                    reason: "fewer than two options",
                });
            }
            if question.kind.option_count() > self.max_options {
                return Err(CapabilityError::Limit {
                    resource: "option count",
                    limit: self.max_options,
                });
            }
            if let QuestionKind::Choice { labels } = &question.kind {
                let mut labels_seen = BTreeSet::new();
                if labels
                    .iter()
                    .any(|label| label.id.is_empty() || !labels_seen.insert(&label.id))
                {
                    return Err(CapabilityError::InvalidRequest {
                        reason: "empty or duplicate label id",
                    });
                }
            }
        }
        encoded_size(&request, self.max_request_bytes)?;
        Ok(())
    }
}

// Count serialized bytes without allocating an additional unbounded buffer.
fn encoded_size(value: &impl Serialize, limit: usize) -> Result<usize, CapabilityError> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other("decision size limit"));
            }
            self.bytes += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    if serde_json::to_writer(&mut counter, value).is_err() {
        return Err(CapabilityError::Limit {
            resource: "encoded bytes",
            limit,
        });
    }
    Ok(counter.bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    NotDispatched,
    UncertainSend,
}

#[derive(Debug, Error)]
pub enum DecisionError {
    #[error(transparent)]
    Capability(#[from] CapabilityError),
    #[error("decision deadline expired ({dispatch:?})")]
    Deadline { dispatch: DispatchStatus },
    #[error("decision cancelled ({dispatch:?})")]
    Cancelled { dispatch: DispatchStatus },
    #[error("decision provider backpressure before dispatch")]
    Backpressure,
    #[error("decision transport failed ({dispatch:?}): {source}")]
    Transport {
        dispatch: DispatchStatus,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid decision provider response: {source}")]
    InvalidResponse {
        #[source]
        source: serde_json::Error,
    },
}

impl DecisionError {
    pub fn dispatch_status(&self) -> DispatchStatus {
        match self {
            Self::Capability(_) | Self::Backpressure => DispatchStatus::NotDispatched,
            Self::Deadline { dispatch }
            | Self::Cancelled { dispatch }
            | Self::Transport { dispatch, .. } => *dispatch,
            Self::InvalidResponse { .. } => DispatchStatus::UncertainSend,
        }
    }
}

pub type DecisionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DecisionResponse, DecisionError>> + Send + 'a>>;

/// Adapter transport contract. Direct implementers must enforce vendor wire
/// limits after encoding; application consumers use [`DecisionProvider`].
pub trait DecisionTransport: Send + Sync {
    fn capabilities(&self) -> &DecisionCapabilities;
    fn dispatch(&self, request: DecisionRequest, deadline: Instant) -> DecisionFuture<'_>;
}

/// Binds admission to the receiving transport's capabilities.
pub struct DecisionProvider {
    transport: Box<dyn DecisionTransport>,
}

impl DecisionProvider {
    pub fn new(transport: impl DecisionTransport + 'static) -> Self {
        Self {
            transport: Box::new(transport),
        }
    }

    pub fn capabilities(&self) -> &DecisionCapabilities {
        self.transport.capabilities()
    }

    /// Admission is not spend admission, egress consent or model qualification.
    pub async fn evaluate(
        &self,
        request: DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.capabilities().validate(&request)?;
        if Instant::now() >= deadline {
            return Err(DecisionError::Deadline {
                dispatch: DispatchStatus::NotDispatched,
            });
        }
        self.transport.dispatch(request, deadline).await
    }
}

pub async fn evaluate(
    provider: &DecisionProvider,
    request: DecisionRequest,
    deadline: Instant,
) -> Result<DecisionResponse, DecisionError> {
    provider.evaluate(request, deadline).await
}
