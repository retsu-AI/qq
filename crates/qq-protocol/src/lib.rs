//! Commands, events, identifiers, and versioned wire types.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

mod capabilities;
mod ids;
mod input;
mod limits;
mod local;
mod plan;
mod sessions;

pub use capabilities::{
    AgentProfileSummary, CAPABILITIES_VERSION, CapabilitiesRequest, DelegationCapabilities,
    DelegationRole, DelegationRoster, DelegationRosterEntry, EventCapabilities, LimitCapabilities,
    PackSummary, ServerCapabilities, SkillCapabilities, SkillSummary, SteeringCapabilities,
    ToolCapabilities, ToolHostSummary, WorkspaceToolCapabilities,
};
pub use ids::{CommandId, IdError, MessageId, RunId, SessionId, StoreId, ToolCallId, WorkspaceId};
pub use input::{
    Correlation, CorrelationError, InputError, InputPart, InputPartKind, MAX_CORRELATION_BYTES,
    MAX_CORRELATION_ENTRIES, MAX_CORRELATION_KEY_BYTES, MAX_CORRELATION_VALUE_BYTES,
    MAX_INPUT_FILE_BYTES, MAX_INPUT_FILE_PARTS, MAX_INPUT_PARTS, MAX_INPUT_PATH_BYTES,
    MAX_INPUT_TEXT_BYTES, MAX_RESOLVED_INPUT_BYTES, validate_input,
};
pub use limits::{
    MAX_EVENT_BYTES, MAX_MODEL_BYTES, MAX_ORGANIZATION_BYTES, MAX_REQUEST_BYTES,
    MAX_WORKSPACE_BYTES,
};
pub use local::{
    LocalConnectionError, LocalServerConnection, MAX_BASE_URL_BYTES, MAX_CREDENTIAL_BYTES,
    ServerConnection, ServerConnectionError,
};
pub use plan::{
    AgentPlanDigest, AgentProfileId, AgentProfileIdError, CredentialEpoch, MAX_PROFILE_ID_BYTES,
    RunPlanIdentity,
};
pub use qq_reasoning::{ReasoningEvent, ReasoningKind};
pub use sessions::{
    AccountingTotal, ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution,
    AuditOutcome, AuditRecord, BudgetExhaustion, BudgetLimitKind, CapabilitySupport,
    ChildAuthority, CommandOutcome, CommandReceipt, CommandRequest, ContentHash, ContentHashError,
    ContextSourceOutcome, ContextSourceRecord, CursorError, EditPreview, EventCursor,
    GenerationCapabilities, GuidanceIdentity, GuidanceKind, InstructionHash, InstructionHashError,
    MAX_INCLUDED_SESSIONS, MessageRole, MessageSnapshot, MessageState, ModelCatalogRequest,
    ModelDescriptor, ModelPricing, ModelPricingTier, ModelSelection, PromptCacheCapabilities,
    PromptVersion, ProviderRequestShapeIdentity, ProviderRequestShapeVersion, ResolvedModel,
    ResolvedModelVersion, RunActivity, RunFailure, RunLimits, RunOutcome, RunPromptIdentity,
    RunSnapshot, RunStatus, SessionAccounting, SessionCommand, SessionCommandKind, SessionEvent,
    SessionEventEnvelope, SessionPurpose, SessionSnapshot, SessionStatus, SessionSummary,
    ShellCommandPreview, SnapshotRequest, SpawnOrigin, SubscribeRequest, TextChannel, TokenUsage,
    ToolCallDisplay, ToolCallSnapshot, ToolCallState, ToolExposure, WorkspaceGrantOutcome,
    WorkspaceSnapshot, WorkspaceSummary,
};

pub const PROTOCOL_VERSION: u16 = 18;

/// Slash commands owned by interactive clients rather than the shared
/// runtime. Keeping this vocabulary in the transport-neutral protocol avoids
/// a client/runtime drift where one side forwards a name the other reserves.
pub const RESERVED_CLIENT_SLASH_COMMANDS: [&str; 20] = [
    "/help",
    "/commands",
    "/models",
    "/profile",
    "/approval",
    "/skills",
    "/sessions",
    "/resume",
    "/agents",
    "/theme",
    "/editor",
    "/new",
    "/compact",
    "/rollback",
    "/prune",
    "/mouse",
    "/attention",
    "/changes",
    "/quit",
    "/exit",
];

/// Starts one model run from user input. The in-process form of a prompt:
/// `new` wraps one text part, `from_parts` carries validated structured input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommand {
    input: Vec<InputPart>,
}

impl RunCommand {
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            input: vec![InputPart::text(prompt)],
        }
    }

    /// Wraps structured input after checking the shared bounds.
    pub fn from_parts(input: Vec<InputPart>) -> Result<Self, InputError> {
        validate_input(&input)?;
        Ok(Self { input })
    }

    #[must_use]
    pub fn input(&self) -> &[InputPart] {
        &self.input
    }

    #[must_use]
    pub fn into_input(self) -> Vec<InputPart> {
        self.input
    }
}

