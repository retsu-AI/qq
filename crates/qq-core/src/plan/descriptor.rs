//! The secret-free, canonically encoded account of a compiled plan.

use qq_protocol::{
    AgentPlanDigest, AgentProfileId, ContentHash, DelegationRoster, PromptVersion, ResolvedModel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::PlanCompileError;

/// Version of the descriptor's canonical encoding. Bump it whenever a field is
/// added, removed, renamed, or its normalization changes, so historical digests
/// are never compared against a different encoding.
pub const DESCRIPTOR_VERSION: u16 = 6;

/// Domain separator prepended to the canonical bytes before hashing.
const DIGEST_DOMAIN: &[u8] = b"qq-agent-plan-descriptor-v6\0";

/// Where a credential comes from, without its value. Two plans that read the
/// same environment variable or stored credential name share a reference and
/// therefore a digest even when the secret behind it has rotated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum CredentialReference {
    /// No credential is sent.
    None,
    /// An environment variable named here supplies the secret.
    Environment(String),
    /// A credential stored under this name in the credential store.
    Stored(String),
    /// A literal in configuration. The value is not represented; the
    /// reference records only that one was configured inline.
    Inline,
    /// A named request-time credential profile (OAuth-style refreshing auth).
    Profile(String),
    /// An ambient provider chain (for example the AWS default chain).
    AmbientChain,
}

/// Secret-free identity of the compiled provider adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    /// The configured provider name (`openai`, `anthropic`, a custom id).
    pub id: String,
    /// The wire protocol family (`openai_responses`, `anthropic_messages`,
    /// `bedrock_converse`, `embedded`, ...).
    pub api: String,
    /// Endpoint with any userinfo, query, and fragment removed; `None` for
    /// SDK-managed or embedded providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `base` or `exact` for HTTP endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_mode: Option<String>,
    /// The authorization scheme (`bearer`, `api_key`, `header:<name>`,
    /// `sigv4`, `none`).
    pub auth_scheme: String,
    pub credential: CredentialReference,
    /// Names of static headers sent with every request. Values may be
    /// sensitive and are excluded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub header_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

impl ProviderDescriptor {
    /// The descriptor for a provider handed directly to the runtime without
    /// configuration; nothing about its transport is known to core.
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

/// Secret-free identity of one configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDescriptor {
    pub name: String,
    pub transport: McpTransportKind,
    /// Stdio command or HTTP URL (userinfo, query, and fragment removed).
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Names of environment variables passed through to a stdio server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    pub credential: CredentialReference,
    pub eager: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    pub call_timeout_seconds: u64,
    pub max_concurrent_calls: u32,
}

/// The complete tool catalog every run of the plan selects from: built-ins,
/// session tools, and every external host's admitted declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalogDescriptor {
    /// Digest over every admitted entry (name, description, schema, effect)
    /// and the exposure mode.
    pub catalog_digest: ContentHash,
    pub exposure: crate::catalog::Exposure,
    pub names: Vec<String>,
    /// External hosts in contribution order with the catalog generation each
    /// was snapshotted under.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<crate::catalog::HostSummary>,
    /// Declarations left out and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<crate::catalog::ExcludedTool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_model_routes: Vec<String>,
    /// Exact tool names the configuration pre-approves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_grants: Vec<String>,
}

/// The agent pack a plan realizes, when its profile came from one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackDescriptor {
    pub id: String,
    pub version: String,
    /// Hex SHA-256 of the manifest bytes.
    pub manifest_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_deny: Vec<String>,
}

/// The compiled skill/command index: what the model can be told about and
/// what `/name` resolves to, without any document body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillIndexDescriptor {
    pub digest: ContentHash,
    pub indexed: usize,
    pub disclosed: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Everything behavior-affecting about a compiled plan, and nothing secret.
///
/// Field order is the canonical order: the digest hashes the compact JSON
/// encoding of this struct exactly as `serde` emits it, so reordering or
/// renaming a field is an encoding change and requires a
/// [`DESCRIPTOR_VERSION`] bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanDescriptor {
    pub version: u16,
    /// The configured agent profile the plan realizes.
    pub profile: AgentProfileId,
    /// Provider adapter build identity (crate version and compiled families).
    pub adapter_build: String,
    pub provider: ProviderDescriptor,
    pub model: ResolvedModel,
    /// Canonical workspace root the plan executes in.
    pub workspace: String,
    pub prompt_version: PromptVersion,
    pub instruction_hash: qq_protocol::InstructionHash,
    /// `AGENTS.md` or `CLAUDE.md` when one was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_source: Option<String>,
    pub tools: ToolCatalogDescriptor,
    /// The roster and bounds `spawn_agent` was compiled with (version 4).
    pub delegation: DelegationRoster,
    /// When the root run's final answer is audited (version 4).
    pub audit: AuditDescriptor,
    pub skills: SkillIndexDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<PackDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerDescriptor>,
    /// Labels of the configuration sources that produced the plan, in
    /// application order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<String>,
    /// Registered pre-turn context sources in registration order, with the
    /// budget the runtime actually enforces (version 6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_sources: Vec<ContextSourceDescriptor>,
}

/// One registered context source: its identity, the clamped budget the
/// runtime enforces, and what happens when it fails. No source content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSourceDescriptor {
    pub name: String,
    pub version: String,
    pub budget: ContextBudgetDescriptor,
    pub fail_policy: FailPolicyDescriptor,
}

/// Fixed-width mirror of [`crate::ContextBudget`] so the canonical encoding
/// does not depend on the platform's `usize`.
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
    /// The exact bytes the digest covers: the domain separator followed by
    /// compact JSON in declaration order.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PlanCompileError> {
        let mut bytes = DIGEST_DOMAIN.to_vec();
        serde_json::to_writer(&mut bytes, self).map_err(|error| PlanCompileError::Encode {
            message: error.to_string(),
        })?;
        Ok(bytes)
    }

    pub fn digest(&self) -> Result<AgentPlanDigest, PlanCompileError> {
        let bytes = self.canonical_bytes()?;
        Ok(AgentPlanDigest::from_hash(ContentHash::from_bytes(
            Sha256::digest(&bytes).into(),
        )))
    }
}

/// The audit policy a plan compiled with. Mirrors `runtime::AuditPolicy` in
/// the canonical encoding.
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
