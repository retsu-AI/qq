use std::sync::Arc;

use qq_protocol::{
    BudgetExhaustion, ContentHash, MessageId, ReasoningKind, RunActivity, RunFailureKind,
    RunPromptIdentity, TokenUsage, ToolCallDisplay, ToolCallId,
};
use qq_provider::Message;
use sha2::{Digest, Sha256};

use crate::catalog::EffectClass;
use crate::workspace::FileStateUpdate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedRequestWeight {
    pub(crate) max_output_tokens: u32,
    pub(crate) system_bytes: u64,
    pub(crate) tool_schema_bytes: u64,
    pub(crate) reducible_message_bytes: u64,
    pub(crate) irreducible_message_bytes: u64,
    /// Provider-measured occupancy of the compatible preceding request plus
    /// the conservative byte weight appended since that request.
    pub(crate) compatible_input_tokens: Option<u64>,
}

impl PreparedRequestWeight {
    pub(crate) const fn input_bytes(self) -> u64 {
        self.system_bytes
            .saturating_add(self.tool_schema_bytes)
            .saturating_add(self.reducible_message_bytes)
            .saturating_add(self.irreducible_message_bytes)
    }
}

/// Identity of the exact immutable prefix placed before the conversation for
/// one provider turn. Tool-free checkpoint and compaction turns intentionally
/// differ from ordinary turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct PreparedStaticPrefix(ContentHash);

