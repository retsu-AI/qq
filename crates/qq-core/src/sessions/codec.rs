//! Column codecs for the store: string forms of every persisted enum and
//! identifier, run limits and correlation JSON, and the persisted content-block
//! shape. Every parse maps to `SessionRuntimeError::CODEC`.

use super::*;

pub(super) fn parse_run_kind(value: &str) -> Result<RunKind, SessionRuntimeError> {
    match value {
        "prompt" => Ok(RunKind::Prompt),
        "compaction" => Ok(RunKind::Compaction),
        _ => Err(SessionRuntimeError::CODEC),
    }
}

/// Encodes a correlation map for its column: NULL for the empty map so
/// historical rows and unattributed rows are indistinguishable.
pub(super) fn encode_correlation(
    correlation: &Correlation,
) -> Result<Option<String>, SessionRuntimeError> {
    if correlation.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(correlation)
        .map(Some)
        .map_err(|_| SessionRuntimeError::CODEC)
}

pub(super) fn parse_correlation(encoded: Option<&str>) -> Result<Correlation, SessionRuntimeError> {
    match encoded {
        None => Ok(Correlation::default()),
        Some(encoded) => serde_json::from_str(encoded).map_err(|_| SessionRuntimeError::CODEC),
    }
}

pub(super) fn parse_profile(encoded: Option<&str>) -> Result<AgentProfileId, SessionRuntimeError> {
    match encoded {
        None => Ok(AgentProfileId::default()),
        Some(encoded) => encoded.parse().map_err(|_| SessionRuntimeError::CODEC),
    }
}

pub(super) fn parse_input_parts(
    encoded: Option<&str>,
) -> Result<Vec<InputPart>, SessionRuntimeError> {
    match encoded {
        None => Ok(Vec::new()),
        Some(encoded) => serde_json::from_str(encoded).map_err(|_| SessionRuntimeError::CODEC),
    }
}

/// NULL is the historical unlimited run; stored JSON is authoritative and a
/// malformed row is a persistence fault rather than a silent default.
pub(super) fn parse_run_limits(encoded: Option<&str>) -> Result<RunLimits, SessionRuntimeError> {
    match encoded {
        None => Ok(RunLimits::default()),
        Some(encoded) => serde_json::from_str(encoded).map_err(|_| SessionRuntimeError::CODEC),
    }
}

pub(super) const fn run_activity_column(activity: RunActivity) -> &'static str {
    match activity {
        RunActivity::WaitingForProvider => "waiting_for_provider",
        RunActivity::Reasoning => "reasoning",
        RunActivity::GeneratingResponse => "generating_response",
        RunActivity::PreparingToolCall => "preparing_tool_call",
    }
}

pub(super) fn parse_run_activity(column: &str) -> Result<RunActivity, SessionRuntimeError> {
    match column {
        "waiting_for_provider" => Ok(RunActivity::WaitingForProvider),
        "reasoning" => Ok(RunActivity::Reasoning),
        "generating_response" => Ok(RunActivity::GeneratingResponse),
        "preparing_tool_call" => Ok(RunActivity::PreparingToolCall),
        _ => Err(SessionRuntimeError::CODEC),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PersistedContentBlock {
    Text {
        text: String,
    },
    /// `arguments` is stored as the JSON object itself, not a string. It is a
    /// `Value` here because serde buffers the content of an internally tagged
    /// enum, which a `RawValue` cannot survive; the transcript block keeps
    /// the compact text and the conversion happens once at load, not per
    /// request.
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl From<&ContentBlock> for PersistedContentBlock {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => Self::Text { text: text.clone() },
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id: id.clone(),
                name: name.clone(),
                // The text is canonical serde_json output; parsing cannot
                // fail, and a corrupted block must not be persisted silently.
                arguments: serde_json::from_str(arguments.get())
                    .expect("transcript tool-call arguments are valid JSON"),
            },
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        }
    }
}

impl From<PersistedContentBlock> for ContentBlock {
    fn from(block: PersistedContentBlock) -> Self {
        match block {
            PersistedContentBlock::Text { text } => Self::Text { text },
            PersistedContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::tool_call(id, name, &arguments),
            PersistedContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id,
                content,
                is_error,
            },
        }
    }
}

pub(super) fn parse_id<T>(value: &str) -> Result<T, SessionRuntimeError>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| SessionRuntimeError::CODEC)
}

pub(super) fn parse_session_status(value: &str) -> Result<SessionStatus, SessionRuntimeError> {
    match value {
        "idle" => Ok(SessionStatus::Idle),
        "queued" => Ok(SessionStatus::Queued),
        "running" => Ok(SessionStatus::Running),
        _ => Err(SessionRuntimeError::CODEC),
    }
}

