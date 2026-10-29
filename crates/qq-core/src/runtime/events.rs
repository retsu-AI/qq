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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnNotice {
    Report,
    StallReport,
    RootStallReport,
    Continuation,
    FinalAnswer,
}

impl TurnNotice {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Continuation => "continuation",
            Self::StallReport => "stall_report",
            Self::RootStallReport => "root_stall_report",
            Self::FinalAnswer => "final_answer",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        match value {
            "report" => Some(Self::Report),
            "continuation" => Some(Self::Continuation),
            "stall_report" => Some(Self::StallReport),
            "root_stall_report" => Some(Self::RootStallReport),
            "final_answer" => Some(Self::FinalAnswer),
            _ => None,
        }
    }

    pub(crate) const fn text(self) -> &'static str {
        match self {
            Self::Report => crate::SLICE_CHECKPOINT_NOTICE,
            Self::Continuation => crate::SLICE_CONTINUATION_NOTICE,
            Self::StallReport => crate::STALL_REPORT_NOTICE,
            Self::RootStallReport => crate::ROOT_STALL_REPORT_NOTICE,
            Self::FinalAnswer => crate::SUBAGENT_FINAL_ANSWER_NOTICE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrderedTurnNotice {
    #[serde(deserialize_with = "deserialize_notice_text")]
    pub(crate) text: String,
    pub(crate) preceding_deliveries: usize,
}

fn deserialize_notice_text<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> Result<String, D::Error> {
    let text = <String as serde::Deserialize>::deserialize(decoder)?;
    if text.len() > crate::tools::goal::MAX_GOAL_RENDER_BYTES
        || !text.starts_with("[QQ runtime notice; not a user instruction]\n")
    {
        return Err(serde::de::Error::custom("invalid framed boundary notice"));
    }
    Ok(text)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelWorkDisposition {
    Committed,
    RejectedBeforeWork,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RuntimeEvent {
    GoalRequestRejected,
    GoalRequestAdmitted {
        cursor: Option<qq_protocol::EventCursor>,
    },
    GoalPaidCancelled,
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
        model_work: ModelWorkDisposition,
        turn_ordinal: u32,
        message: Message,
        usage: Option<TokenUsage>,
        calls: Vec<RuntimeToolCall>,
        truncated: bool,
        notice: Option<TurnNotice>,
        goal_notices: Vec<OrderedTurnNotice>,
        reserved_goal_notices: usize,
    },
    ToolCallStarted {
        id: ToolCallId,
        may_change_files: bool,
    },
    ToolCallDenied {
        id: ToolCallId,
        message: String,
    },
    ToolCallUnexecuted {
        id: ToolCallId,
        message: String,
        interrupted: bool,
    },
    ToolCallAnswered {
        id: ToolCallId,
        result: String,
    },
    ToolCallOutputDelta {
        id: ToolCallId,
        chunk: String,
    },
    ToolResultsProjected {
        turn_ordinal: u32,
        projection: Arc<ProjectedToolTurn>,
    },
    ToolCallFinished {
        id: ToolCallId,
        result: String,
        is_error: bool,
        file_states: Vec<FileStateUpdate>,
        display: Option<ToolCallDisplay>,
        spill: Option<crate::tools::SpillRecord>,
        execution_context: Option<Box<crate::tools::process_context::ExecutionContextReceipt>>,
    },
    Audited {
        outcome: qq_protocol::AuditOutcome,
        findings: Vec<String>,
        revisions: u16,
        usage: Option<TokenUsage>,
        cost_usd_nanos: Option<u64>,
        audit_session: Option<qq_protocol::SessionId>,
    },
    ContextPruned {
        turn_ordinal: u32,
    },
    InRunCompacted {
        turn_ordinal: u32,
        turn_cutoff: u32,
    },
    ProviderOverflow {
        turn_ordinal: u32,
        message: String,
    },
    SteeringApplied {
        message_id: MessageId,
        turn_ordinal: u32,
        attachments: Vec<crate::input::ResolvedAttachment>,
    },
    Interrupted {
        turn_ordinal: u32,
    },
    OutputTruncated {
        turn_ordinal: u32,
        continuation: u16,
    },
    TurnRetrying {
        turn_ordinal: u32,
        attempt: u16,
        delay: std::time::Duration,
        kind: RunFailureKind,
        message: String,
    },
    Paused {
        pause: Box<qq_protocol::RunPause>,
    },
    OutputRepairRequested {
        turn_ordinal: u32,
        repair: u8,
        errors: Vec<String>,
    },
    Completed {
        final_output: Option<Box<qq_protocol::FinalOutput>>,
    },
    Failed {
        kind: RunFailureKind,
        message: String,
    },
    BudgetExhausted {
        exhaustion: BudgetExhaustion,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectedToolTurn {
    pub(crate) message: Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallRejection {
    Runtime(String),
    OutputBudget,
}

impl CallRejection {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Runtime(message) => message,
            Self::OutputBudget => "turn_output_budget: search was not executed; retry the exact same arguments on the next turn".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeToolCall {
    pub(crate) id: ToolCallId,
    pub(crate) turn_ordinal: u32,
    pub(crate) call_ordinal: u16,
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) effect: EffectClass,
    pub(crate) rejection: Option<CallRejection>,
}

pub(crate) struct PendingToolCall {
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) rejection: Option<String>,
    pub(crate) completed: bool,
    pub(crate) oversized: bool,
}

pub(crate) enum TurnBlock {
    Text(String),
    ToolCall(usize),
}