impl PreparedStaticPrefix {
    pub(crate) fn new(system: ContentHash, tools: Option<ContentHash>) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"qq-prepared-static-prefix-v1");
        digest.update(system.as_bytes());
        match tools {
            Some(tools) => {
                digest.update([1]);
                digest.update(tools.as_bytes());
            }
            None => digest.update([0]),
        }
        Self(ContentHash::from_bytes(digest.finalize().into()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RuntimeEvent {
    Started,
    Prepared {
        turn_ordinal: u32,
        identity: Option<Arc<RunPromptIdentity>>,
        static_prefix: PreparedStaticPrefix,
        weight: PreparedRequestWeight,
    },
    ActivityChanged {
        activity: RunActivity,
    },
    ReasoningStarted {
        kind: ReasoningKind,
    },
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    ReasoningCompleted {
        kind: ReasoningKind,
    },
    OutputTextDelta {
        text: String,
    },
    RefusalDelta {
        text: String,
    },
    AssistantTurnCompleted {
        turn_ordinal: u32,
        message: Message,
        usage: Option<TokenUsage>,
        /// Tool calls requested by this turn, in request order. Carried on the
        /// same event as the completed turn so the store can persist the turn
        /// and its calls in one transaction; a crash must never leave a
        /// persisted ToolCall block without its tool_calls rows.
        calls: Vec<RuntimeToolCall>,
        /// The provider stopped this turn at its output token limit. The
        /// message is a valid prefix; `calls` is always empty.
        truncated: bool,
    },
    ToolCallStarted {
        id: ToolCallId,
    },
    ToolCallDenied {
        id: ToolCallId,
        message: String,
    },
    /// An `ask_user` call the user answered. Like a denial, the session
    /// layer persisted and published the settled call before this fires;
    /// `result` is what the model reads.
    ToolCallAnswered {
        id: ToolCallId,
        result: String,
    },
    /// A chunk of live output from a running tool (shell commands stream their
    /// combined stdout+stderr). Display-only: the bounded result on
    /// `ToolCallFinished` remains authoritative.
    ToolCallOutputDelta {
        id: ToolCallId,
        chunk: String,
    },
    ToolCallFinished {
        id: ToolCallId,
        result: String,
        is_error: bool,
        /// File-state map entries recorded by this execution, persisted with
        /// the result so the map can be rebuilt for later runs.
        file_states: Vec<FileStateUpdate>,
        /// A UI-facing payload persisted with the result (the applied diff of
        /// a successful edit). Never enters model context.
        display: Option<ToolCallDisplay>,
        /// The complete output when `result` was cut and a session store
        /// will keep it under the handle the marker already names.
        spill: Option<crate::tools::SpillRecord>,
    },
    CheckpointStarted {
        correlation: String,
        phase: qq_protocol::CheckpointPhase,
        tool_call_id: Option<ToolCallId>,
    },
    CheckpointReviewed {
        correlation: String,
        phase: qq_protocol::CheckpointPhase,
        tool_call_id: Option<ToolCallId>,
        outcome: qq_protocol::CheckpointOutcome,
        confidence: Option<f64>,
        feedback: String,
        spend: Option<qq_protocol::CheckpointSpend>,
    },
    /// The final-answer auditor settled. Emitted before `Completed` (when the
    /// answer stands) or before the revision turn (when it does not); the
    /// store persists the record and charges the audit's spend to the run.
    Audited {
        outcome: qq_protocol::AuditOutcome,
        findings: Vec<String>,
        /// Revisions already spent before this audit (0 for the first).
        revisions: u16,
        usage: Option<TokenUsage>,
        cost_usd_nanos: Option<u64>,
        audit_session: Option<qq_protocol::SessionId>,
    },
    /// The approval reviewer answered for one of this run's held calls; its
    /// provider spend is the run's to account for. Emitted before the call
    /// executes or settles as denied.
    ReviewCharged {
        usage: Option<TokenUsage>,
        cost_usd_nanos: Option<u64>,
    },
    /// Queued steering entered model context: the message will be part of
    /// the request for `turn_ordinal`. Emitted at the boundary, before that
    /// turn is prepared.
    /// The run summarized its own turns through `turn_cutoff` before
    /// preparing `turn_ordinal`; the compactor already committed the marker.
    /// Informational for the session layer (occupancy is unknown again).
    InRunCompacted {
        turn_ordinal: u32,
        turn_cutoff: u32,
    },
    SteeringApplied {
        message_id: MessageId,
        turn_ordinal: u32,
        /// Files the applied message read, for the store to keep as this
        /// message's attachments. Empty for text-only steering.
        attachments: Vec<crate::input::ResolvedAttachment>,
    },
    /// An interrupting steer aborted turn `turn_ordinal` in flight. Emitted
    /// after the partial turn (if any text streamed) is committed via
    /// `AssistantTurnCompleted` and before its unfinished calls are settled;
    /// the store marks every call of the turn still open as interrupted.
    Interrupted {
        turn_ordinal: u32,
    },
    /// The provider cut turn `turn_ordinal` at its output token limit. Emitted
    /// after the partial turn is committed via `AssistantTurnCompleted`; the
    /// loop then continues with the next turn. `continuation` is 1-based.
    OutputTruncated {
        turn_ordinal: u32,
        continuation: u16,
    },
    /// A transient provider fault ended turn `turn_ordinal` after its first
    /// event. Emitted after the partial turn is committed via
    /// `AssistantTurnCompleted`; the loop then sleeps `delay` and re-issues
    /// the turn. `attempt` is 1-based within this turn's recovery.
    TurnRetrying {
        turn_ordinal: u32,
        attempt: u16,
        delay: std::time::Duration,
        kind: RunFailureKind,
        message: String,
    },
    /// Turn recovery exhausted its allowance on a transient provider fault.
    /// Every completed turn is durable; the run is resumable by the next
    /// prompt rather than failed.
    Paused {
        pause: Box<qq_protocol::RunPause>,
    },
    /// The final answer failed its output contract and the loop is about to
    /// spend repair turn `repair` (1-based) on it. Emitted after the failing
    /// turn is committed via `AssistantTurnCompleted`; `errors` is bounded.
    OutputRepairRequested {
        turn_ordinal: u32,
        repair: u8,
        errors: Vec<String>,
    },
    /// The run reached a final answer. `final_output` is the contract
    /// verdict for a run claimed with one, `None` otherwise.
    Completed {
        final_output: Option<Box<qq_protocol::FinalOutput>>,
    },
    Failed {
        kind: RunFailureKind,
        message: String,
    },
    /// A caller-imposed limit settled the run. Emitted after the reserved
    /// final response turn (if any) has been persisted via
    /// `AssistantTurnCompleted`.
    BudgetExhausted {
        exhaustion: BudgetExhaustion,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeToolCall {
    pub(crate) id: ToolCallId,
    pub(crate) turn_ordinal: u32,
    pub(crate) call_ordinal: u16,
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    /// The catalog's effect for `name`, resolved once when the call is
    /// admitted. Policy classifies from this, never from the name. A name
    /// absent from the catalog is a tool error and never reaches the gate.
    pub(crate) effect: EffectClass,
    /// Set when the call cannot execute: the provider streamed arguments that
    /// were not valid JSON, or the name is not in the catalog. The call never
    /// reaches the gate; this message is returned to the model as a
    /// retryable tool error instead of failing the run.
    pub(crate) rejection: Option<String>,
}

pub(crate) struct PendingToolCall {
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    /// While streaming, the raw argument text as the provider sent it; once
    /// completed, its compact canonical re-encoding (or `{}` when the text
    /// was not JSON, with `rejection` set). The transcript embeds this text
    /// verbatim, so it is never re-serialized.
    pub(crate) arguments: String,
    pub(crate) rejection: Option<String>,
    pub(crate) completed: bool,
}

pub(crate) enum TurnBlock {
    Text(String),
    ToolCall(usize),
}