pub(super) fn parse_run_status(value: &str) -> Result<RunStatus, SessionRuntimeError> {
    match value {
        "queued" => Ok(RunStatus::Queued),
        "running" => Ok(RunStatus::Running),
        "completed" => Ok(RunStatus::Completed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "failed" => Ok(RunStatus::Failed),
        "interrupted" => Ok(RunStatus::Interrupted),
        "budget_exhausted" => Ok(RunStatus::BudgetExhausted),
        "paused" => Ok(RunStatus::Paused),
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

pub(super) fn parse_message_role(value: &str) -> Result<MessageRole, SessionRuntimeError> {
    match value {
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        _ => Err(SessionRuntimeError::CODEC),
    }
}

pub(super) fn parse_message_state(value: &str) -> Result<MessageState, SessionRuntimeError> {
    match value {
        "queued" => Ok(MessageState::Queued),
        "streaming" => Ok(MessageState::Streaming),
        "complete" => Ok(MessageState::Complete),
        "cancelled" => Ok(MessageState::Cancelled),
        "failed" => Ok(MessageState::Failed),
        "interrupted" => Ok(MessageState::Interrupted),
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

pub(super) const fn approval_mode_str(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::Supervised => "supervised",
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
        ApprovalMode::Full => "full",
    }
}

pub(super) fn parse_approval_mode(value: &str) -> Result<ApprovalMode, SessionRuntimeError> {
    match value {
        "read_only" => Ok(ApprovalMode::ReadOnly),
        "supervised" => Ok(ApprovalMode::Supervised),
        "ask" => Ok(ApprovalMode::Ask),
        "auto" => Ok(ApprovalMode::Auto),
        "full" => Ok(ApprovalMode::Full),
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

/// NULL is no session override: the configured `approval_delegate` applies.
/// A stored value is the session's own choice for the rest of the session.
pub(super) fn parse_approval_delegate(
    encoded: Option<&str>,
) -> Result<Option<qq_protocol::ApprovalDelegate>, SessionRuntimeError> {
    match encoded {
        None => Ok(None),
        Some("by_mode") => Ok(Some(qq_protocol::ApprovalDelegate::ByMode)),
        Some("on") => Ok(Some(qq_protocol::ApprovalDelegate::On)),
        Some("off") => Ok(Some(qq_protocol::ApprovalDelegate::Off)),
        Some(_) => Err(SessionRuntimeError::CODEC),
    }
}

pub(super) fn approval_delegate_column(
    delegate: Option<qq_protocol::ApprovalDelegate>,
) -> Option<&'static str> {
    delegate.map(qq_protocol::ApprovalDelegate::as_str)
}

/// NULL is omission (provider/config defaults). A stored value is the session's
/// explicit pin for the next claim.
pub(super) fn parse_reasoning_effort(
    encoded: Option<&str>,
) -> Result<Option<qq_provider::ReasoningEffort>, SessionRuntimeError> {
    match encoded {
        None => Ok(None),
        Some("none") => Ok(Some(qq_provider::ReasoningEffort::None)),
        Some("minimal") => Ok(Some(qq_provider::ReasoningEffort::Minimal)),
        Some("low") => Ok(Some(qq_provider::ReasoningEffort::Low)),
        Some("medium") => Ok(Some(qq_provider::ReasoningEffort::Medium)),
        Some("high") => Ok(Some(qq_provider::ReasoningEffort::High)),
        Some("xhigh") => Ok(Some(qq_provider::ReasoningEffort::Xhigh)),
        Some("max") => Ok(Some(qq_provider::ReasoningEffort::Max)),
        Some(_) => Err(SessionRuntimeError::CODEC),
    }
}

pub(super) fn reasoning_effort_column(
    effort: Option<qq_provider::ReasoningEffort>,
) -> Option<&'static str> {
    effort.map(qq_provider::ReasoningEffort::as_str)
}

/// Authority order of approval modes: a higher rank executes strictly more
/// without a human than a lower one.
pub(super) const fn approval_rank(mode: ApprovalMode) -> u8 {
    match mode {
        ApprovalMode::ReadOnly => 0,
        // Supervised executes nothing non-read without adjudication; Ask lets
        // grants through, so it ranks above.
        ApprovalMode::Supervised => 1,
        ApprovalMode::Ask => 2,
        ApprovalMode::Auto => 3,
        ApprovalMode::Full => 4,
    }
}

pub(super) const fn approval_resolution_str(resolution: ApprovalResolution) -> &'static str {
    match resolution {
        ApprovalResolution::ApprovedOnce => "approved_once",
        ApprovalResolution::ApprovedForSession => "approved_for_session",
        ApprovalResolution::ApprovedForWorkspace => "approved_for_workspace",
        ApprovalResolution::ApprovedByReviewer => "approved_by_reviewer",
        ApprovalResolution::Denied => "denied",
        ApprovalResolution::DeniedTimeout => "denied_timeout",
        ApprovalResolution::DeniedByReviewer => "denied_by_reviewer",
        ApprovalResolution::Answered => "answered",
    }
}

pub(super) fn parse_approval_resolution(
    value: &str,
) -> Result<ApprovalResolution, SessionRuntimeError> {
    match value {
        "approved_once" => Ok(ApprovalResolution::ApprovedOnce),
        "approved_for_session" => Ok(ApprovalResolution::ApprovedForSession),
        "approved_for_workspace" => Ok(ApprovalResolution::ApprovedForWorkspace),
        "approved_by_reviewer" => Ok(ApprovalResolution::ApprovedByReviewer),
        "denied" => Ok(ApprovalResolution::Denied),
        "denied_timeout" => Ok(ApprovalResolution::DeniedTimeout),
        "denied_by_reviewer" => Ok(ApprovalResolution::DeniedByReviewer),
        "answered" => Ok(ApprovalResolution::Answered),
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

pub(super) fn parse_tool_call_state(value: &str) -> Result<ToolCallState, SessionRuntimeError> {
    match value {
        "requested" => Ok(ToolCallState::Requested),
        "awaiting_approval" => Ok(ToolCallState::AwaitingApproval),
        "running" => Ok(ToolCallState::Running),
        "completed" => Ok(ToolCallState::Completed),
        "failed" => Ok(ToolCallState::Failed),
        "denied" => Ok(ToolCallState::Denied),
        "interrupted" => Ok(ToolCallState::Interrupted),
        _ => Err(SessionRuntimeError::CONSTRAINT),
    }
}

pub(super) fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
