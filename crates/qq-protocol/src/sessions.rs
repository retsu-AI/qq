use std::{fmt, num::NonZeroU16, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{
    CommandId, MessageId, ReasoningKind, RunFailureKind, RunId, SessionId, StoreId, ToolCallId,
    WorkspaceId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub store_id: StoreId,
    pub workspace_id: WorkspaceId,
    pub sequence: u64,
}

impl fmt::Display for EventCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}:{}",
            self.store_id, self.workspace_id, self.sequence
        )
    }
}

impl FromStr for EventCursor {
    type Err = CursorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(':');
        let store_id = parts
            .next()
            .ok_or(CursorError)?
            .parse()
            .map_err(|_| CursorError)?;
        let workspace_id = parts
            .next()
            .ok_or(CursorError)?
            .parse()
            .map_err(|_| CursorError)?;
        let sequence = parts
            .next()
            .ok_or(CursorError)?
            .parse()
            .map_err(|_| CursorError)?;
        if parts.next().is_some() {
            return Err(CursorError);
        }
        Ok(Self {
            store_id,
            workspace_id,
            sequence,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorError;

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid event cursor")
    }
}

impl std::error::Error for CursorError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
}

/// Usage and estimated cost derived from the durable runs owned by a session.
/// A missing usage or cost means the aggregate is unavailable or unknown; it
/// is never interchangeable with a known zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountingTotal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
}

/// Direct accounting covers runs owned by this session. Inclusive accounting
/// adds runs owned directly by its immediate children. Both are explicit so
/// consumers never need to join the session tree or guess which total a field
/// represents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAccounting {
    pub direct: AccountingTotal,
    pub inclusive: AccountingTotal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_usd_nanos_per_token: u64,
    pub output_usd_nanos_per_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tier: Option<ModelPricingTier>,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricingTier {
    pub above_input_tokens: u64,
    pub input_usd_nanos_per_token: u64,
    pub output_usd_nanos_per_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_nanos_per_token: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogRequest {
    pub workspace: String,
    pub selection: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    pub selection: ModelSelection,
}

/// Per-session policy for tool calls that mutate state or leave the workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    ReadOnly,
    Ask,
    /// Default: edits and safe shell run without prompting; only dangerous
    /// shell commands (deletion, privilege escalation, force-push, piped
    /// installers) require approval.
    #[default]
    Auto,
    /// Zero restrictions: every tool call executes without prompting.
    Full,
}

/// A client's answer to one pending tool approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalDecision {
    ApproveOnce,
    ApproveForSession {
        grant: ApprovalGrant,
    },
    /// Approve like `ApproveForSession` and additionally request that the
    /// grant be promoted into the workspace configuration. The promotion's
    /// fate arrives later as a `workspace_grant_promoted` event; a failed
    /// promotion never fails the approval.
    ApproveForWorkspace {
        grant: ApprovalGrant,
    },
    Deny,
}

/// A session-scoped allowlist entry recorded by approve-for-session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalGrant {
    Tool { name: String },
    ShellPrefix { prefix: String },
}

/// The durable outcome of one tool approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolution {
    ApprovedOnce,
    ApprovedForSession,
    /// Approved with a session grant plus a requested promotion into the
    /// workspace configuration.
    ApprovedForWorkspace,
    /// Approved by the configured approval reviewer model, without a human
    /// in the loop. Carries no grant: the approval covers this call only.
    ApprovedByReviewer,
    Denied,
    DeniedTimeout,
    /// Denied by the configured approval reviewer model. Reserved: the
    /// current reviewer escalates to a human instead of denying, but the
    /// wire vocabulary is versioned with the feature.
    DeniedByReviewer,
}

/// The durable fate of one workspace-lifetime grant promotion, carried by
/// `workspace_grant_promoted`. Failures are informational: the session grant
/// recorded by the approval stands regardless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceGrantOutcome {
    /// The grant was appended to the workspace configuration file.
    Written { path: String },
    /// The configuration file already declared the grant; nothing changed.
    AlreadyPresent { path: String },
    /// The grant could not be persisted (managed deny, IO error, or a server
    /// without a workspace grant store).
    Failed { message: String },
}

