use qq_protocol::{
    AgentPlanDigest, AgentProfileId, ContentHash, DelegationRoster, PromptVersion, ResolvedModel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::PlanCompileError;

pub const DESCRIPTOR_VERSION: u16 = 16;

const DIGEST_DOMAIN: &[u8] = b"qq-agent-plan-descriptor-v16\0";
const V15_DIGEST_DOMAIN: &[u8] = b"qq-agent-plan-descriptor-v15\0";
const V14_DIGEST_DOMAIN: &[u8] = b"qq-agent-plan-descriptor-v14\0";
const LEGACY_DIGEST_DOMAIN: &[u8] = b"qq-agent-plan-descriptor-v13\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum CredentialReference {
    None,
    Environment(String),
    Stored(String),
    Inline,
    Profile(String),
    AmbientChain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub api: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_mode: Option<String>,
    pub auth_scheme: String,
    pub credential: CredentialReference,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub header_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

impl ProviderDescriptor {
    #[must_use]
    pub fn embedded() -> Self {
        Self {
            id: "embedded".to_owned(),
            api: "embedded".to_owned(),
            endpoint: None,
            endpoint_mode: None,
            auth_scheme: "unknown".to_owned(),
            credential: CredentialReference::None,
            header_names: Vec::new(),
            region: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDescriptor {
    pub name: String,
    pub transport: McpTransportKind,
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    pub credential: CredentialReference,
    pub eager: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    pub call_timeout_seconds: u64,
    pub max_concurrent_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalogDescriptor {
    pub catalog_digest: ContentHash,
    pub exposure: crate::catalog::Exposure,
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<crate::catalog::HostSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<crate::catalog::ExcludedTool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_model_routes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_grants: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackDescriptor {
    pub id: String,
    pub version: String,
    pub manifest_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillIndexDescriptor {
    pub digest: ContentHash,
    pub indexed: usize,
    pub disclosed: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputProjection {
    StrictReservedV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanDescriptor {
    pub version: u16,
    pub profile: AgentProfileId,
    pub adapter_build: String,
    pub provider: ProviderDescriptor,
    pub model: ResolvedModel,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_pack_roots: Vec<String>,
    pub prompt_version: PromptVersion,
    pub instruction_hash: qq_protocol::InstructionHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_source: Option<String>,
    pub tools: ToolCatalogDescriptor,
    pub delegation: DelegationRoster,
    pub audit: AuditDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_read_images: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output_projection: Option<ToolOutputProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_delegate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_configuration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub skills: SkillIndexDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<PackDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_sources: Vec<ContextSourceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSourceDescriptor {
    pub name: String,
    pub version: String,
    pub budget: ContextBudgetDescriptor,
    pub fail_policy: FailPolicyDescriptor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBudgetDescriptor {
    pub max_bytes: u64,
    pub max_items: u32,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailPolicyDescriptor {
    Open,
    Closed,
}

impl From<&crate::context_source::RegisteredSource> for ContextSourceDescriptor {
    fn from(registered: &crate::context_source::RegisteredSource) -> Self {
        let budget = registered.budget;
        Self {
            name: registered.source.name().to_owned(),
            version: registered.source.version().to_owned(),
            budget: ContextBudgetDescriptor {
                max_bytes: budget.max_bytes as u64,
                max_items: u32::try_from(budget.max_items).unwrap_or(u32::MAX),
                timeout_ms: u64::try_from(budget.timeout.as_millis()).unwrap_or(u64::MAX),
            },
            fail_policy: match registered.source.fail_policy() {
                crate::FailPolicy::Open => FailPolicyDescriptor::Open,
                crate::FailPolicy::Closed => FailPolicyDescriptor::Closed,
            },
        }
    }
}

impl AgentPlanDescriptor {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PlanCompileError> {
        let domain = match self.version {
            13 => LEGACY_DIGEST_DOMAIN,
            14 => V14_DIGEST_DOMAIN,
            15 => V15_DIGEST_DOMAIN,
            16 => DIGEST_DOMAIN,
            _ => {
                return Err(PlanCompileError::Encode {
                    message: format!("unsupported descriptor version {}", self.version),
                });
            }
        };
        if (self.version >= 15) != self.tool_output_projection.is_some() {
            return Err(PlanCompileError::Encode {
                message:
                    "tool output projection identity must be present in descriptor 15 and later"
                        .to_owned(),
            });
        }
        if self.version < 16 && !self.protected_pack_roots.is_empty() {
            return Err(PlanCompileError::Encode {
                message: "protected pack roots require descriptor 16".to_owned(),
            });
        }
        let mut bytes = domain.to_vec();
        serde_json::to_writer(&mut bytes, self).map_err(|error| PlanCompileError::Encode {
            message: error.to_string(),
        })?;
        Ok(bytes)
    }

    pub fn digest(&self) -> Result<AgentPlanDigest, PlanCompileError> {
        Ok(self.encode()?.0)
    }

    pub fn encode(&self) -> Result<(AgentPlanDigest, String), PlanCompileError> {
        let bytes = self.canonical_bytes()?;
        let digest =
            AgentPlanDigest::from_hash(ContentHash::from_bytes(Sha256::digest(&bytes).into()));
        let json = String::from_utf8(bytes[DIGEST_DOMAIN.len()..].to_vec()).map_err(|_| {
            PlanCompileError::Encode {
                message: "descriptor JSON was not UTF-8".to_owned(),
            }
        })?;
        Ok((digest, json))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditDescriptor {
    pub mode: AuditModeDescriptor,
    pub max_revisions: u16,
    pub role: qq_protocol::DelegationRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditModeDescriptor {
    Off,
    Heuristic,
    Always,
}

impl From<crate::runtime::AuditPolicy> for AuditDescriptor {
    fn from(policy: crate::runtime::AuditPolicy) -> Self {
        Self {
            mode: match policy.mode {
                crate::runtime::AuditMode::Off => AuditModeDescriptor::Off,
                crate::runtime::AuditMode::Heuristic => AuditModeDescriptor::Heuristic,
                crate::runtime::AuditMode::Always => AuditModeDescriptor::Always,
            },
            max_revisions: policy.max_revisions,
            role: policy.role,
        }
    }
}
