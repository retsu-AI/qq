use serde::{Deserialize, Serialize};

use crate::{
    AgentProfileId, ApprovalMode, BudgetLimitKind, ContentHash, GuidanceKind, InputPartKind,
    ToolExposure, WorkspaceId, sessions::SessionCommandKind,
};

pub const CAPABILITIES_VERSION: u16 = 1;

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
    pub input_parts: Vec<InputPartKind>,
    pub commands: Vec<SessionCommandKind>,
    pub steering: SteeringCapabilities,
    pub limits: LimitCapabilities,
    pub approvals: Vec<String>,
    pub approval_modes: Vec<ApprovalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<AgentProfileSummary>>,
    pub tools: ToolCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_tools: Option<WorkspaceToolCapabilities>,
    pub events: EventCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCapabilities {
    pub post_commit: bool,
    pub replay_page: u16,
    pub max_subscriptions: u16,
    pub max_event_bytes: u64,
    pub retention_bounded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilities {
    pub max_catalog_tools: u32,
    pub max_tool_schema_bytes: u64,
    pub max_catalog_schema_bytes: u64,
    pub full_exposure_tools: u32,
    pub full_exposure_schema_bytes: u64,
    pub max_pinned_tools: u32,
    pub max_indexed_skills: u32,
    pub external_prefixes: Vec<String>,
}

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
    pub disclosed: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<SkillSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub kind: GuidanceKind,
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub disclosed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringCapabilities {
    pub boundary: bool,
    pub interrupt: bool,
    pub max_pending_per_run: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitCapabilities {
    pub supported: Vec<BudgetLimitKind>,
    pub max_request_bytes: u64,
    pub max_event_bytes: u64,
    pub max_input_parts: u16,
    pub max_input_text_bytes: u64,
    pub max_input_file_parts: u16,
    pub max_input_file_bytes: u64,
    pub max_pending_prompts: u16,
    pub max_children: u16,
    pub max_concurrent_children: u16,
    pub max_child_depth: u16,
    pub max_correlation_entries: u16,
    #[serde(default)]
    pub max_descendants: u16,
    #[serde(default)]
    pub max_output_continuations: u16,
    #[serde(default)]
    pub max_output_schema_bytes: u64,
    #[serde(default)]
    pub max_output_schema_depth: u16,
    #[serde(default)]
    pub max_output_schema_values: u32,
    #[serde(default)]
    pub max_output_repair_turns: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfileSummary {
    pub id: AgentProfileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub approval_mode: ApprovalMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<PackSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackSummary {
    pub id: String,
    pub version: String,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRosterEntry {
    pub route: String,
    pub role: DelegationRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<qq_reasoning::ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_cost_permille: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRoster {
    pub roster: Vec<DelegationRosterEntry>,
    pub default_role: DelegationRole,
    pub max_depth: u16,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationCapabilities {
    #[serde(flatten)]
    pub roster: DelegationRoster,
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