/// Shell details carried by an approval request so clients can decide in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellCommandPreview {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Edit details carried by an approval request so clients can decide in place:
/// the workspace-relative path and a bounded unified-diff-style preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditPreview {
    pub path: String,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCommand {
    ResolveWorkspace {
        path: String,
    },
    CreateSession {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<SessionId>,
        model: ModelSelection,
        #[serde(default)]
        approval_mode: ApprovalMode,
    },
    SubmitPrompt {
        session_id: SessionId,
        prompt: String,
    },
    CancelRun {
        run_id: RunId,
    },
    RespondToolApproval {
        run_id: RunId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    },
    SetApprovalMode {
        session_id: SessionId,
        mode: ApprovalMode,
    },
    /// Repoints the session's model. Takes effect when the next run is
    /// claimed; a run already executing keeps the model it started with.
    SetSessionModel {
        session_id: SessionId,
        model: ModelSelection,
    },
    /// Deletes an idle session and every row it owns. Refused while the
    /// session has an active run; the client cancels first.
    DeleteSession {
        session_id: SessionId,
    },
    /// Deletes every idle session in the workspace that never received a
    /// message (the residue of creating sessions without prompting them).
    PruneSessions {
        workspace_id: WorkspaceId,
    },
    /// Compacts the session's model context: an internal summarization run
    /// replaces everything before a new cutoff marker with a structured
    /// summary. Valid only while the session is idle; refused while a run is
    /// active or queued. The client transcript is untouched.
    CompactSession {
        session_id: SessionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub command_id: CommandId,
    pub command: SessionCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub command_id: CommandId,
    pub committed_through: EventCursor,
    pub outcome: CommandOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandOutcome {
    WorkspaceResolved {
        workspace_id: WorkspaceId,
    },
    SessionCreated {
        session_id: SessionId,
    },
    PromptQueued {
        session_id: SessionId,
        run_id: RunId,
        queue_position: u16,
    },
    CancellationRequested {
        run_id: RunId,
    },
    RunAlreadyFinished {
        run_id: RunId,
        outcome: RunOutcome,
    },
    ToolApprovalResolved {
        tool_call_id: ToolCallId,
        resolution: ApprovalResolution,
    },
    ApprovalModeSet {
        session_id: SessionId,
        mode: ApprovalMode,
    },
    SessionModelSet {
        session_id: SessionId,
        model: ModelSelection,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    SessionsPruned {
        workspace_id: WorkspaceId,
        deleted: u32,
    },
    CompactionQueued {
        session_id: SessionId,
        run_id: RunId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Queued,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunActivity {
    /// The request is being assembled or QQ is waiting for the provider's
    /// first meaningful stream event.
    WaitingForProvider,
    /// The provider is streaming displayable reasoning or a reasoning summary.
    Reasoning,
    /// The provider is streaming user-visible assistant text.
    GeneratingResponse,
    /// The provider is constructing one or more tool calls. Arguments may be
    /// incomplete and are deliberately not exposed by this status channel.
    PreparingToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Cancelled,
    Interrupted,
    Failed { failure: RunFailure },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFailure {
    pub kind: RunFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Queued,
    Streaming,
    Complete,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Requested,
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Denied,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextChannel {
    Output,
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    pub title: String,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<RunId>,
    pub queued_prompts: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Input-token total of the latest measured prompt turn for this
    /// session. This is session state rather than arbitrary run history:
    /// compaction and model changes clear it until the next prompt turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Structured durable accounting. Absent on payloads written before
    /// inclusive accounting was introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<SessionAccounting>,
    /// Compatibility alias for `accounting.direct.estimated_cost_usd_nanos`.
    /// New consumers should choose direct or inclusive accounting explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<RunOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSnapshot {
    pub id: RunId,
    pub session_id: SessionId,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// Shared system-prompt version and workspace-instruction identity. Absent
    /// on historical runs and runs that failed before prompt preparation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_identity: Option<Box<RunPromptIdentity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// Input-token total (fresh input + cache reads + cache writes) of the
    /// run's final completed model turn. Distinct from `usage`, which sums
    /// every turn for billing. An internal compaction run measures its
    /// pre-compaction request, not the session state after compaction; clients
    /// use `SessionSummary::context_tokens` for the session meter. Absent on
    /// runs persisted before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
}

/// Version of the provider-neutral system prompt prepared for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PromptVersion(NonZeroU16);

impl PromptVersion {
    #[must_use]
    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// SHA-256 identity of ordered workspace-instruction paths and bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstructionHash([u8; 32]);

impl InstructionHash {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for InstructionHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for InstructionHash {
    type Err = InstructionHashError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(InstructionHashError);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_nibble(pair[0]).ok_or(InstructionHashError)?;
            let low = hex_nibble(pair[1]).ok_or(InstructionHashError)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for InstructionHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for InstructionHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("instruction hash must be exactly 64 lowercase hexadecimal characters")]
pub struct InstructionHashError;

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Validated SHA-256 identity for a prompt, tool declaration set, or selected
/// guidance document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ContentHash {
    type Err = ContentHashError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ContentHashError);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_nibble(pair[0]).ok_or(ContentHashError)?;
            let low = hex_nibble(pair[1]).ok_or(ContentHashError)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for ContentHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("content hash must be exactly 64 lowercase hexadecimal characters")]
pub struct ContentHashError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuidanceKind {
    Command,
    Skill,
}

/// Provenance for one explicitly selected command or skill document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuidanceIdentity {
    pub kind: GuidanceKind,
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub content_hash: ContentHash,
}

/// All-or-none identity of the system prefix prepared for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPromptIdentity {
    pub version: PromptVersion,
    pub instruction_hash: InstructionHash,
    /// Absent only on rows written before protocol version 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_hash: Option<ContentHash>,
    /// Absent only on rows written before protocol version 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_guidance: Option<Box<GuidanceIdentity>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSnapshot {
    pub id: MessageId,
    pub session_id: SessionId,
    pub run_id: RunId,
    /// The model turn this message belongs to within its run. User messages
    /// (and assistant messages persisted before per-turn messages existed)
    /// carry 0; assistant turns are 1-based, matching tool-call ordinals.
    #[serde(default)]
    pub turn_ordinal: u16,
    pub role: MessageRole,
    pub state: MessageState,
    pub output: String,
    pub refusal: String,
    pub created_at_ms: u64,
}

/// A UI-facing rendering payload carried alongside a tool call's result.
/// Display-only: model context assembly never includes it, so it can hold
/// richer content than the bounded result string the model sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallDisplay {
    /// A bounded unified-diff-style rendering of a completed
    /// `edit_file`/`write_file` call.
    Diff { path: String, diff: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallSnapshot {
    pub id: ToolCallId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_ordinal: u16,
    pub call_ordinal: u16,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: String,
    pub state: ToolCallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolCallDisplay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub summary: SessionSummary,
    pub messages: Vec<MessageSnapshot>,
    pub runs: Vec<RunSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallSnapshot>,
    #[serde(default)]
    pub has_older_tool_calls: bool,
    pub has_older_messages: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub cursor: EventCursor,
    pub workspace: WorkspaceSummary,
    pub sessions: Vec<SessionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<SessionSnapshot>,
    pub has_older_sessions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_session_id: Option<SessionId>,
    pub session_limit: u16,
    pub message_limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeRequest {
    pub workspace_id: WorkspaceId,
    pub after: EventCursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEventEnvelope {
    pub cursor: EventCursor,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<CommandId>,
    pub occurred_at_ms: u64,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionCreated {
        session: SessionSummary,
    },
    /// A non-run mutation of the session row (today: its model selection).
    /// Carries the full updated summary so clients re-render without a
    /// round trip.
    SessionUpdated {
        session: SessionSummary,
    },
    /// The session and every row it owned were deleted. Earlier events for
    /// the session remain in the workspace log; replaying them and then this
    /// event converges every client on the deleted state.
    SessionDeleted {
        session_id: SessionId,
    },
    PromptQueued {
        session: SessionSummary,
        message: MessageSnapshot,
        run: RunSnapshot,
        queue_position: u16,
    },
    RunStarted {
        session: SessionSummary,
        run_id: RunId,
    },
    /// Replaceable liveness information for an active run. This describes
    /// harness/provider state, not assistant transcript content.
    RunActivityChanged {
        run_id: RunId,
        activity: RunActivity,
    },
    /// A displayable reasoning block began. This is not assistant transcript
    /// content and must not be fed back into model context implicitly.
    ReasoningStarted {
        run_id: RunId,
        kind: ReasoningKind,
    },
    /// A bounded chunk of provider-exposed reasoning text.
    ReasoningDelta {
        run_id: RunId,
        kind: ReasoningKind,
        text: String,
    },
    ReasoningCompleted {
        run_id: RunId,
        kind: ReasoningKind,
    },
    AssistantMessageStarted {
        message: MessageSnapshot,
    },
    TextAppended {
        message_id: MessageId,
        channel: TextChannel,
        text: String,
    },
    /// One provider inference committed durably. This is the authoritative
    /// per-turn audit seam used by trajectory exporters: model selection,
    /// usage, and cost stay attributable even when a turn contains only tool
    /// calls and therefore has no assistant message row.
    ModelTurnCompleted {
        run_id: RunId,
        turn_ordinal: u16,
        model: ModelSelection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimated_cost_usd_nanos: Option<u64>,
    },
    ToolCallRequested {
        tool_call: ToolCallSnapshot,
    },
    ToolApprovalRequested {
        tool_call: ToolCallSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<ShellCommandPreview>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit: Option<EditPreview>,
    },
    ToolApprovalResolved {
        tool_call: ToolCallSnapshot,
        resolution: ApprovalResolution,
    },
    /// The follow-through of an approve-for-workspace decision: the attempt
    /// to persist the grant into the workspace configuration finished.
    /// Published after `tool_approval_resolved`, from outside the approval's
    /// transaction, so a failed write can never fail the approval. A
    /// `failed` outcome is informational only — the session grant stands.
    WorkspaceGrantPromoted {
        grant: ApprovalGrant,
        outcome: WorkspaceGrantOutcome,
    },
    ToolCallStarted {
        tool_call: ToolCallSnapshot,
    },
    /// A chunk of live tool output (streamed shell output), batched like text
    /// deltas. Chunks are display-only: the authoritative bounded result
    /// arrives on the `ToolCallFinished` snapshot.
    ToolCallOutputDelta {
        tool_call_id: ToolCallId,
        chunk: String,
    },
    ToolCallFinished {
        tool_call: ToolCallSnapshot,
    },
    CancellationRequested {
        session: SessionSummary,
        run_id: RunId,
    },
    /// A compaction committed: the session's context assembly now starts from
    /// a fresh summary. Carries the refreshed summary (the compaction run has
    /// finished by the time this is published), a bounded excerpt of the
    /// summary text, and the assembled context size before and after.
    SessionCompacted {
        session: SessionSummary,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        before_bytes: u64,
        after_bytes: u64,
    },
    /// A measured model turn committed mid-run: the run's per-turn context
    /// audit value moved. `context_tokens` is the completed turn's input-token
    /// total (fresh input + cache reads + cache writes). This does not update
    /// the session meter; clients use `SessionContextUpdated` for that.
    RunContextUpdated {
        run_id: RunId,
        context_tokens: u64,
    },
    /// A prompt turn committed against the session's currently selected
    /// model. `None` explicitly makes occupancy unknown when the provider did
    /// not report usage. Deliberately small: no snapshots.
    SessionContextUpdated {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_tokens: Option<u64>,
    },
    RunFinished {
        session: SessionSummary,
        run_id: RunId,
        outcome: RunOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        /// The final completed model turn's input-token total for this run,
        /// as opposed to `usage`, which sums every turn. The accompanying
        /// session summary owns the current session-meter value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_tokens: Option<u64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id<T>(byte: u8) -> T
    where
        T: FromIdBytes,
    {
        T::from_id_bytes([byte; 16])
    }

    trait FromIdBytes {
        fn from_id_bytes(bytes: [u8; 16]) -> Self;
    }

    macro_rules! from_id_bytes {
        ($($kind:ty),+ $(,)?) => {$(
            impl FromIdBytes for $kind {
                fn from_id_bytes(bytes: [u8; 16]) -> Self {
                    Self::from_bytes(bytes)
                }
            }
        )+};
    }

    from_id_bytes!(
        StoreId,
        WorkspaceId,
        SessionId,
        RunId,
        MessageId,
        ToolCallId,
        CommandId
    );

    #[test]
    fn session_events_have_a_stable_tagged_wire_shape() {
        let workspace_id = id::<WorkspaceId>(2);
        let envelope = SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(1),
                workspace_id,
                sequence: 7,
            },
            session_id: id(3),
            run_id: Some(id(4)),
            caused_by: Some(id(5)),
            occurred_at_ms: 11,
            event: SessionEvent::TextAppended {
                message_id: id(6),
                channel: TextChannel::Output,
                text: "hello".to_owned(),
            },
        };

        let encoded = serde_json::to_value(&envelope).unwrap();

        assert_eq!(encoded["cursor"]["sequence"], 7);
        assert_eq!(encoded["event"]["type"], "text_appended");
        assert_eq!(encoded["event"]["channel"], "output");
        assert_eq!(
            serde_json::from_value::<SessionEventEnvelope>(encoded).unwrap(),
            envelope
        );
        assert_eq!(envelope.cursor.to_string().parse(), Ok(envelope.cursor));
    }

    #[test]
    fn model_turn_event_preserves_per_turn_audit_fields() {
        let event = SessionEvent::ModelTurnCompleted {
            run_id: id(1),
            turn_ordinal: 2,
            model: ModelSelection {
                model: Some("provider/model".to_owned()),
                max_output_tokens: Some(4096),
                organization: Some("org".to_owned()),
            },
            usage: Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 3,
                cache_write_input_tokens: 1,
                output_tokens: 4,
            }),
            estimated_cost_usd_nanos: Some(99),
        };

        let encoded = serde_json::to_value(&event).unwrap();

        assert_eq!(encoded["type"], "model_turn_completed");
        assert_eq!(encoded["turn_ordinal"], 2);
        assert_eq!(encoded["model"]["organization"], "org");
        assert_eq!(encoded["usage"]["cache_read_input_tokens"], 3);
        assert_eq!(encoded["estimated_cost_usd_nanos"], 99);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn tool_call_events_have_a_stable_tagged_wire_shape() {
        let event = SessionEvent::ToolCallFinished {
            tool_call: ToolCallSnapshot {
                id: id(7),
                session_id: id(3),
                run_id: id(4),
                turn_ordinal: 1,
                call_ordinal: 2,
                provider_call_id: "call_2".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"README.md"}"#.to_owned(),
                state: ToolCallState::Completed,
                result: Some("QQ".to_owned()),
                is_error: false,
                display: None,
            },
        };

        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "tool_call_finished");
        assert_eq!(encoded["tool_call"]["state"], "completed");
        assert_eq!(encoded["tool_call"]["name"], "read_file");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn tool_call_display_payloads_round_trip_and_legacy_payloads_decode_to_none() {
        let tool_call = ToolCallSnapshot {
            id: id(7),
            session_id: id(3),
            run_id: id(4),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_1".to_owned(),
            name: "edit_file".to_owned(),
            arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
            state: ToolCallState::Completed,
            result: Some("Edited src/lib.rs: replaced 1 occurrence(s).".to_owned()),
            is_error: false,
            display: Some(ToolCallDisplay::Diff {
                path: "src/lib.rs".to_owned(),
                diff: "- old\n+ new\n".to_owned(),
            }),
        };

        let encoded = serde_json::to_value(&tool_call).unwrap();
        assert_eq!(encoded["display"]["type"], "diff");
        assert_eq!(encoded["display"]["diff"], "- old\n+ new\n");
        assert_eq!(
            serde_json::from_value::<ToolCallSnapshot>(encoded).unwrap(),
            tool_call
        );

        // Calls persisted before the protocol carried a display payload must
        // still decode; legacy snapshots default to no payload.
        let mut legacy = serde_json::to_value(&tool_call).unwrap();
        legacy.as_object_mut().unwrap().remove("display");
        let decoded = serde_json::from_value::<ToolCallSnapshot>(legacy).unwrap();
        assert_eq!(decoded.display, None);
        assert_eq!(decoded.result, tool_call.result);

        // Snapshots without a payload keep their previous wire shape.
        let bare = ToolCallSnapshot {
            display: None,
            ..tool_call
        };
        let encoded = serde_json::to_value(&bare).unwrap();
        assert!(encoded.get("display").is_none());
        assert_eq!(
            serde_json::from_value::<ToolCallSnapshot>(encoded).unwrap(),
            bare
        );
    }

    #[test]
    fn tool_output_deltas_have_a_stable_tagged_wire_shape() {
        let event = SessionEvent::ToolCallOutputDelta {
            tool_call_id: id(7),
            chunk: "Compiling qq-core v0.1.0\n".to_owned(),
        };

        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "tool_call_output_delta");
        assert_eq!(encoded["chunk"], "Compiling qq-core v0.1.0\n");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn approval_events_and_commands_round_trip_with_stable_tags() {
        let tool_call = ToolCallSnapshot {
            id: id(7),
            session_id: id(3),
            run_id: id(4),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_1".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"cargo test"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        };
        let requested = SessionEvent::ToolApprovalRequested {
            tool_call: tool_call.clone(),
            shell: Some(ShellCommandPreview {
                command: "cargo test".to_owned(),
                cwd: Some("crates/qq-core".to_owned()),
            }),
            edit: None,
        };
        let encoded = serde_json::to_value(&requested).unwrap();
        assert_eq!(encoded["type"], "tool_approval_requested");
        assert_eq!(encoded["tool_call"]["state"], "awaiting_approval");
        assert_eq!(encoded["shell"]["command"], "cargo test");
        assert!(encoded.get("edit").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            requested
        );

        let edit_requested = SessionEvent::ToolApprovalRequested {
            tool_call: tool_call.clone(),
            shell: None,
            edit: Some(EditPreview {
                path: "src/lib.rs".to_owned(),
                diff: "- old\n+ new".to_owned(),
            }),
        };
        let encoded = serde_json::to_value(&edit_requested).unwrap();
        assert_eq!(encoded["edit"]["path"], "src/lib.rs");
        assert_eq!(encoded["edit"]["diff"], "- old\n+ new");
        assert!(encoded.get("shell").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            edit_requested
        );

        let resolved = SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        };
        let encoded = serde_json::to_value(&resolved).unwrap();
        assert_eq!(encoded["type"], "tool_approval_resolved");
        assert_eq!(encoded["resolution"], "denied_timeout");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            resolved
        );

        let command = SessionCommand::RespondToolApproval {
            run_id: id(4),
            tool_call_id: id(7),
            decision: ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "cargo test".to_owned(),
                },
            },
        };
        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(encoded["type"], "respond_tool_approval");
        assert_eq!(encoded["decision"]["type"], "approve_for_session");
        assert_eq!(encoded["decision"]["grant"]["type"], "shell_prefix");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            command
        );

        let outcome = CommandOutcome::ToolApprovalResolved {
            tool_call_id: id(7),
            resolution: ApprovalResolution::ApprovedOnce,
        };
        let encoded = serde_json::to_value(&outcome).unwrap();
        assert_eq!(encoded["type"], "tool_approval_resolved");
        assert_eq!(encoded["resolution"], "approved_once");
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            outcome
        );
    }

    #[test]
    fn workspace_grant_decisions_and_promotions_round_trip_on_the_wire() {
        let command = SessionCommand::RespondToolApproval {
            run_id: id(4),
            tool_call_id: id(7),
            decision: ApprovalDecision::ApproveForWorkspace {
                grant: ApprovalGrant::Tool {
                    name: "mcp__github__create_issue".to_owned(),
                },
            },
        };
        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(encoded["decision"]["type"], "approve_for_workspace");
        assert_eq!(encoded["decision"]["grant"]["type"], "tool");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            command
        );

        let resolution = serde_json::to_value(ApprovalResolution::ApprovedForWorkspace).unwrap();
        assert_eq!(resolution, "approved_for_workspace");

        for (outcome, tag, field, value) in [
            (
                WorkspaceGrantOutcome::Written {
                    path: "/repo/.qq/config.ron".to_owned(),
                },
                "written",
                "path",
                "/repo/.qq/config.ron",
            ),
            (
                WorkspaceGrantOutcome::AlreadyPresent {
                    path: "/repo/.qq/config.ron".to_owned(),
                },
                "already_present",
                "path",
                "/repo/.qq/config.ron",
            ),
            (
                WorkspaceGrantOutcome::Failed {
                    message: "denied by managed policy".to_owned(),
                },
                "failed",
                "message",
                "denied by managed policy",
            ),
        ] {
            let event = SessionEvent::WorkspaceGrantPromoted {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "cargo test".to_owned(),
                },
                outcome,
            };
            let encoded = serde_json::to_value(&event).unwrap();
            assert_eq!(encoded["type"], "workspace_grant_promoted");
            assert_eq!(encoded["grant"]["type"], "shell_prefix");
            assert_eq!(encoded["outcome"]["type"], tag);
            assert_eq!(encoded["outcome"][field], value);
            assert_eq!(
                serde_json::from_value::<SessionEvent>(encoded).unwrap(),
                event
            );
        }

        // Version-3 decision payloads still decode unchanged.
        for (legacy, expected) in [
            (
                serde_json::json!({ "type": "approve_once" }),
                ApprovalDecision::ApproveOnce,
            ),
            (
                serde_json::json!({
                    "type": "approve_for_session",
                    "grant": { "type": "shell_prefix", "prefix": "git status" },
                }),
                ApprovalDecision::ApproveForSession {
                    grant: ApprovalGrant::ShellPrefix {
                        prefix: "git status".to_owned(),
                    },
                },
            ),
            (
                serde_json::json!({ "type": "deny" }),
                ApprovalDecision::Deny,
            ),
        ] {
            assert_eq!(
                serde_json::from_value::<ApprovalDecision>(legacy).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn message_snapshots_round_trip_turn_ordinal_and_default_legacy_payloads_to_zero() {
        let message = MessageSnapshot {
            id: id(6),
            session_id: id(3),
            run_id: id(4),
            turn_ordinal: 2,
            role: MessageRole::Assistant,
            state: MessageState::Streaming,
            output: "checking".to_owned(),
            refusal: String::new(),
            created_at_ms: 11,
        };
        let event = SessionEvent::AssistantMessageStarted {
            message: message.clone(),
        };

        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "assistant_message_started");
        assert_eq!(encoded["message"]["turn_ordinal"], 2);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );

        // Events persisted before the protocol carried turn_ordinal must
        // still decode; legacy messages default to turn 0.
        let mut legacy = serde_json::to_value(&message).unwrap();
        legacy.as_object_mut().unwrap().remove("turn_ordinal");
        let decoded = serde_json::from_value::<MessageSnapshot>(legacy).unwrap();
        assert_eq!(decoded.turn_ordinal, 0);
        assert_eq!(decoded.output, "checking");
    }

    #[test]
    fn create_session_without_an_approval_mode_defaults_to_auto() {
        let encoded = serde_json::json!({
            "type": "create_session",
            "workspace_id": id::<WorkspaceId>(2).to_string(),
            "model": { "model": "test/model" },
        });
        let command = serde_json::from_value::<SessionCommand>(encoded).unwrap();
        assert!(matches!(
            command,
            SessionCommand::CreateSession {
                approval_mode: ApprovalMode::Auto,
                ..
            }
        ));
    }

    #[test]
    fn session_management_commands_round_trip_with_stable_tags() {
        let session_id = id::<SessionId>(3);
        let workspace_id = id::<WorkspaceId>(2);

        let set_model = SessionCommand::SetSessionModel {
            session_id,
            model: ModelSelection {
                model: Some("test/model".to_owned()),
                max_output_tokens: Some(256),
                organization: None,
            },
        };
        let encoded = serde_json::to_value(&set_model).unwrap();
        assert_eq!(encoded["type"], "set_session_model");
        assert_eq!(encoded["model"]["model"], "test/model");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            set_model
        );

        let delete = SessionCommand::DeleteSession { session_id };
        let encoded = serde_json::to_value(&delete).unwrap();
        assert_eq!(encoded["type"], "delete_session");
        assert_eq!(encoded["session_id"], session_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            delete
        );

        let prune = SessionCommand::PruneSessions { workspace_id };
        let encoded = serde_json::to_value(&prune).unwrap();
        assert_eq!(encoded["type"], "prune_sessions");
        assert_eq!(encoded["workspace_id"], workspace_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            prune
        );

        let model_set = CommandOutcome::SessionModelSet {
            session_id,
            model: ModelSelection {
                model: Some("test/model".to_owned()),
                max_output_tokens: None,
                organization: None,
            },
        };
        let encoded = serde_json::to_value(&model_set).unwrap();
        assert_eq!(encoded["type"], "session_model_set");
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            model_set
        );

        let deleted = CommandOutcome::SessionDeleted { session_id };
        let encoded = serde_json::to_value(&deleted).unwrap();
        assert_eq!(encoded["type"], "session_deleted");
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            deleted
        );

        let pruned = CommandOutcome::SessionsPruned {
            workspace_id,
            deleted: 3,
        };
        let encoded = serde_json::to_value(&pruned).unwrap();
        assert_eq!(encoded["type"], "sessions_pruned");
        assert_eq!(encoded["deleted"], 3);
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            pruned
        );
    }

    #[test]
    fn compaction_commands_outcomes_and_events_round_trip_with_stable_tags() {
        let session_id = id::<SessionId>(3);
        let run_id = id::<RunId>(4);

        let command = SessionCommand::CompactSession { session_id };
        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(encoded["type"], "compact_session");
        assert_eq!(encoded["session_id"], session_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            command
        );

        let outcome = CommandOutcome::CompactionQueued { session_id, run_id };
        let encoded = serde_json::to_value(&outcome).unwrap();
        assert_eq!(encoded["type"], "compaction_queued");
        assert_eq!(encoded["run_id"], run_id.to_string());
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            outcome
        );

        let event = SessionEvent::SessionCompacted {
            session: SessionSummary {
                id: session_id,
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                context_tokens: None,
                accounting: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 11,
                last_outcome: Some(RunOutcome::Completed),
            },
            summary: Some("intent: ship compaction".to_owned()),
            before_bytes: 3_200_000,
            after_bytes: 240_000,
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "session_compacted");
        assert_eq!(encoded["before_bytes"], 3_200_000);
        assert_eq!(encoded["after_bytes"], 240_000);
        assert_eq!(encoded["summary"], "intent: ship compaction");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );

        // The summary excerpt is optional and absent from the wire when the
        // event omits it.
        let bare = SessionEvent::SessionCompacted {
            session: match &event {
                SessionEvent::SessionCompacted { session, .. } => session.clone(),
                _ => unreachable!(),
            },
            summary: None,
            before_bytes: 1,
            after_bytes: 1,
        };
        let encoded = serde_json::to_value(&bare).unwrap();
        assert!(encoded.get("summary").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            bare
        );
    }

    #[test]
    fn session_update_and_deletion_events_round_trip_with_stable_tags() {
        let session_id = id::<SessionId>(3);
        let updated = SessionEvent::SessionUpdated {
            session: SessionSummary {
                id: session_id,
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model-b".to_owned()),
                context_tokens: Some(12_500),
                accounting: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 11,
                last_outcome: None,
            },
        };
        let encoded = serde_json::to_value(&updated).unwrap();
        assert_eq!(encoded["type"], "session_updated");
        assert_eq!(encoded["session"]["model"], "test/model-b");
        assert_eq!(encoded["session"]["context_tokens"], 12_500);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded.clone()).unwrap(),
            updated
        );
        let mut legacy = encoded;
        legacy["session"]
            .as_object_mut()
            .unwrap()
            .remove("context_tokens");
        let SessionEvent::SessionUpdated { session } =
            serde_json::from_value::<SessionEvent>(legacy).unwrap()
        else {
            panic!("expected session update")
        };
        assert_eq!(session.context_tokens, None);

        let deleted = SessionEvent::SessionDeleted { session_id };
        let encoded = serde_json::to_value(&deleted).unwrap();
        assert_eq!(encoded["type"], "session_deleted");
        assert_eq!(encoded["session_id"], session_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            deleted
        );
    }

    #[test]
    fn run_finished_without_usage_retains_its_previous_wire_shape() {
        let workspace_id = id::<WorkspaceId>(2);
        let session_id = id::<SessionId>(3);
        let event = SessionEvent::RunFinished {
            session: SessionSummary {
                id: session_id,
                workspace_id,
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                context_tokens: None,
                accounting: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 11,
                last_outcome: Some(RunOutcome::Completed),
            },
            run_id: id(4),
            outcome: RunOutcome::Completed,
            usage: None,
            context_tokens: None,
        };

        let encoded = serde_json::to_value(&event).unwrap();

        assert!(encoded.get("usage").is_none());
        assert!(encoded.get("context_tokens").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn session_summary_accounting_is_additive_and_legacy_cost_stays_direct() {
        let legacy = serde_json::json!({
            "id": id::<SessionId>(3),
            "workspace_id": id::<WorkspaceId>(2),
            "title": "legacy",
            "status": "idle",
            "queued_prompts": 0,
            "estimated_cost_usd_nanos": 7,
            "updated_at_ms": 11
        });
        let decoded: SessionSummary = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.accounting, None);
        assert_eq!(decoded.estimated_cost_usd_nanos, Some(7));

        let current = SessionSummary {
            accounting: Some(SessionAccounting {
                direct: AccountingTotal {
                    usage: Some(TokenUsage {
                        input_tokens: 1,
                        cache_read_input_tokens: 2,
                        cache_write_input_tokens: 3,
                        output_tokens: 4,
                    }),
                    estimated_cost_usd_nanos: Some(7),
                },
                inclusive: AccountingTotal {
                    usage: Some(TokenUsage {
                        input_tokens: 11,
                        cache_read_input_tokens: 12,
                        cache_write_input_tokens: 13,
                        output_tokens: 14,
                    }),
                    estimated_cost_usd_nanos: Some(17),
                },
            }),
            ..decoded
        };
        let encoded = serde_json::to_value(&current).unwrap();
        assert_eq!(encoded["estimated_cost_usd_nanos"], 7);
        assert_eq!(
            encoded["accounting"]["inclusive"]["estimated_cost_usd_nanos"],
            17
        );
        assert_eq!(
            serde_json::from_value::<SessionSummary>(encoded).unwrap(),
            current
        );
    }

    #[test]
    fn context_events_and_fields_round_trip_and_legacy_payloads_decode_to_none() {
        let updated = SessionEvent::RunContextUpdated {
            run_id: id(4),
            context_tokens: 12_500,
        };
        let encoded = serde_json::to_value(&updated).unwrap();
        assert_eq!(encoded["type"], "run_context_updated");
        assert_eq!(encoded["context_tokens"], 12_500);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            updated
        );

        let session_updated = SessionEvent::SessionContextUpdated {
            run_id: id(4),
            context_tokens: Some(12_500),
        };
        let encoded = serde_json::to_value(&session_updated).unwrap();
        assert_eq!(encoded["type"], "session_context_updated");
        assert_eq!(encoded["context_tokens"], 12_500);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            session_updated
        );

        let unknown = SessionEvent::SessionContextUpdated {
            run_id: id(4),
            context_tokens: None,
        };
        let encoded = serde_json::to_value(&unknown).unwrap();
        assert!(encoded.get("context_tokens").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            unknown
        );

        let finished = SessionEvent::RunFinished {
            session: SessionSummary {
                id: id(3),
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                context_tokens: Some(16),
                accounting: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 11,
                last_outcome: Some(RunOutcome::Completed),
            },
            run_id: id(4),
            outcome: RunOutcome::Completed,
            usage: Some(TokenUsage {
                input_tokens: 30,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 2,
                output_tokens: 9,
            }),
            context_tokens: Some(16),
        };
        let encoded = serde_json::to_value(&finished).unwrap();
        assert_eq!(encoded["type"], "run_finished");
        assert_eq!(encoded["context_tokens"], 16);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            finished
        );

        let run = RunSnapshot {
            id: id(4),
            session_id: id(3),
            status: RunStatus::Completed,
            outcome: Some(RunOutcome::Completed),
            prompt_identity: Some(Box::new(RunPromptIdentity {
                version: PromptVersion::new(7).unwrap(),
                instruction_hash: "a".repeat(64).parse().unwrap(),
                system_prompt_hash: Some("b".repeat(64).parse().unwrap()),
                tool_schema_hash: Some("c".repeat(64).parse().unwrap()),
                selected_guidance: Some(Box::new(GuidanceIdentity {
                    kind: GuidanceKind::Skill,
                    name: "review".to_owned(),
                    source: ".qq/skills/review/SKILL.md".to_owned(),
                    version: None,
                    content_hash: "d".repeat(64).parse().unwrap(),
                })),
            })),
            usage: Some(TokenUsage {
                input_tokens: 30,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 2,
                output_tokens: 9,
            }),
            context_tokens: Some(16),
            estimated_cost_usd_nanos: Some(1),
        };
        let encoded = serde_json::to_value(&run).unwrap();
        assert_eq!(encoded["context_tokens"], 16);
        assert_eq!(encoded["prompt_identity"]["version"], 7);
        assert_eq!(
            encoded["prompt_identity"]["instruction_hash"],
            "a".repeat(64)
        );
        assert_eq!(
            encoded["prompt_identity"]["system_prompt_hash"],
            "b".repeat(64)
        );
        assert_eq!(
            encoded["prompt_identity"]["selected_guidance"]["kind"],
            "skill"
        );
        assert_eq!(serde_json::from_value::<RunSnapshot>(encoded).unwrap(), run);

        let legacy_identity: RunPromptIdentity = serde_json::from_value(serde_json::json!({
            "version": 6,
            "instruction_hash": "e".repeat(64),
        }))
        .unwrap();
        assert_eq!(legacy_identity.version, PromptVersion::new(6).unwrap());
        assert_eq!(legacy_identity.system_prompt_hash, None);
        assert_eq!(legacy_identity.tool_schema_hash, None);
        assert_eq!(legacy_identity.selected_guidance, None);

        // Runs persisted before the protocol carried context tokens must
        // still decode; legacy snapshots default to no value.
        let mut legacy = serde_json::to_value(&run).unwrap();
        legacy.as_object_mut().unwrap().remove("context_tokens");
        legacy.as_object_mut().unwrap().remove("prompt_identity");
        let decoded = serde_json::from_value::<RunSnapshot>(legacy).unwrap();
        assert_eq!(decoded.context_tokens, None);
        assert_eq!(decoded.prompt_identity, None);
        assert_eq!(decoded.usage, run.usage);

        // Snapshots without a value keep their previous wire shape.
        let bare = RunSnapshot {
            context_tokens: None,
            prompt_identity: None,
            ..run.clone()
        };
        let encoded = serde_json::to_value(&bare).unwrap();
        assert!(encoded.get("context_tokens").is_none());
        assert!(encoded.get("prompt_identity").is_none());
        assert_eq!(
            serde_json::from_value::<RunSnapshot>(encoded).unwrap(),
            bare
        );

        for invalid in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
            assert!(invalid.parse::<InstructionHash>().is_err());
        }
        let mut invalid = serde_json::to_value(&run).unwrap();
        invalid["prompt_identity"]["version"] = serde_json::json!(0);
        assert!(serde_json::from_value::<RunSnapshot>(invalid).is_err());
        let mut invalid = serde_json::to_value(&run).unwrap();
        invalid["prompt_identity"]["instruction_hash"] = serde_json::json!("A".repeat(64));
        assert!(serde_json::from_value::<RunSnapshot>(invalid).is_err());
    }
}
