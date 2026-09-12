use std::{fmt, num::NonZeroU16, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{
    AgentProfileId, CommandId, Correlation, InputPart, MessageId, ReasoningKind, RunFailureKind,
    RunId, RunPlanIdentity, SessionId, StoreId, ToolCallId, WorkspaceId,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
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
    /// Every generated token, including hidden reasoning tokens when the
    /// provider reports them. Billing and budgets use this total.
    pub output_tokens: u64,
    /// The reasoning-token portion of `output_tokens` when the provider
    /// reports it separately; `None` when it does not. Observational: it is
    /// never added to `output_tokens` again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
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

/// Version of the secret-free resolved-model projection persisted for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResolvedModelVersion(NonZeroU16);

impl ResolvedModelVersion {
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

/// Version of the opaque provider request-shape identity carried by a
/// resolved model. The identity is deliberately separate from the resolved
/// model schema so its digest domain can evolve without reinterpreting old
/// rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderRequestShapeVersion(NonZeroU16);

impl ProviderRequestShapeVersion {
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

/// Secret-free, opaque identity of the provider adapter and immutable
/// deployment configuration that determine its wire request shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequestShapeIdentity {
    pub version: ProviderRequestShapeVersion,
    pub digest: ContentHash,
}

/// Whether QQ's selected provider codec can express one generation control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Native,
    Unsupported,
}

/// Generation controls implemented by the selected provider codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationCapabilities {
    pub reasoning_effort: CapabilitySupport,
}

/// Prompt-cache controls and accounting fields implemented by the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCacheCapabilities {
    pub control: CapabilitySupport,
    pub cache_read_usage: bool,
    pub cache_write_usage: bool,
}

/// Immutable, secret-free account of the exact model execution admitted for
/// one run. `route` is QQ's effective `provider/model` selection while
/// `provider_model` is the identifier sent to the provider codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModel {
    pub version: ResolvedModelVersion,
    /// Absent for historical rows and deployments whose exact shape cannot be
    /// represented without deriving an identity from secret-bearing config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_shape: Option<ProviderRequestShapeIdentity>,
    pub route: String,
    pub provider_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Non-secret named credential profile when the configured provider has
    /// one. Literal credentials and credential values are never represented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_profile: Option<String>,
    pub max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    pub output_token_control: CapabilitySupport,
    pub generation: GenerationCapabilities,
    pub prompt_cache: PromptCacheCapabilities,
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
    /// Every mutating, shell, and external call is held and adjudicated by
    /// the configured reviewer model regardless of grants; a reviewer denial
    /// is final and an escalation reaches the human. Only spawned write
    /// children run here; a client cannot select it for a root session.
    Supervised,
    Ask,
    /// Default: edits and safe shell run without prompting; only dangerous
    /// shell commands (deletion, privilege escalation, force-push, piped
    /// installers) require approval.
    #[default]
    Auto,
    /// Zero restrictions: every tool call executes without prompting.
    Full,
}

/// The authority a parent grants a spawned child: `read` yields a `ReadOnly`
/// child; `write` yields a `Supervised` one whose every held action is
/// adjudicated before it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildAuthority {
    #[default]
    Read,
    Write,
}

