//! Versioned capability discovery.
//!
//! Clients learn what a server supports from this document rather than from
//! provider names or trial commands. The response is deliberately tolerant of
//! unknown fields so a client built against an older revision can still read
//! the parts it knows; inbound requests stay strict.

use serde::{Deserialize, Serialize};

use crate::{
    AgentProfileId, ApprovalMode, BudgetLimitKind, ContentHash, GuidanceKind, InputPartKind,
    ToolExposure, WorkspaceId, sessions::SessionCommandKind,
};

/// Schema version of [`ServerCapabilities`]. Bumped when the meaning of an
/// existing field changes; additive fields do not bump it.
pub const CAPABILITIES_VERSION: u16 = 1;

/// Asks for the capability document. Workspace-scoped sections (profiles) are
/// included only when a workspace is named, because they derive from that
/// workspace's layered configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub version: u16,
    pub protocol_version: u16,
    pub server_version: String,
    /// Input part kinds the server accepts in prompts and steering.
    pub input_parts: Vec<InputPartKind>,
    /// Every session command the server routes.
    pub commands: Vec<SessionCommandKind>,
    pub steering: SteeringCapabilities,
    pub limits: LimitCapabilities,
    /// Approval decision tags the server accepts.
    pub approvals: Vec<String>,
    /// Approval modes a session may select.
    pub approval_modes: Vec<ApprovalMode>,
    /// Present only when the request named a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<AgentProfileSummary>>,
    /// Bounds every plan's tool catalog observes, and how large external
    /// catalogs are disclosed.
    pub tools: ToolCapabilities,
    /// Present only when the request named a workspace: the external tool
    /// hosts and skill index of that workspace's default plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_tools: Option<WorkspaceToolCapabilities>,
    /// How committed events are delivered to observers.
    pub events: EventCapabilities,
    /// Present only when the request named a workspace: that workspace's
    /// delegation roster and the runtime's delegation ceilings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationCapabilities>,
}

/// Delivery contract for post-commit observers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCapabilities {
    /// Events are published only after their durable commit.
    pub post_commit: bool,
    /// Events per replay page a subscriber is served from its cursor.
    pub replay_page: u16,
    /// Most concurrent SSE subscriptions the server accepts (503 beyond).
    pub max_subscriptions: u16,
    pub max_event_bytes: u64,
    /// Whether committed events are ever pruned. `false` means a cursor
    /// from any point in the store's history replays.
    pub retention_bounded: bool,
}

/// Catalog and exposure bounds. Clients learn from here that a run may see
/// `select_tools` and `load_skill` rather than inferring it from tool names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilities {
    pub max_catalog_tools: u32,
    pub max_tool_schema_bytes: u64,
    pub max_catalog_schema_bytes: u64,
    /// External catalogs at or under both thresholds are sent whole.
    pub full_exposure_tools: u32,
    pub full_exposure_schema_bytes: u64,
    /// Most external tools one run may pin under progressive exposure.
    pub max_pinned_tools: u32,
    pub max_indexed_skills: u32,
    /// Prefixes external tool names carry, by host kind.
    pub external_prefixes: Vec<String>,
}