/// Provider-independent events produced by one model run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    Started,
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
    Usage {
        usage: TokenUsage,
    },
    Completed,
    Failed {
        kind: RunFailureKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailureKind {
    InvalidCommand,
    Configuration,
    Authentication,
    Policy,
    Server,
    /// A fail-closed context source could not supply its context before the
    /// run's first provider request.
    ContextSource,
    ProviderConfiguration,
    ProviderAuthentication,
    ProviderRateLimited,
    ProviderInvalidRequest,
    /// The assembled request exceeded the model's context window. The
    /// session layer treats this as recoverable: it compacts and retries
    /// before ever surfacing this kind as terminal.
    ProviderContextExceeded,
    ProviderUnavailable,
    ProviderTransport,
    ProviderApi,
    ProviderResponse,
    ProviderProtocol,
    /// The provider stopped at its output token limit on every continuation
    /// the runtime allowed. The partial turns are persisted in the transcript.
    ProviderOutputTruncated,
}

/// Version information returned by the server health endpoint. Tolerates
/// unknown fields so an older client can read a newer server's answer and
/// report the version skew instead of a decode failure.
///
/// `server_id` is the durable identity of the store this server owns: it
/// survives restarts, endpoint changes, and token rotation, and it is the
/// same value every `EventCursor` from this server carries. Clients key a
/// saved server profile by it, never by address. `display_name` is a
/// human label (configured, else the host name) and carries no identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol_version: u16,
    pub version: String,
    pub pid: u32,
    pub server_id: StoreId,
    pub display_name: String,
}

/// Longest `ServerInfo.display_name` a server may advertise, in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 64;

impl ServerInfo {
    /// Whether every field satisfies the wire invariants: a nonzero pid, a
    /// printable bounded version, and a printable bounded display name.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.pid != 0
            && valid_process_version(&self.version)
            && valid_display_name(&self.display_name)
    }
}

/// Whether `version` may be advertised in `ServerInfo.version`: 1..=256 bytes
/// of printable ASCII without spaces.
#[must_use]
pub fn valid_process_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 256
        && version.bytes().all(|byte| byte.is_ascii_graphic())
}

pub(crate) fn valid_display_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DISPLAY_NAME_BYTES
        && !name.chars().any(char::is_control)
        && name.trim() == name
}

/// Reduces an arbitrary host or configured label to a valid display name,
/// or `None` when nothing printable remains.
#[must_use]
pub fn sanitize_display_name(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut name = String::with_capacity(trimmed.len().min(MAX_DISPLAY_NAME_BYTES));
    for character in trimmed.chars() {
        if character.is_control() {
            continue;
        }
        if name.len() + character.len_utf8() > MAX_DISPLAY_NAME_BYTES {
            break;
        }
        name.push(character);
    }
    let name = name.trim_end().to_owned();
    valid_display_name(&name).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_bounded_printable_and_trimmed() {
        assert_eq!(
            sanitize_display_name("  build-box.local \n"),
            Some("build-box.local".to_owned())
        );
        assert_eq!(
            sanitize_display_name("bad\u{7}name\u{1b}[0m"),
            Some("badname[0m".to_owned())
        );
        assert_eq!(sanitize_display_name("   "), None);
        assert_eq!(sanitize_display_name("\u{7}\u{8}"), None);

        let long = "x".repeat(MAX_DISPLAY_NAME_BYTES + 10);
        let clamped = sanitize_display_name(&long).unwrap();
        assert_eq!(clamped.len(), MAX_DISPLAY_NAME_BYTES);

        // A multi-byte character that would straddle the bound is dropped whole.
        let mixed = format!("{}é", "x".repeat(MAX_DISPLAY_NAME_BYTES - 1));
        assert_eq!(
            sanitize_display_name(&mixed),
            Some("x".repeat(MAX_DISPLAY_NAME_BYTES - 1))
        );
    }

    #[test]
    fn server_info_well_formed_checks_every_field() {
        let info = ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            version: "0.1.0".to_owned(),
            pid: 1,
            server_id: StoreId::from_bytes([1; 16]),
            display_name: "devbox".to_owned(),
        };
        assert!(info.is_well_formed());
        assert!(
            !ServerInfo {
                pid: 0,
                ..info.clone()
            }
            .is_well_formed()
        );
        assert!(
            !ServerInfo {
                version: "has space".to_owned(),
                ..info.clone()
            }
            .is_well_formed()
        );
        assert!(
            !ServerInfo {
                display_name: String::new(),
                ..info.clone()
            }
            .is_well_formed()
        );
        assert!(
            !ServerInfo {
                display_name: " padded".to_owned(),
                ..info
            }
            .is_well_formed()
        );
    }
}