impl ChildAuthority {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
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
    /// Denied by the configured approval reviewer model. Live for `supervised`
    /// sessions (write children), where a reviewer denial is final; for root
    /// `auto` sessions the reviewer still escalates to a human instead.
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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
        /// Configured agent profile the session runs under. `default` when
        /// omitted. Rejected when the workspace configuration does not declare
        /// the profile.
        #[serde(default, skip_serializing_if = "AgentProfileId::is_default")]
        profile: AgentProfileId,
        #[serde(default, skip_serializing_if = "Correlation::is_empty")]
        correlation: Correlation,
    },
    SubmitPrompt {
        session_id: SessionId,
        /// Bounded typed input; see [`crate::validate_input`] for the limits
        /// the server enforces before durable admission.
        input: Vec<InputPart>,
        /// Core-owned budgets for the run this prompt creates. Every accepted
        /// limit settles as a typed `RunOutcome::BudgetExhausted`; an absent
        /// or empty value imposes no limit beyond the runtime's own bounds.
        #[serde(default, skip_serializing_if = "RunLimits::is_empty")]
        limits: RunLimits,
        #[serde(default, skip_serializing_if = "Correlation::is_empty")]
        correlation: Correlation,
    },
    /// Adds user input to a run that is already executing. The input is
    /// injected as a user message at the next safe model/tool boundary, so a
    /// provider request already in flight is never rewritten. With
    /// `interrupt`, the in-flight provider stream or tool is aborted first so
    /// the boundary arrives now; partial assistant text is kept and unstarted
    /// tool calls settle as interrupted. Distinct from `SubmitPrompt`, which
    /// queues a new run, and `CancelRun`, which ends the run.
    SteerRun {
        run_id: RunId,
        input: Vec<InputPart>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupt: bool,
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
    /// Repoints the session's agent profile. Takes effect when the next run
    /// is claimed. Rejected when the workspace configuration does not declare
    /// the profile.
    SetSessionProfile {
        session_id: SessionId,
        profile: AgentProfileId,
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
    /// Discards the newest compaction so assembly falls back to the prior
    /// retained one (or to the verbatim transcript when none remains). Valid
    /// only while the session is idle; refused when there is nothing to roll
    /// back. The client transcript is untouched.
    RollbackCompaction {
        session_id: SessionId,
    },
}

impl SessionCommand {
    #[must_use]
    pub const fn kind(&self) -> SessionCommandKind {
        match self {
            Self::ResolveWorkspace { .. } => SessionCommandKind::ResolveWorkspace,
            Self::CreateSession { .. } => SessionCommandKind::CreateSession,
            Self::SubmitPrompt { .. } => SessionCommandKind::SubmitPrompt,
            Self::SteerRun { .. } => SessionCommandKind::SteerRun,
            Self::CancelRun { .. } => SessionCommandKind::CancelRun,
            Self::RespondToolApproval { .. } => SessionCommandKind::RespondToolApproval,
            Self::SetApprovalMode { .. } => SessionCommandKind::SetApprovalMode,
            Self::SetSessionModel { .. } => SessionCommandKind::SetSessionModel,
            Self::SetSessionProfile { .. } => SessionCommandKind::SetSessionProfile,
            Self::DeleteSession { .. } => SessionCommandKind::DeleteSession,
            Self::PruneSessions { .. } => SessionCommandKind::PruneSessions,
            Self::CompactSession { .. } => SessionCommandKind::CompactSession,
            Self::RollbackCompaction { .. } => SessionCommandKind::RollbackCompaction,
        }
    }
}

/// The tag vocabulary of [`SessionCommand`], advertised by server capabilities
/// and used by transports to check that a body matches its route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCommandKind {
    ResolveWorkspace,
    CreateSession,
    SubmitPrompt,
    SteerRun,
    CancelRun,
    RespondToolApproval,
    SetApprovalMode,
    SetSessionModel,
    SetSessionProfile,
    DeleteSession,
    PruneSessions,
    CompactSession,
    RollbackCompaction,
}

impl SessionCommandKind {
    /// Every command this protocol revision routes, in declaration order.
    pub const ALL: [Self; 13] = [
        Self::ResolveWorkspace,
        Self::CreateSession,
        Self::SubmitPrompt,
        Self::SteerRun,
        Self::CancelRun,
        Self::RespondToolApproval,
        Self::SetApprovalMode,
        Self::SetSessionModel,
        Self::SetSessionProfile,
        Self::DeleteSession,
        Self::PruneSessions,
        Self::CompactSession,
        Self::RollbackCompaction,
    ];
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
    /// The steering input was recorded and will be applied at the run's next
    /// boundary (or now, when interruption was requested).
    SteeringQueued {
        run_id: RunId,
        message_id: MessageId,
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
    SessionProfileSet {
        session_id: SessionId,
        profile: AgentProfileId,
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
    CompactionRolledBack {
        session_id: SessionId,
        /// Compactions still retained after the rollback.
        remaining: u16,
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
    BudgetExhausted,
}

/// Per-run budgets imposed by the caller and enforced by the core runtime.
/// Each accepted limit yields exactly one typed terminal outcome, so every
/// client observes the same bound the same way. A limit the active model
/// cannot account for is rejected before provider work rather than ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLimits {
    /// Wall-clock budget from run start, spanning provider retries, tool
    /// execution, and sub-agent work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
    /// Provider turns this run may make in total. The last permitted turn is
    /// reserved as a tool-free final status response when work remains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_turns: Option<u32>,
    /// Tool calls the model may request across the whole run. The meter
    /// reserves a tool-free final response before the cap would be crossed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Total input plus output tokens, summed over every provider turn of the
    /// run and its sub-agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// Estimated spend cap in USD nanos. Requires configured pricing; a turn
    /// whose usage the provider omits settles as `cost_unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_nanos: Option<u64>,
    /// Fresh input plus cache-read plus cache-write tokens, summed over every
    /// provider turn of the run and its sub-agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Output tokens summed over every provider turn of the run and its
    /// sub-agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Bytes of tool results fed back to the model across the run, after the
    /// runtime's per-result truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_output_bytes: Option<u64>,
    /// Sub-agents this run may spawn in total. Capped by the runtime's hard
    /// ceiling; a refused spawn is reported to the model as a tool error, not
    /// as a terminal outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u16>,
    /// Sub-agents this run may have executing at once. Capped by the
    /// runtime's hard ceiling; excess spawns wait for a slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_children: Option<u16>,
}

impl RunLimits {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.max_duration_ms.is_none()
            && self.max_model_turns.is_none()
            && self.max_tool_calls.is_none()
            && self.max_total_tokens.is_none()
            && self.max_cost_usd_nanos.is_none()
            && self.max_input_tokens.is_none()
            && self.max_output_tokens.is_none()
            && self.max_tool_output_bytes.is_none()
            && self.max_children.is_none()
            && self.max_concurrent_children.is_none()
    }
}

/// Which budget settled a run and how far it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetExhaustion {
    pub limit: BudgetLimitKind,
    /// Whether the run was granted its reserved final status response after
    /// the work budget ran out. False when even the final turn could not be
    /// afforded, or when the limit tripped mid-turn.
    pub final_response: bool,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLimitKind {
    Duration,
    ModelTurns,
    ToolCalls,
    TotalTokens,
    Cost,
    /// A cost cap was imposed but a provider turn omitted usage, so spend
    /// could no longer be measured. Never labelled a provider failure.
    CostUnknown,
    InputTokens,
    OutputTokens,
    /// A token cap was imposed but a provider turn omitted usage, so the
    /// count could no longer be measured. Never labelled a provider failure.
    TokensUnknown,
    ToolOutputBytes,
}

impl BudgetLimitKind {
    /// Every kind this protocol revision can settle, in declaration order.
    pub const ALL: [Self; 10] = [
        Self::Duration,
        Self::ModelTurns,
        Self::ToolCalls,
        Self::TotalTokens,
        Self::Cost,
        Self::CostUnknown,
        Self::InputTokens,
        Self::OutputTokens,
        Self::TokensUnknown,
        Self::ToolOutputBytes,
    ];