/// One workspace's external hosts and skills as its default plan sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceToolCapabilities {
    pub catalog_digest: ContentHash,
    pub exposure: ToolExposure,
    pub hosts: Vec<ToolHostSummary>,
    pub excluded_tools: u32,
    pub skills: SkillCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolHostSummary {
    pub name: String,
    pub generation: u64,
    pub tool_count: u32,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCapabilities {
    pub digest: ContentHash,
    pub indexed: u32,
    /// Documents the model may load itself through `load_skill`.
    pub disclosed: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Every indexed document, so a client can list and complete them.
    /// Bounded by the runtime's skill index (`ToolCapabilities::max_indexed_skills`
    /// entries, descriptions at most 512 bytes each). Absent from servers
    /// older than this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<SkillSummary>,
}

/// One indexed guidance document of the workspace's default plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    /// The slash name: `/name` invokes a command; a skill is loaded by the
    /// model through `load_skill` or forwarded by a client as `/name`.
    pub name: String,
    pub kind: GuidanceKind,
    /// Workspace-relative path, or `pack:<id>/<relative>` for a pack document.
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Whether the model may load this document itself.
    pub disclosed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringCapabilities {
    /// Steering input is injected at the next model/tool boundary.
    pub boundary: bool,
    /// Steering may also interrupt the in-flight provider stream or tool.
    pub interrupt: bool,
    /// Most steering messages pending per run.
    pub max_pending_per_run: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitCapabilities {
    /// Budget kinds `RunLimits` may impose and settle with a typed outcome.
    pub supported: Vec<BudgetLimitKind>,
    pub max_request_bytes: u64,
    pub max_event_bytes: u64,
    pub max_input_parts: u16,
    pub max_input_text_bytes: u64,
    pub max_input_file_parts: u16,
    pub max_input_file_bytes: u64,
    pub max_pending_prompts: u16,
    /// Hard ceiling on `RunLimits::max_children`.
    pub max_children: u16,
    /// Hard ceiling on `RunLimits::max_concurrent_children`.
    pub max_concurrent_children: u16,
    /// Deepest sub-agent nesting the runtime executes (the ceiling on a
    /// roster's `max_depth`).
    pub max_child_depth: u16,
    pub max_correlation_entries: u16,
    /// Most sessions one root run's delegation tree may hold.
    #[serde(default)]
    pub max_descendants: u16,
    /// Most times one run resumes a turn the provider cut at its output token
    /// limit before settling as `provider_output_truncated`.
    #[serde(default)]
    pub max_output_continuations: u16,
    /// Bounds of the optional typed-output contract on `submit_prompt`: the
    /// schema byte, nesting, and value ceilings and the most repair turns a
    /// caller may request. Absent (zero) on servers before protocol 19.
    #[serde(default)]
    pub max_output_schema_bytes: u64,
    #[serde(default)]
    pub max_output_schema_depth: u16,
    #[serde(default)]
    pub max_output_schema_values: u32,
    #[serde(default)]
    pub max_output_repair_turns: u8,
}

/// A configured profile a session may select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfileSummary {
    pub id: AgentProfileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub approval_mode: ApprovalMode,
    /// Set when the profile is declared by an agent pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<PackSummary>,
}

/// The agent pack behind a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackSummary {
    pub id: String,
    pub version: String,
}

/// The operator-declared role of one delegation route. Roles are declared,
/// never inferred from price or benchmarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRole {
    Fast,
    Balanced,
    Strong,
}

impl DelegationRole {
    pub const ALL: [Self; 3] = [Self::Fast, Self::Balanced, Self::Strong];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fast" => Some(Self::Fast),
            "balanced" => Some(Self::Balanced),
            "strong" => Some(Self::Strong),
            _ => None,
        }
    }
}

/// One route an agent may delegate to, with the facts the agent needs to
/// choose it. Secret-free: routes and catalog metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRosterEntry {
    /// Canonical `provider/model` route.
    pub route: String,
    pub role: DelegationRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The reasoning effort children spawned on this entry run at. `None`
    /// derives one from the role: `fast` → low, `balanced` → medium, never
    /// above the parent's own effort; `strong` inherits the parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<qq_reasoning::ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// This route's blended per-token price relative to the spawning model's,
    /// in thousandths (1000 = the same price). `None` when either price is
    /// unknown. Observational guidance for the model; never used for budgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_cost_permille: Option<u32>,
}

/// The delegation roster and bounds a plan compiled with. Empty roster means
/// `spawn_agent` resolves through the legacy worker/parent fallback only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRoster {
    pub roster: Vec<DelegationRosterEntry>,
    pub default_role: DelegationRole,
    /// Deepest nesting this configuration permits (1 = children only).
    pub max_depth: u16,
    /// Whether children may be spawned with write authority.
    pub write_children: bool,
}

impl DelegationRoster {
    #[must_use]
    pub fn route_for_role(&self, role: DelegationRole) -> Option<&str> {
        self.entry_for_role(role).map(|entry| entry.route.as_str())
    }

    #[must_use]
    pub fn entry_for_role(&self, role: DelegationRole) -> Option<&DelegationRosterEntry> {
        self.roster.iter().find(|entry| entry.role == role)
    }

    #[must_use]
    pub fn entry_for_route(&self, route: &str) -> Option<&DelegationRosterEntry> {
        self.roster.iter().find(|entry| entry.route == route)
    }

    #[must_use]
    pub fn contains_route(&self, route: &str) -> bool {
        self.entry_for_route(route).is_some()
    }
}

