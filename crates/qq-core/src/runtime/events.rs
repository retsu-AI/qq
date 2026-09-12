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
        /// A file-state map entry recorded by this execution, persisted with
        /// the result so the map can be rebuilt for later runs.
        file_state: Option<FileStateUpdate>,
        /// A UI-facing payload persisted with the result (the applied diff of
        /// a successful edit). Never enters model context.
        display: Option<ToolCallDisplay>,
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
    SteeringApplied {
        message_id: MessageId,
        turn_ordinal: u32,
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
    Completed,
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
    pub(crate) arguments: String,
    pub(crate) parsed_arguments: Option<serde_json::Value>,
    pub(crate) rejection: Option<String>,
    pub(crate) completed: bool,
}

pub(crate) enum TurnBlock {
    Text(String),
    ToolCall(usize),
}