    /// The wire name of this kind, for messages that name a budget family.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::ModelTurns => "model_turns",
            Self::ToolCalls => "tool_calls",
            Self::TotalTokens => "total_tokens",
            Self::Cost => "cost",
            Self::CostUnknown => "cost_unknown",
            Self::InputTokens => "input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::TokensUnknown => "tokens_unknown",
            Self::ToolOutputBytes => "tool_output_bytes",
        }
    }
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
    Failed {
        failure: RunFailure,
    },
    /// A caller-imposed `RunLimits` bound settled the run. Distinct from
    /// `Failed`: the harness, model, and provider all behaved; the caller's
    /// budget ran out.
    BudgetExhausted {
        exhaustion: Box<BudgetExhaustion>,
    },
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

/// The parent run and tool call that spawned a child session. Lets a client
/// render the child inline under the `spawn_agent` call that created it and
/// attribute its work to the parent run without inferring from timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnOrigin {
    pub run_id: RunId,
    /// Absent for children created before the spawning call was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Nesting depth of the spawned session (1 = a root's child). Defaults to
    /// 1 on decode, which is what every session recorded before depth
    /// existed was.
    #[serde(default = "default_spawn_depth")]
    pub depth: u16,
}

const fn default_spawn_depth() -> u16 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    /// Set on child sessions created by a parent run; `None` for roots and
    /// for summaries produced by older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<SpawnOrigin>,
    /// Why the session exists; `task` unless it is an audit child.
    #[serde(default, skip_serializing_if = "SessionPurpose::is_task")]
    pub purpose: SessionPurpose,
    pub title: String,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<RunId>,
    /// Latest liveness state of the active run, so a client that loads or
    /// reconnects mid-run shows the right label without waiting for the next
    /// `RunActivityChanged`. `None` when idle or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<RunActivity>,
    pub queued_prompts: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent profile the session's next run is claimed under. `default` on
    /// rows written before profiles existed.
    #[serde(default, skip_serializing_if = "AgentProfileId::is_default")]
    pub profile: AgentProfileId,
    /// Approval policy the session's tool calls are held against. Changed by
    /// `SetApprovalMode`, which publishes the updated summary. Defaults on
    /// decode because persisted events written before version 15 lack it and
    /// still replay; `auto` was the only value those sessions could have been
    /// created with by the shipped clients.
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    #[serde(default, skip_serializing_if = "Correlation::is_empty")]
    pub correlation: Correlation,
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
    /// Exact resolved model admitted before provider work. Absent on
    /// historical runs and runs that failed before runtime resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<Box<ResolvedModel>>,
    /// Profile, plan digest, and credential epoch the run was admitted with.
    /// Absent while queued, on historical runs, and on runs that failed
    /// before plan compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Box<RunPlanIdentity>>,
    #[serde(default, skip_serializing_if = "Correlation::is_empty")]
    pub correlation: Correlation,
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
    /// Budgets the caller imposed on this run. Absent for historical runs and
    /// runs submitted without limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Box<RunLimits>>,
    /// How the final answer was audited, when the run was audited at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<Box<AuditRecord>>,
}

/// How the root run's final answer was audited before it was presented as
/// complete. Advisory: the audit can send the run back for one revision but
/// never fails it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    pub outcome: AuditOutcome,
    /// The auditor's findings, bounded; empty on `pass` or `unavailable`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
    /// How many revision cycles the audit sent the run through (0 or 1 with
    /// the default `max_revisions`).
    #[serde(default)]
    pub revisions: u16,
    /// The audit child run's usage and cost, charged to the audited run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The auditor verified the answer's claims.
    Pass,
    /// The auditor found problems; the run revised its answer (once) and the
    /// revision stands unaudited.
    Revised,
    /// The auditor could not answer (failed, timed out, unparseable) and the
    /// answer stands as given. Fail-open, recorded rather than hidden.
    Unavailable,
}