/// The effort a child spawned on `entry` runs at when the parent runs at
/// `parent`. An explicit roster effort wins. Otherwise the role decides:
/// `fast` lookups and breadth at `low`, the `balanced` worker at `medium`,
/// each capped by the parent's effort so a child never thinks harder than
/// the run that delegated to it; `strong` inherits the parent's pin.
#[must_use]
pub fn child_reasoning_effort(
    entry: &DelegationRosterEntry,
    parent: Option<qq_reasoning::ReasoningEffort>,
) -> Option<qq_reasoning::ReasoningEffort> {
    use qq_reasoning::ReasoningEffort;
    if let Some(effort) = entry.effort {
        return Some(effort);
    }
    let ceiling = match entry.role {
        DelegationRole::Fast => ReasoningEffort::Low,
        DelegationRole::Balanced => ReasoningEffort::Medium,
        DelegationRole::Strong => return parent,
    };
    let rank = |effort: ReasoningEffort| {
        ReasoningEffort::ALL
            .iter()
            .position(|candidate| *candidate == effort)
    };
    match parent {
        // `Default` and an unpinned parent both mean the provider chooses;
        // the role ceiling is then the only bound worth sending.
        None | Some(ReasoningEffort::Default) => Some(ceiling),
        Some(parent) => match (rank(parent), rank(ceiling)) {
            (Some(parent_rank), Some(ceiling_rank)) if parent_rank <= ceiling_rank => Some(parent),
            _ => Some(ceiling),
        },
    }
}

impl Default for DelegationRoster {
    fn default() -> Self {
        Self {
            roster: Vec::new(),
            default_role: DelegationRole::Balanced,
            max_depth: 1,
            write_children: false,
        }
    }
}

/// Server-advertised delegation settings for a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationCapabilities {
    #[serde(flatten)]
    pub roster: DelegationRoster,
    /// Hard ceilings the runtime enforces regardless of configuration.
    pub max_roster_entries: u16,
    pub max_depth_ceiling: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_tolerate_unknown_response_fields_but_not_request_fields() {
        let json = r#"{
            "version": 1,
            "protocol_version": 14,
            "server_version": "0.1.0",
            "input_parts": ["text", "workspace_file", "hologram"],
            "commands": ["submit_prompt"],
            "steering": {"boundary": true, "interrupt": true, "max_pending_per_run": 4, "telepathy": true},
            "limits": {
                "supported": ["duration"],
                "max_request_bytes": 1, "max_event_bytes": 1, "max_input_parts": 1,
                "max_input_text_bytes": 1, "max_input_file_parts": 1, "max_input_file_bytes": 1,
                "max_pending_prompts": 1, "max_children": 1, "max_concurrent_children": 1,
                "max_child_depth": 1, "max_correlation_entries": 1
            },
            "approvals": ["deny"],
            "approval_modes": ["auto"],
            "tools": {
                "max_catalog_tools": 512, "max_tool_schema_bytes": 16384,
                "max_catalog_schema_bytes": 1048576, "full_exposure_tools": 24,
                "full_exposure_schema_bytes": 32768, "max_pinned_tools": 32,
                "max_indexed_skills": 64, "external_prefixes": ["mcp__"]
            },
            "events": {
                "post_commit": true, "replay_page": 128, "max_subscriptions": 64,
                "max_event_bytes": 1048576, "retention_bounded": false
            },
            "future_section": {"anything": 1}
        }"#;
        // Unknown enum values inside arrays are still rejected: a client that
        // cannot name a kind must not silently pretend it understands it.
        assert!(serde_json::from_str::<ServerCapabilities>(json).is_err());
        let known = json.replace(", \"hologram\"", "");
        let decoded: ServerCapabilities = serde_json::from_str(&known).unwrap();
        assert_eq!(decoded.protocol_version, 14);
        assert!(decoded.profiles.is_none());
        assert!(decoded.workspace_tools.is_none());
        assert_eq!(decoded.tools.max_pinned_tools, 32);
        assert!(decoded.steering.interrupt);

        assert!(serde_json::from_str::<CapabilitiesRequest>(r#"{"workspace":"x"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<CapabilitiesRequest>("{}").unwrap(),
            CapabilitiesRequest::default()
        );
    }
}