/// Why a session exists, when it is not an ordinary user or delegated task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPurpose {
    #[default]
    Task,
    /// A read-only child spawned to audit its parent's final answer.
    Audit,
}

impl SessionPurpose {
    #[must_use]
    pub const fn is_task(&self) -> bool {
        matches!(self, Self::Task)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Audit => "audit",
        }
    }
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

/// How a plan's external tools reach the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    /// Every external schema is in every request.
    Full,
    /// Requests carry an index; the model pins schemas with `select_tools`.
    Progressive,
}

/// How one context source contributed to a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSourceOutcome {
    Fetched,
    FetchedTruncated,
    Cached,
    CachedTruncated,
    TimedOut,
    Unavailable,
    Refused,
    Invalid,
}

/// One context source consulted before a run's first provider request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSourceRecord {
    pub name: String,
    pub version: String,
    pub outcome: ContextSourceOutcome,
    pub items: u32,
    pub bytes: u64,
    /// Hash of the items that entered the prompt, when any did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
    /// Hash of the tool schemas in the run's first request. Absent only on
    /// rows written before protocol version 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_guidance: Option<Box<GuidanceIdentity>>,
    /// Digest of the plan's complete tool catalog (every tool the run could
    /// reach, exposed or not). Absent on rows written before protocol 14.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_digest: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ToolExposure>,
    /// Context sources consulted for this run, in registration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_sources: Vec<ContextSourceRecord>,
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
    pub turn_ordinal: u32,
    pub role: MessageRole,
    pub state: MessageState,
    /// A user message added by `SteerRun` while the run executed, rather than
    /// the prompt that created the run. `queued` until applied at a boundary,
    /// then `complete`; `cancelled` when the run finished first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steering: bool,
    /// The provider cut this assistant message at its output token limit.
    /// The text is a valid prefix; the run either continued in the next turn
    /// or settled as `provider_output_truncated`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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
    pub turn_ordinal: u32,
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
    /// Bodies for every session named in `SnapshotRequest::include_sessions`,
    /// in request order, so a client can keep several transcripts warm from
    /// one round trip. Sessions outside the workspace are omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included: Vec<SessionSnapshot>,
    pub has_older_sessions: bool,
}

/// Upper bound on `SnapshotRequest::include_sessions`; requests above it are
/// rejected rather than truncated.
pub const MAX_INCLUDED_SESSIONS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_session_id: Option<SessionId>,
    /// Additional sessions whose bodies should be returned in
    /// `WorkspaceSnapshot::included`, each bounded by `message_limit`. At
    /// most [`MAX_INCLUDED_SESSIONS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_sessions: Vec<SessionId>,
    pub session_limit: u16,
    pub message_limit: u16,
}

impl SnapshotRequest {
    /// A workspace snapshot with an optional focused body and no extras.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        focused_session_id: Option<SessionId>,
        session_limit: u16,
        message_limit: u16,
    ) -> Self {
        Self {
            workspace_id,
            focused_session_id,
            include_sessions: Vec::new(),
            session_limit,
            message_limit,
        }
    }
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
        run: Box<RunSnapshot>,
        queue_position: u16,
    },
    RunStarted {
        session: SessionSummary,
        run_id: RunId,
        /// Fixed behavioral identity of the run. Absent only on envelopes
        /// written before plan identity was recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<Box<RunPlanIdentity>>,
    },
    /// Steering input was durably recorded against an executing run.
    SteeringQueued {
        run_id: RunId,
        message: MessageSnapshot,
    },
    /// Queued steering entered model context at the given boundary. The
    /// message's state moves to `complete`.
    SteeringApplied {
        run_id: RunId,
        message_id: MessageId,
        /// The model turn whose request first carried the steering.
        turn_ordinal: u32,
    },
    /// The run finished before the steering could be applied. The message's
    /// state moves to `cancelled`.
    SteeringSuperseded {
        run_id: RunId,
        message_id: MessageId,
    },
    /// An interrupting steer aborted the in-flight provider stream or tool
    /// calls of the given turn. Partial assistant text of that turn stands;
    /// tool calls that had not finished settle as interrupted.
    RunInterrupted {
        run_id: RunId,
        turn_ordinal: u32,
    },
    /// The provider stopped turn `turn_ordinal` at its output token limit.
    /// The partial turn is committed (its text stands in the transcript) and
    /// the run continues with turn `turn_ordinal + 1` asking the model to
    /// resume. `continuation` counts continuations so far in this run
    /// (1-based) against `LimitCapabilities::max_output_continuations`.
    RunOutputTruncated {
        run_id: RunId,
        turn_ordinal: u32,
        continuation: u16,
    },
    /// The run's candidate final answer is being audited by a read-only
    /// child before it is presented as complete. `audit_session_id` is that
    /// child; its transcript is the audit.
    RunAuditStarted {
        run_id: RunId,
        audit_session_id: SessionId,
    },
    /// The audit settled and its record is durable on the run. On `revised`
    /// the run continues with the findings; otherwise it completes.
    RunAuditCompleted {
        run_id: RunId,
        audit: AuditRecord,
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
        turn_ordinal: u32,
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
    /// The newest compaction was discarded. `remaining` compactions still
    /// apply; assembly now reads the newest of them, or the full transcript
    /// when none remain. The session meter is unknown until the next prompt.
    SessionCompactionRolledBack {
        session: SessionSummary,
        remaining: u16,
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
                reasoning_tokens: None,
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
    fn resolved_model_v1_decodes_with_unknown_request_shape_and_v2_round_trips_identity() {
        let historical: ResolvedModel = serde_json::from_value(serde_json::json!({
            "version": 1,
            "route": "provider/model",
            "provider_model": "model",
            "max_output_tokens": 4096,
            "output_token_control": "native",
            "generation": { "reasoning_effort": "unsupported" },
            "prompt_cache": {
                "control": "unsupported",
                "cache_read_usage": true,
                "cache_write_usage": false
            }
        }))
        .unwrap();
        assert_eq!(historical.request_shape, None);

        let mut current = historical;
        current.version = ResolvedModelVersion::new(2).unwrap();
        current.request_shape = Some(ProviderRequestShapeIdentity {
            version: ProviderRequestShapeVersion::new(1).unwrap(),
            digest: "a".repeat(64).parse().unwrap(),
        });
        let encoded = serde_json::to_value(&current).unwrap();
        assert_eq!(encoded["version"], 2);
        assert_eq!(encoded["request_shape"]["version"], 1);
        assert_eq!(encoded["request_shape"]["digest"], "a".repeat(64));
        assert_eq!(
            serde_json::from_value::<ResolvedModel>(encoded).unwrap(),
            current
        );

        current.request_shape = None;
        let encoded = serde_json::to_value(&current).unwrap();
        assert_eq!(encoded["version"], 2);
        assert!(encoded.get("request_shape").is_none());
        assert_eq!(
            serde_json::from_value::<ResolvedModel>(encoded).unwrap(),
            current
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
            steering: false,
            truncated: false,
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
                activity: None,
                spawned_by: None,
                purpose: SessionPurpose::Task,
                id: session_id,
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                profile: AgentProfileId::default(),
                approval_mode: ApprovalMode::Auto,
                correlation: Correlation::default(),
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
                activity: None,
                spawned_by: None,
                purpose: SessionPurpose::Task,
                id: session_id,
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model-b".to_owned()),
                profile: AgentProfileId::default(),
                approval_mode: ApprovalMode::Auto,
                correlation: Correlation::default(),
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
                activity: None,
                spawned_by: None,
                purpose: SessionPurpose::Task,
                id: session_id,
                workspace_id,
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                profile: AgentProfileId::default(),
                approval_mode: ApprovalMode::Auto,
                correlation: Correlation::default(),
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
                        reasoning_tokens: None,
                    }),
                    estimated_cost_usd_nanos: Some(7),
                },
                inclusive: AccountingTotal {
                    usage: Some(TokenUsage {
                        input_tokens: 11,
                        cache_read_input_tokens: 12,
                        cache_write_input_tokens: 13,
                        output_tokens: 14,
                        reasoning_tokens: None,
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
    fn spawn_origin_and_activity_are_additive_on_session_summaries() {
        let legacy = serde_json::json!({
            "id": id::<SessionId>(3),
            "workspace_id": id::<WorkspaceId>(2),
            "parent_id": id::<SessionId>(1),
            "title": "child",
            "status": "running",
            "active_run_id": id::<RunId>(9),
            "queued_prompts": 0,
            "updated_at_ms": 11
        });
        let decoded: SessionSummary = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.spawned_by, None);
        assert_eq!(decoded.activity, None);
        assert!(
            !serde_json::to_value(&decoded)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("spawned_by")
        );

        let current = SessionSummary {
            spawned_by: Some(SpawnOrigin {
                run_id: id(8),
                tool_call_id: Some(id(7)),
                depth: 1,
            }),
            activity: Some(RunActivity::GeneratingResponse),
            ..decoded
        };
        let encoded = serde_json::to_value(&current).unwrap();
        assert_eq!(
            encoded["spawned_by"]["run_id"],
            serde_json::json!(id::<RunId>(8))
        );
        assert_eq!(encoded["activity"], "generating_response");
        assert_eq!(
            serde_json::from_value::<SessionSummary>(encoded).unwrap(),
            current
        );
        // A spawn origin without a recorded tool call is still valid.
        let bare = serde_json::json!({ "run_id": id::<RunId>(8) });
        assert_eq!(
            serde_json::from_value::<SpawnOrigin>(bare).unwrap(),
            SpawnOrigin {
                run_id: id(8),
                tool_call_id: None,
                depth: 1,
            }
        );
    }

    #[test]
    fn snapshot_requests_and_responses_stay_additive_for_included_sessions() {
        let legacy_request = serde_json::json!({
            "workspace_id": id::<WorkspaceId>(2),
            "focused_session_id": id::<SessionId>(3),
            "session_limit": 8,
            "message_limit": 16
        });
        let request: SnapshotRequest = serde_json::from_value(legacy_request).unwrap();
        assert!(request.include_sessions.is_empty());
        assert_eq!(request, SnapshotRequest::new(id(2), Some(id(3)), 8, 16));
        let encoded = serde_json::to_value(&request).unwrap();
        assert!(
            !encoded
                .as_object()
                .unwrap()
                .contains_key("include_sessions")
        );

        let with_extras = SnapshotRequest {
            include_sessions: vec![id(4), id(5)],
            ..request
        };
        let encoded = serde_json::to_value(&with_extras).unwrap();
        assert_eq!(encoded["include_sessions"].as_array().unwrap().len(), 2);
        assert_eq!(
            serde_json::from_value::<SnapshotRequest>(encoded).unwrap(),
            with_extras
        );

        let legacy_response = serde_json::json!({
            "cursor": { "store_id": id::<StoreId>(1), "workspace_id": id::<WorkspaceId>(2), "sequence": 5 },
            "workspace": { "id": id::<WorkspaceId>(2), "path": "/w" },
            "sessions": [],
            "has_older_sessions": false
        });
        let snapshot: WorkspaceSnapshot = serde_json::from_value(legacy_response).unwrap();
        assert!(snapshot.included.is_empty());
        assert!(
            !serde_json::to_value(&snapshot)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("included")
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
                activity: None,
                spawned_by: None,
                purpose: SessionPurpose::Task,
                id: id(3),
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                profile: AgentProfileId::default(),
                approval_mode: ApprovalMode::Auto,
                correlation: Correlation::default(),
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
                reasoning_tokens: None,
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
                catalog_digest: Some("e".repeat(64).parse().unwrap()),
                exposure: Some(ToolExposure::Progressive),
                context_sources: vec![ContextSourceRecord {
                    name: "memory".to_owned(),
                    version: "1".to_owned(),
                    outcome: ContextSourceOutcome::Fetched,
                    items: 2,
                    bytes: 512,
                    content_hash: Some("f".repeat(64).parse().unwrap()),
                    message: None,
                }],
            })),
            resolved_model: Some(Box::new(ResolvedModel {
                version: ResolvedModelVersion::new(1).unwrap(),
                request_shape: None,
                route: "provider/model".to_owned(),
                provider_model: "model".to_owned(),
                organization: Some("org".to_owned()),
                credential_profile: Some("work".to_owned()),
                max_output_tokens: 4096,
                context_window: Some(128_000),
                pricing: Some(ModelPricing {
                    input_usd_nanos_per_token: 1,
                    output_usd_nanos_per_token: 2,
                    cache_read_usd_nanos_per_token: Some(3),
                    cache_write_usd_nanos_per_token: None,
                    context_tier: None,
                    provenance: "public catalog".to_owned(),
                }),
                output_token_control: CapabilitySupport::Native,
                generation: GenerationCapabilities {
                    reasoning_effort: CapabilitySupport::Unsupported,
                },
                prompt_cache: PromptCacheCapabilities {
                    control: CapabilitySupport::Unsupported,
                    cache_read_usage: true,
                    cache_write_usage: false,
                },
            })),
            plan: None,
            correlation: Correlation::default(),
            usage: Some(TokenUsage {
                input_tokens: 30,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 2,
                output_tokens: 9,
                reasoning_tokens: None,
            }),
            context_tokens: Some(16),
            estimated_cost_usd_nanos: Some(1),
            limits: Some(Box::new(RunLimits {
                max_model_turns: Some(12),
                ..RunLimits::default()
            })),
            audit: None,
        };
        let encoded = serde_json::to_value(&run).unwrap();
        assert_eq!(encoded["context_tokens"], 16);
        assert_eq!(
            encoded["limits"],
            serde_json::json!({"max_model_turns": 12})
        );
        assert_eq!(encoded["prompt_identity"]["version"], 7);
        assert_eq!(encoded["resolved_model"]["version"], 1);
        assert_eq!(encoded["resolved_model"]["route"], "provider/model");
        assert_eq!(encoded["resolved_model"]["max_output_tokens"], 4096);
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
        legacy.as_object_mut().unwrap().remove("resolved_model");
        let decoded = serde_json::from_value::<RunSnapshot>(legacy).unwrap();
        assert_eq!(decoded.context_tokens, None);
        assert_eq!(decoded.prompt_identity, None);
        assert_eq!(decoded.resolved_model, None);
        assert_eq!(decoded.usage, run.usage);

        // Snapshots without a value keep their previous wire shape.
        let bare = RunSnapshot {
            context_tokens: None,
            prompt_identity: None,
            resolved_model: None,
            limits: None,
            audit: None,
            ..run.clone()
        };
        let encoded = serde_json::to_value(&bare).unwrap();
        assert!(encoded.get("context_tokens").is_none());
        assert!(encoded.get("prompt_identity").is_none());
        assert!(encoded.get("resolved_model").is_none());
        assert!(encoded.get("limits").is_none());
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

        let encoded = serde_json::to_string(&run.resolved_model).unwrap();
        assert!(encoded.contains("credential_profile"));
        for forbidden in ["sk-super-secret", "literal-api-key", "access-token-value"] {
            assert!(!encoded.contains(forbidden), "{encoded}");
        }
        let mut invalid = serde_json::to_value(&run).unwrap();
        invalid["resolved_model"]["version"] = serde_json::json!(0);
        assert!(serde_json::from_value::<RunSnapshot>(invalid).is_err());

        // Adding request-shape identity to the nested descriptor breaks older
        // `deny_unknown_fields` clients, so negotiation rejects version 8 peers.
        // Version 12 added optional `spawned_by`/`activity` on summaries and
        // `include_sessions`/`included` on snapshots. Version 13 replaced the
        // prompt string with input parts and added profiles, plan identity,
        // steering, correlation, and capabilities.
        // Version 15 added `approval_mode` on summaries. Version 16 added
        // output continuation: `run_output_truncated`, `MessageSnapshot.
        // truncated`, and the `provider_output_truncated` failure kind.
        // Version 17 added the durable `server_id` and `display_name` to
        // `ServerInfo` so clients can key a server profile by identity.
        // Version 18 widened `RunLimits.max_model_turns` and every
        // `turn_ordinal` from u16 to u32; the accepted wire range grew, so
        // every version-17 record still decodes.
        assert_eq!(crate::PROTOCOL_VERSION, 18);
        let mut invalid = serde_json::to_value(&run).unwrap();
        invalid["resolved_model"]["future_control"] = serde_json::json!(true);
        assert!(serde_json::from_value::<RunSnapshot>(invalid).is_err());
    }

    #[test]
    fn run_limits_and_budget_exhaustion_round_trip_and_stay_additive() {
        // Version-9 clients submit prompts without limits; the field defaults
        // to empty and stays off the wire when unset.
        let historical = serde_json::json!({
            "type": "submit_prompt",
            "session_id": SessionId::generate().unwrap(),
            "input": [{"type": "text", "text": "hello"}],
        });
        let SessionCommand::SubmitPrompt { limits, .. } =
            serde_json::from_value::<SessionCommand>(historical).unwrap()
        else {
            panic!("unexpected command")
        };
        assert!(limits.is_empty());
        let bare = SessionCommand::SubmitPrompt {
            session_id: SessionId::generate().unwrap(),
            input: vec![InputPart::text("hello")],
            limits: RunLimits::default(),
            correlation: Correlation::default(),
        };
        assert!(serde_json::to_value(&bare).unwrap().get("limits").is_none());

        let limits = RunLimits {
            max_duration_ms: Some(30_000),
            max_model_turns: Some(4),
            max_tool_calls: Some(40),
            max_total_tokens: Some(200_000),
            max_cost_usd_nanos: Some(1_500_000_000),
            max_input_tokens: None,
            max_output_tokens: Some(8_000),
            max_tool_output_bytes: None,
            max_children: Some(2),
            max_concurrent_children: None,
        };
        let limited = SessionCommand::SubmitPrompt {
            session_id: SessionId::generate().unwrap(),
            input: vec![InputPart::text("hello")],
            limits,
            correlation: Correlation::default(),
        };
        let encoded = serde_json::to_value(&limited).unwrap();
        assert_eq!(encoded["limits"]["max_model_turns"], 4);
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            limited
        );
        let unknown = serde_json::json!({"max_model_turns": 4, "max_pizzas": 1});
        assert!(serde_json::from_value::<RunLimits>(unknown).is_err());

        let outcome = RunOutcome::BudgetExhausted {
            exhaustion: Box::new(BudgetExhaustion {
                limit: BudgetLimitKind::CostUnknown,
                final_response: false,
                message: "usage omitted".to_owned(),
            }),
        };
        let encoded = serde_json::to_value(&outcome).unwrap();
        assert_eq!(encoded["type"], "budget_exhausted");
        assert_eq!(encoded["exhaustion"]["limit"], "cost_unknown");
        assert_eq!(
            serde_json::from_value::<RunOutcome>(encoded).unwrap(),
            outcome
        );
        assert_eq!(
            serde_json::to_value(RunStatus::BudgetExhausted).unwrap(),
            "budget_exhausted"
        );
    }
    #[test]
    fn steering_profile_and_correlation_wire_shapes_are_stable() {
        let steer = SessionCommand::SteerRun {
            run_id: id(4),
            input: vec![InputPart::text("focus on tests")],
            interrupt: true,
        };
        let encoded = serde_json::to_value(&steer).unwrap();
        assert_eq!(encoded["type"], "steer_run");
        assert_eq!(encoded["interrupt"], true);
        assert_eq!(encoded["input"][0]["type"], "text");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            steer
        );
        let plain = SessionCommand::SteerRun {
            run_id: id(4),
            input: vec![InputPart::text("x")],
            interrupt: false,
        };
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("interrupt")
                .is_none()
        );
        assert_eq!(steer.kind(), SessionCommandKind::SteerRun);
        assert_eq!(
            serde_json::to_value(SessionCommandKind::SetSessionProfile).unwrap(),
            "set_session_profile"
        );
        assert_eq!(SessionCommandKind::ALL.len(), 13);

        let create = SessionCommand::CreateSession {
            workspace_id: id(2),
            parent_id: None,
            model: ModelSelection::default(),
            approval_mode: ApprovalMode::Ask,
            profile: AgentProfileId::new("review").unwrap(),
            correlation: Correlation::new(std::collections::BTreeMap::from([(
                "thread".to_owned(),
                "t1".to_owned(),
            )]))
            .unwrap(),
        };
        let encoded = serde_json::to_value(&create).unwrap();
        assert_eq!(encoded["profile"], "review");
        assert_eq!(encoded["correlation"]["thread"], "t1");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            create
        );
        let minimal = serde_json::json!({
            "type": "create_session",
            "workspace_id": id::<WorkspaceId>(2),
            "model": {},
        });
        let SessionCommand::CreateSession {
            profile,
            correlation,
            ..
        } = serde_json::from_value::<SessionCommand>(minimal).unwrap()
        else {
            panic!("unexpected command")
        };
        assert!(profile.is_default());
        assert!(correlation.is_empty());

        let outcomes = [
            (
                CommandOutcome::SteeringQueued {
                    run_id: id(4),
                    message_id: id(9),
                },
                "steering_queued",
            ),
            (
                CommandOutcome::SessionProfileSet {
                    session_id: id(3),
                    profile: AgentProfileId::default(),
                },
                "session_profile_set",
            ),
        ];
        for (outcome, tag) in outcomes {
            let encoded = serde_json::to_value(&outcome).unwrap();
            assert_eq!(encoded["type"], tag);
            assert_eq!(
                serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
                outcome
            );
        }

        let identity = RunPlanIdentity {
            profile: AgentProfileId::default(),
            descriptor_version: 2,
            digest: crate::AgentPlanDigest::from_hash(ContentHash::from_bytes([7; 32])),
            credential_epoch: crate::CredentialEpoch::new(1),
        };
        let events = [
            (
                SessionEvent::SteeringApplied {
                    run_id: id(4),
                    message_id: id(9),
                    turn_ordinal: 3,
                },
                "steering_applied",
            ),
            (
                SessionEvent::SteeringSuperseded {
                    run_id: id(4),
                    message_id: id(9),
                },
                "steering_superseded",
            ),
            (
                SessionEvent::RunInterrupted {
                    run_id: id(4),
                    turn_ordinal: 3,
                },
                "run_interrupted",
            ),
        ];
        for (event, tag) in events {
            let encoded = serde_json::to_value(&event).unwrap();
            assert_eq!(encoded["type"], tag);
            assert_eq!(
                serde_json::from_value::<SessionEvent>(encoded).unwrap(),
                event
            );
        }
        // Legacy run_started envelopes carry no plan; new ones carry the
        // identity verbatim.
        let legacy = serde_json::json!({
            "type": "run_started",
            "session": serde_json::to_value(SessionSummary {
                activity: None,
                spawned_by: None,
                purpose: SessionPurpose::Task,
                id: id(3),
                workspace_id: id(2),
                parent_id: None,
                title: "s".to_owned(),
                status: SessionStatus::Running,
                active_run_id: Some(id(4)),
                queued_prompts: 0,
                model: None,
                profile: AgentProfileId::default(),
                approval_mode: ApprovalMode::Auto,
                correlation: Correlation::default(),
                context_tokens: None,
                accounting: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 1,
                last_outcome: None,
            }).unwrap(),
            "run_id": id::<RunId>(4),
        });
        let SessionEvent::RunStarted { plan, .. } =
            serde_json::from_value::<SessionEvent>(legacy).unwrap()
        else {
            panic!("unexpected event")
        };
        assert!(plan.is_none());
        let started = serde_json::to_value(SessionEvent::RunStarted {
            session: serde_json::from_value(serde_json::json!({
                "id": id::<SessionId>(3), "workspace_id": id::<WorkspaceId>(2),
                "title": "s", "status": "idle", "queued_prompts": 0, "updated_at_ms": 1
            }))
            .unwrap(),
            run_id: id(4),
            plan: Some(Box::new(identity.clone())),
        })
        .unwrap();
        assert_eq!(started["plan"]["descriptor_version"], 2);
        assert_eq!(started["plan"]["credential_epoch"], 1);
        assert_eq!(BudgetLimitKind::ALL.len(), 10);
        assert_eq!(
            serde_json::to_value(BudgetLimitKind::ToolOutputBytes).unwrap(),
            "tool_output_bytes"
        );
    }
}
