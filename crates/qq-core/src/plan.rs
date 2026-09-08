//! Compile-once execution plans.
//!
//! A [`CompiledAgentPlan`] is everything about an agent's behavior that does
//! not depend on the prompt: the compiled provider handle, the resolved model,
//! the open workspace with its instructions, the complete tool catalog (built-
//! ins plus every external host's admitted declarations), the skill index, the
//! and the sub-agent routes. Compiling it does the filesystem,
//! provider, and host-catalog work once; the run loop then executes directly
//! from shared immutable data. Its [`AgentPlanDescriptor`] is the secret-free
//! account of that behavior whose canonical digest identifies the plan for
//! caching, traces, and the wire.

mod descriptor;
mod fingerprint;

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use qq_protocol::{
    AgentPlanDigest, AgentProfileId, ContentHash, CredentialEpoch, DelegationRoster,
    InstructionHash, ResolvedModel, RunPlanIdentity,
};
use qq_provider::Provider;
use sha2::Digest as _;
use thiserror::Error;

pub use descriptor::{
    AgentPlanDescriptor, AuditDescriptor, AuditModeDescriptor, CredentialReference,
    DESCRIPTOR_VERSION, McpServerDescriptor, McpTransportKind, PackDescriptor, ProviderDescriptor,
    SkillIndexDescriptor, ToolCatalogDescriptor,
};
pub use fingerprint::SourceFingerprint;

use crate::{
    ContextCache, ContextSource, Runtime, RuntimeConfigError,
    catalog::{
        EffectClass, HostContribution, StaticTool, ToolCatalog, ToolHost, select_tools_spec,
    },
    hosts::{ExternalToolHost, HostCatalog},
    runtime::{AuditPolicy, search_history_spec},
    tools,
    workspace::{
        SkillIndex, Workspace, WorkspaceInstructionError, WorkspaceInstructions,
        skills::{SkillRoot, load_skill_spec},
    },
};

/// One external host and the catalog snapshot it contributed to this
/// compile. The snapshot is taken by the caller (outside the compile, where
/// it may await); the compile only validates and orders it.
pub struct HostSnapshot {
    pub host: Arc<dyn ExternalToolHost>,
    pub catalog: HostCatalog,
}

impl HostSnapshot {
    /// Snapshots `host` now, blocking for at most the host's own bounds.
    #[must_use]
    pub fn capture_blocking(host: Arc<dyn ExternalToolHost>) -> Self {
        let catalog = host.catalog_blocking();
        Self { host, catalog }
    }
}

/// The pack behind a selected profile, as the compiler needs it: identity for
/// the descriptor, roots to index, a persona to read, and a tool policy to
/// apply. Paths are absolute and were validated to stay inside the pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackSelection {
    pub id: String,
    pub version: String,
    /// Hex SHA-256 of the manifest bytes.
    pub manifest_digest: String,
    /// Canonical pack directory; skill roots and the persona live under it.
    pub directory: PathBuf,
    /// Pack-relative path of the persona document, if any.
    pub persona: Option<String>,
    /// Pack-relative directories of `<name>/SKILL.md` documents.
    pub skill_roots: Vec<String>,
    /// Pack-relative directories of `<name>.md` documents.
    pub command_roots: Vec<String>,
    /// Exact names or `prefix*` rules; `deny` wins, empty `allow` means all.
    pub tool_allow: Vec<String>,
    pub tool_deny: Vec<String>,
}

impl PackSelection {
    fn permits(&self, tool: &str) -> bool {
        let matches = |rule: &String| match rule.strip_suffix('*') {
            Some(prefix) => tool.starts_with(prefix),
            None => rule == tool,
        };
        if self.tool_deny.iter().any(matches) {
            return false;
        }
        self.tool_allow.is_empty() || self.tool_allow.iter().any(matches)
    }
}

/// Typed, application-neutral input to plan compilation. The embedding
/// application translates its configuration into this shape; core never sees
/// configuration documents, secret values, or credential stores.
pub struct AgentProfile {
    provider: Arc<dyn Provider>,
    provider_descriptor: ProviderDescriptor,
    resolved_model: ResolvedModel,
    workspace: PathBuf,
    hosts: Vec<HostSnapshot>,
    mcp_servers: Vec<McpServerDescriptor>,
    spawn_model_routes: Vec<String>,
    delegation: DelegationRoster,
    audit: AuditPolicy,
    adapter_build: String,
    provenance: Vec<String>,
    credential_epoch: CredentialEpoch,
    profile_id: AgentProfileId,
    pack: Option<PackSelection>,
    exposed_tools: Option<Vec<String>>,
    context_sources: Vec<Arc<dyn ContextSource>>,
    context_cache: Option<Arc<ContextCache>>,
}

impl AgentProfile {
    /// Starts a profile for a compiled provider. `workspace` must already be
    /// the canonical absolute path the run will execute in.
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        provider_descriptor: ProviderDescriptor,
        resolved_model: ResolvedModel,
        workspace: PathBuf,
    ) -> Self {
        Self {
            provider,
            provider_descriptor,
            resolved_model,
            workspace,
            hosts: Vec::new(),
            mcp_servers: Vec::new(),
            spawn_model_routes: Vec::new(),
            delegation: DelegationRoster::default(),
            audit: AuditPolicy::default(),
            adapter_build: qq_provider::BUILD_IDENTITY.to_owned(),
            provenance: Vec::new(),
            credential_epoch: CredentialEpoch::NONE,
            profile_id: AgentProfileId::default(),
            pack: None,
            exposed_tools: None,
            context_sources: Vec::new(),
            context_cache: None,
        }
    }

    /// A profile for an embedded runtime built without configuration: the
    /// descriptor records the provider as embedded and takes model identity
    /// from the runtime. The runtime's hosts are snapshotted here, blocking.
    /// Used by direct `Runtime::run*` entry points and tests.
    #[must_use]
    pub fn embedded(runtime: &Runtime, workspace: PathBuf) -> Self {
        Self {
            provider: Arc::clone(&runtime.provider),
            provider_descriptor: ProviderDescriptor::embedded(),
            resolved_model: runtime.embedded_resolved_model(),
            workspace,
            hosts: runtime
                .hosts
                .iter()
                .map(|host| HostSnapshot::capture_blocking(Arc::clone(host)))
                .collect(),
            mcp_servers: Vec::new(),
            spawn_model_routes: runtime.spawn_model_routes.to_vec(),
            delegation: runtime.delegation.as_ref().clone(),
            audit: runtime.audit,
            adapter_build: qq_provider::BUILD_IDENTITY.to_owned(),
            provenance: Vec::new(),
            credential_epoch: CredentialEpoch::NONE,
            profile_id: AgentProfileId::default(),
            pack: None,
            exposed_tools: None,
            context_sources: runtime
                .context_sources
                .iter()
                .map(|registered| Arc::clone(&registered.source))
                .collect(),
            context_cache: Some(Arc::clone(&runtime.context_cache)),
        }
    }

    /// Registers a bounded pre-turn context source for runs of this plan.
    /// See [`Runtime::with_context_source`].
    #[must_use]
    pub fn with_context_source(mut self, source: Arc<dyn ContextSource>) -> Self {
        self.context_sources.push(source);
        self
    }

    /// Shares a context cache across plans (the root passes one per factory).
    #[must_use]
    pub fn with_context_cache(mut self, cache: Arc<ContextCache>) -> Self {
        self.context_cache = Some(cache);
        self
    }

    /// Selects an agent pack's resources for this plan. The persona is read
    /// and the roots are indexed at compile; the tool policy filters the
    /// catalog's exposed entries.
    #[must_use]
    pub fn with_pack(mut self, pack: PackSelection) -> Self {
        self.pack = Some(pack);
        self
    }

    /// Narrows catalog exposure to these exact names, intersected with the
    /// selected pack's policy. An empty list exposes no tools and grants
    /// cannot restore excluded tools. Names are checked during compilation.
    #[must_use]
    pub fn with_exposed_tools(mut self, tools: Vec<String>) -> Self {
        self.exposed_tools = Some(tools);
        self
    }

    /// Adds an external tool host with its already-captured catalog. Hosts
    /// contribute in the order added; earlier hosts win catalog capacity.
    #[must_use]
    pub fn with_host(mut self, snapshot: HostSnapshot) -> Self {
        self.hosts.push(snapshot);
        self
    }

    /// Records the secret-free descriptors of the configured MCP servers the
    /// hosts realize. Declaration identity enters the digest independently of
    /// the catalog the servers happen to serve.
    #[must_use]
    pub fn with_mcp_servers(mut self, servers: Vec<McpServerDescriptor>) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// Restricts model-visible sub-agent overrides to these canonical routes.
    #[must_use]
    pub fn with_spawn_model_routes(mut self, routes: Vec<String>) -> Self {
        self.spawn_model_routes = routes;
        self
    }

    /// The delegation roster the agent may spawn from, with its bounds. When
    /// the roster is non-empty it also restricts `spawn_agent`'s exact model
    /// override to roster routes.
    #[must_use]
    pub fn with_delegation(mut self, delegation: DelegationRoster) -> Self {
        self.delegation = delegation;
        self
    }

    /// When a root run's final answer is audited before completion.
    #[must_use]
    pub const fn with_audit(mut self, audit: AuditPolicy) -> Self {
        self.audit = audit;
        self
    }

    /// Human-readable, secret-free labels of the configuration layers that
    /// produced this profile, in application order. They participate in the
    /// digest so a plan compiled from a different source set is distinct.
    #[must_use]
    pub fn with_provenance(mut self, sources: Vec<String>) -> Self {
        self.provenance = sources;
        self
    }

    /// The credential epoch the provider handle was authorized against. It is
    /// carried beside the plan for rotation diagnosis and never enters the
    /// behavioral digest.
    #[must_use]
    pub fn with_credential_epoch(mut self, epoch: CredentialEpoch) -> Self {
        self.credential_epoch = epoch;
        self
    }

    /// The configured agent profile this plan realizes. Part of the digest:
    /// two profiles that happen to resolve identically are still distinct
    /// plans, because the caller selected them by name.
    #[must_use]
    pub fn with_profile_id(mut self, profile_id: AgentProfileId) -> Self {
        self.profile_id = profile_id;
        self
    }
}

/// Why a profile could not be compiled into a plan.
#[derive(Debug, Error)]
pub enum PlanCompileError {
    #[error(
        "exposed tool {name:?} is not a known static tool or a member of the discovered catalog"
    )]
    UnknownExposedTool { name: String },
    #[error(transparent)]
    Runtime(#[from] RuntimeConfigError),
    #[error("workspace path must be absolute and canonical: {path}")]
    NonCanonicalWorkspace { path: PathBuf },
    #[error("could not open workspace {path}: {source}")]
    OpenWorkspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Instructions(#[from] WorkspaceInstructionError),
    #[error("could not open agent pack {id} at {path}: {source}")]
    OpenPack {
        id: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("agent pack {id} persona {path} could not be read: {message}")]
    Persona {
        id: String,
        path: String,
        message: String,
    },
    #[error(
        "resolved model {route} does not match the runtime: model {runtime_model}, output limit \
         {runtime_max_output_tokens}, context window {runtime_context_window:?}"
    )]
    ModelMismatch {
        route: String,
        runtime_model: String,
        runtime_max_output_tokens: u32,
        runtime_context_window: Option<u32>,
    },
    #[error("descriptor could not be encoded canonically: {message}")]
    Encode { message: String },
    #[error(
        "the spawn_agent declaration is {bytes} bytes, above its {limit}-byte bound; shorten \
         roster notes or the roster"
    )]
    SpawnSchemaTooLarge { bytes: usize, limit: usize },
}

/// The immutable live plan one or more runs execute from. It is never
/// serialized: its [`descriptor`](Self::descriptor) is the durable, secret-free
/// account, and the [`digest`](Self::digest) is its identity.
pub struct CompiledAgentPlan {
    pub(crate) runtime: Runtime,
    pub(crate) workspace: Workspace,
    pub(crate) instructions: WorkspaceInstructions,
    pub(crate) catalog: Arc<ToolCatalog>,
    pub(crate) skills: Arc<SkillIndex>,
    /// Opened pack roots the skill index refers to by position.
    pub(crate) pack_roots: Arc<[Workspace]>,
    /// Pack persona text appended after workspace instructions, if any.
    pub(crate) persona: Option<Arc<Persona>>,
    /// The rendered roster block for the Delegation prompt section, when a
    /// roster is configured. Built once here; the run loop concatenates it.
    pub(crate) roster_text: Option<Arc<str>>,
    /// Host handles indexed as `ToolHost::External { host }` names them.
    pub(crate) hosts: Arc<[Arc<dyn ExternalToolHost>]>,
    resolved_model: Arc<ResolvedModel>,
    descriptor: Arc<AgentPlanDescriptor>,
    descriptor_json: Arc<str>,
    digest: AgentPlanDigest,
    credential_epoch: CredentialEpoch,
    /// Instruction files plus skill root directories: everything a cache
    /// must `stat` to revalidate the workspace side of this plan.
    sources: Vec<SourceFingerprint>,
    estimated_bytes: usize,
}

impl fmt::Debug for CompiledAgentPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledAgentPlan")
            .field("digest", &self.digest)
            .field("credential_epoch", &self.credential_epoch)
            .field("route", &self.resolved_model.route)
            .field("workspace", &self.workspace.path())
            .field("catalog", &self.catalog)
            .finish_non_exhaustive()
    }
}

impl CompiledAgentPlan {
    /// Compiles a profile. This opens the workspace, reads its instruction
    /// file, indexes its skill roots, builds the tool catalog from the static
    /// declarations and the host snapshots, and encodes the descriptor. It
    /// performs blocking filesystem work and must run off the async executor
    /// (`spawn_blocking` or a dedicated thread).
    pub fn compile_blocking(profile: AgentProfile) -> Result<Arc<Self>, PlanCompileError> {
        let AgentProfile {
            provider,
            provider_descriptor,
            resolved_model,
            workspace,
            hosts,
            mcp_servers,
            spawn_model_routes,
            delegation,
            audit,
            adapter_build,
            provenance,
            credential_epoch,
            profile_id,
            pack,
            exposed_tools,
            context_sources,
            context_cache,
        } = profile;
        let mut runtime = Runtime::with_provider(
            provider,
            resolved_model.provider_model.clone(),
            resolved_model.max_output_tokens,
        )?
        .with_context_window(resolved_model.context_window)
        .with_spawn_model_routes(spawn_model_routes)
        .with_delegation(delegation)
        .with_audit(audit);
        for source in context_sources {
            runtime = runtime.with_context_source(source);
        }
        if let Some(cache) = context_cache {
            runtime = runtime.with_context_cache(cache);
        }
        let mut config_grants = Vec::new();
        let mut host_handles = Vec::with_capacity(hosts.len());
        let mut contributions = Vec::with_capacity(hosts.len());
        for snapshot in hosts {
            config_grants.extend(snapshot.host.config_grants());
            contributions.push(HostContribution {
                name: snapshot.host.name().to_owned(),
                catalog: snapshot.catalog,
            });
            host_handles.push(snapshot.host);
        }
        config_grants.sort();
        config_grants.dedup();
        let host_handles: Arc<[Arc<dyn ExternalToolHost>]> = host_handles.into();
        runtime.hosts = Arc::clone(&host_handles);

        if !is_canonical_absolute(&workspace) {
            return Err(PlanCompileError::NonCanonicalWorkspace { path: workspace });
        }
        let opened =
            Workspace::open(&workspace).map_err(|source| PlanCompileError::OpenWorkspace {
                path: workspace.clone(),
                source,
            })?;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let (instructions, mut sources) =
            crate::workspace::load_instructions_with_sources(&opened, &cancelled)?;
        // A selected pack contributes one opened root (capability-scoped like
        // the workspace), its skill/command roots to the index, and a persona
        // read once here. Every path it contributes is fingerprinted.
        let mut skill_roots = SkillRoot::workspace_defaults();
        let mut pack_roots: Vec<Workspace> = Vec::new();
        let mut persona = None;
        let mut pack_descriptor = None;
        if let Some(selection) = &pack {
            let pack_workspace = Workspace::open(&selection.directory).map_err(|source| {
                PlanCompileError::OpenPack {
                    id: selection.id.clone(),
                    path: selection.directory.clone(),
                    source,
                }
            })?;
            let root_index = pack_roots.len();
            for directory in &selection.skill_roots {
                skill_roots.push(SkillRoot::pack(
                    &selection.id,
                    root_index,
                    directory,
                    crate::workspace::SkillKind::Skill,
                ));
            }
            for directory in &selection.command_roots {
                skill_roots.push(SkillRoot::pack(
                    &selection.id,
                    root_index,
                    directory,
                    crate::workspace::SkillKind::Command,
                ));
            }
            if let Some(relative) = &selection.persona {
                let (text, fingerprint) = read_persona(&pack_workspace, selection, relative)?;
                sources.push(fingerprint);
                persona = Some(Arc::new(Persona {
                    pack: selection.id.clone(),
                    source: relative.clone(),
                    hash: ContentHash::from_bytes(sha2::Sha256::digest(text.as_bytes()).into()),
                    text,
                }));
            }
            pack_descriptor = Some(PackDescriptor {
                id: selection.id.clone(),
                version: selection.version.clone(),
                manifest_digest: selection.manifest_digest.clone(),
                persona_hash: persona.as_ref().map(|persona| persona.hash),
                tool_allow: selection.tool_allow.clone(),
                tool_deny: selection.tool_deny.clone(),
            });
            pack_roots.push(pack_workspace);
        }
        let (skills, skill_sources) =
            SkillIndex::compile_blocking(&opened, &pack_roots, &skill_roots);
        sources.extend(skill_sources);

        let mut static_tools: Vec<StaticTool> = tools::static_tools();
        let spawn_spec = tools::spawn_agent_spec(&runtime.spawn_model_routes, &runtime.delegation);
        // Only roster-bearing declarations are bounded: the legacy flat route
        // enum may legitimately list every authenticated model.
        if !runtime.delegation.roster.is_empty() {
            let bytes = spawn_spec.name().len()
                + spawn_spec.description().len()
                + spawn_spec.input_schema().to_string().len();
            if bytes > tools::MAX_SPAWN_AGENT_SCHEMA_BYTES {
                return Err(PlanCompileError::SpawnSchemaTooLarge {
                    bytes,
                    limit: tools::MAX_SPAWN_AGENT_SCHEMA_BYTES,
                });
            }
        }
        static_tools.push(StaticTool {
            spec: spawn_spec,
            host: ToolHost::SpawnAgent,
            effect: EffectClass::ReadOnly,
        });
        static_tools.push(StaticTool {
            spec: search_history_spec(),
            host: ToolHost::SearchHistory,
            effect: EffectClass::ReadOnly,
        });
        static_tools.push(StaticTool {
            spec: select_tools_spec(),
            host: ToolHost::SelectTools,
            effect: EffectClass::ReadOnly,
        });
        if skills.disclosed_count() > 0 {
            static_tools.push(StaticTool {
                spec: load_skill_spec(),
                host: ToolHost::LoadSkill,
                effect: EffectClass::ReadOnly,
            });
        }
        let exposed_tools =
            exposed_tools.map(|names| names.into_iter().collect::<std::collections::BTreeSet<_>>());
        if let Some(names) = &exposed_tools {
            // Validate before either restriction removes tools. load_skill
            // is known even when this workspace has no disclosed skills.
            let known = static_tools
                .iter()
                .map(|tool| tool.spec.name())
                .chain(std::iter::once("load_skill"))
                .chain(
                    contributions
                        .iter()
                        .flat_map(|host| host.catalog.tools.iter().map(|tool| tool.spec.name())),
                )
                .collect::<std::collections::BTreeSet<_>>();
            for name in names {
                if !known.contains(name.as_str()) {
                    return Err(PlanCompileError::UnknownExposedTool { name: name.clone() });
                }
            }
        }
        // A pack tool policy filters what the catalog exposes. Filtering the
        // inputs (rather than the compiled catalog) keeps every catalog
        // invariant intact and makes the digest reflect the policy. The
        // selector and loader are never filtered out: they are how the model
        // reaches what the policy does allow.
        if let Some(selection) = &pack {
            static_tools.retain(|tool| {
                matches!(tool.host, ToolHost::SelectTools | ToolHost::LoadSkill)
                    || selection.permits(tool.spec.name())
            });
            for contribution in &mut contributions {
                contribution
                    .catalog
                    .tools
                    .retain(|tool| selection.permits(tool.spec.name()));
            }
        }
        if let Some(names) = &exposed_tools {
            static_tools.retain(|tool| names.contains(tool.spec.name()));
            for contribution in &mut contributions {
                contribution
                    .catalog
                    .tools
                    .retain(|tool| names.contains(tool.spec.name()));
            }
        }
        let catalog = ToolCatalog::compile(static_tools, contributions);

        let descriptor = AgentPlanDescriptor {
            version: DESCRIPTOR_VERSION,
            profile: profile_id,
            adapter_build,
            provider: provider_descriptor,
            model: resolved_model.clone(),
            workspace: workspace.display().to_string(),
            prompt_version: crate::runtime::AGENT_PROMPT_VERSION,
            instruction_hash: instructions.hash(),
            instruction_source: instructions.source_path().map(str::to_owned),
            tools: ToolCatalogDescriptor {
                catalog_digest: catalog.digest(),
                exposure: catalog.exposure(),
                names: catalog.names().map(str::to_owned).collect(),
                hosts: catalog.hosts().to_vec(),
                excluded: catalog.excluded().to_vec(),
                spawn_model_routes: runtime.spawn_model_routes.to_vec(),
                config_grants,
            },
            delegation: runtime.delegation.as_ref().clone(),
            audit: AuditDescriptor::from(runtime.audit),
            skills: SkillIndexDescriptor {
                digest: skills.digest(),
                indexed: skills.len(),
                disclosed: skills.disclosed_count(),
                truncated: skills.truncated(),
            },
            pack: pack_descriptor,
            mcp_servers,
            provenance,
        };
        let digest = descriptor.digest()?;
        let descriptor_json = match serde_json::to_string(&descriptor) {
            Ok(json) => json,
            Err(error) => {
                return Err(PlanCompileError::Encode {
                    message: error.to_string(),
                });
            }
        };
        let estimated_bytes = descriptor_json.len()
            + descriptor.canonical_bytes()?.len()
            + instructions.content_len()
            + persona.as_ref().map_or(0, |persona| persona.text.len())
            + catalog.estimated_bytes()
            + skills.estimated_bytes()
            + std::mem::size_of::<Self>();
        let roster_text = crate::runtime::delegation_roster_text(
            &resolved_model.route,
            resolved_model.context_window,
            &runtime.delegation,
        )
        .map(Arc::from);
        Ok(Arc::new(Self {
            runtime,
            workspace: opened,
            instructions,
            catalog: Arc::new(catalog),
            skills: Arc::new(skills),
            pack_roots: pack_roots.into(),
            persona,
            roster_text,
            hosts: host_handles,
            resolved_model: Arc::new(resolved_model),
            descriptor_json: Arc::from(descriptor_json),
            descriptor: Arc::new(descriptor),
            digest,
            credential_epoch,
            sources,
            estimated_bytes,
        }))
    }

    #[must_use]
    pub fn descriptor(&self) -> &Arc<AgentPlanDescriptor> {
        &self.descriptor
    }

    #[must_use]
    pub const fn digest(&self) -> AgentPlanDigest {
        self.digest
    }

    #[must_use]
    pub const fn credential_epoch(&self) -> CredentialEpoch {
        self.credential_epoch
    }

    /// The wire identity persisted on runs admitted from this plan.
    #[must_use]
    pub fn identity(&self) -> RunPlanIdentity {
        RunPlanIdentity {
            profile: self.descriptor.profile.clone(),
            descriptor_version: self.descriptor.version,
            digest: self.digest,
            credential_epoch: self.credential_epoch,
        }
    }

    #[must_use]
    pub fn resolved_model(&self) -> &Arc<ResolvedModel> {
        &self.resolved_model
    }

    #[must_use]
    pub fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    /// The opened workspace capability, for resolving input attachments.
    pub(crate) fn workspace_handle(&self) -> Workspace {
        self.workspace.clone()
    }

    /// The canonical descriptor JSON persisted beside a run's identity.
    #[must_use]
    pub fn descriptor_json(&self) -> &Arc<str> {
        &self.descriptor_json
    }

    #[must_use]
    pub fn instruction_hash(&self) -> InstructionHash {
        self.instructions.hash()
    }

    /// The instruction files and skill roots this plan read or found absent,
    /// for callers that revalidate a cached plan against the filesystem.
    #[must_use]
    pub fn instruction_sources(&self) -> &[SourceFingerprint] {
        &self.sources
    }

    /// Whether every external host still serves the catalog generation this
    /// plan was compiled from. Cheap and synchronous.
    #[must_use]
    pub fn hosts_are_current(&self) -> bool {
        self.catalog
            .hosts()
            .iter()
            .zip(self.hosts.iter())
            .all(|(summary, host)| host.catalog_is_current(summary.generation))
    }

    #[must_use]
    pub fn catalog(&self) -> &Arc<ToolCatalog> {
        &self.catalog
    }

    #[must_use]
    pub fn skills(&self) -> &Arc<SkillIndex> {
        &self.skills
    }

    /// A conservative accounting of the heap this plan holds, for cache
    /// admission. It excludes the provider transport and host connections,
    /// which are shared across generations.
    #[must_use]
    pub const fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    #[must_use]
    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }
}

/// A pack persona: prompt text appended after workspace instructions.
#[derive(Debug)]
pub(crate) struct Persona {
    pub(crate) pack: String,
    pub(crate) source: String,
    pub(crate) hash: ContentHash,
    pub(crate) text: String,
}

impl Persona {
    pub(crate) fn append_to_prompt(&self, prompt: &mut String) {
        prompt.push_str("\n\nAgent pack `");
        prompt.push_str(&self.pack);
        prompt.push_str("` persona from ");
        prompt.push_str(&self.source);
        prompt.push_str(":\n--- BEGIN PACK PERSONA ---\n");
        prompt.push_str(&self.text);
        if !self.text.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("--- END PACK PERSONA ---");
    }
}

/// Largest pack persona document.
pub const MAX_PERSONA_BYTES: usize = 64 * 1024;

fn read_persona(
    pack: &Workspace,
    selection: &PackSelection,
    relative: &str,
) -> Result<(String, SourceFingerprint), PlanCompileError> {
    use std::io::Read as _;
    let fingerprint = SourceFingerprint::capture(pack.path().join(relative));
    let failure = |message: String| PlanCompileError::Persona {
        id: selection.id.clone(),
        path: relative.to_owned(),
        message,
    };
    let resolved = pack
        .contained_path(relative)
        .map_err(|error| failure(error.to_string()))?;
    let metadata = pack
        .root()
        .metadata(&resolved)
        .map_err(|error| failure(error.to_string()))?;
    if !metadata.is_file() {
        return Err(failure("not a regular file".to_owned()));
    }
    if metadata.len() > MAX_PERSONA_BYTES as u64 {
        return Err(failure(format!(
            "exceeds the {MAX_PERSONA_BYTES}-byte limit"
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    pack.root()
        .open(&resolved)
        .map_err(|error| failure(error.to_string()))?
        .take(MAX_PERSONA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| failure(error.to_string()))?;
    if bytes.len() > MAX_PERSONA_BYTES {
        return Err(failure(format!(
            "exceeds the {MAX_PERSONA_BYTES}-byte limit"
        )));
    }
    let text = String::from_utf8(bytes).map_err(|_| failure("not valid UTF-8".to_owned()))?;
    Ok((text, fingerprint))
}

fn is_canonical_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}
#[cfg(test)]
mod tests_support {
    use std::{path::Path, sync::Arc};

    use futures_util::stream;
    use qq_protocol::{
        CapabilitySupport, GenerationCapabilities, PromptCacheCapabilities,
        ProviderRequestShapeIdentity, ProviderRequestShapeVersion, ResolvedModel,
        ResolvedModelVersion,
    };
    use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

    use super::{AgentProfile, CredentialReference, ProviderDescriptor};

    pub(super) struct SilentProvider;

    impl Provider for SilentProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
        }
    }

    pub(super) fn canonical_temp() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            std::fs::canonicalize(directory.path()).unwrap(),
            directory.path(),
            "test temp dir must be canonical"
        );
        directory
    }

    pub(super) fn resolved_model() -> ResolvedModel {
        ResolvedModel {
            version: ResolvedModelVersion::new(2).unwrap(),
            request_shape: Some(ProviderRequestShapeIdentity {
                version: ProviderRequestShapeVersion::new(1).unwrap(),
                digest: qq_protocol::ContentHash::from_bytes([7; 32]),
            }),
            route: "custom/test-model".to_owned(),
            provider_model: "test-model".to_owned(),
            organization: None,
            credential_profile: None,
            max_output_tokens: 256,
            context_window: Some(8_192),
            pricing: None,
            output_token_control: CapabilitySupport::Native,
            generation: GenerationCapabilities {
                reasoning_effort: CapabilitySupport::Unsupported,
            },
            prompt_cache: PromptCacheCapabilities {
                control: CapabilitySupport::Unsupported,
                cache_read_usage: false,
                cache_write_usage: false,
            },
        }
    }

    pub(super) fn provider_descriptor() -> ProviderDescriptor {
        ProviderDescriptor {
            id: "custom".to_owned(),
            api: "openai_responses".to_owned(),
            endpoint: Some("https://api.example.test/v1".to_owned()),
            endpoint_mode: Some("base".to_owned()),
            auth_scheme: "bearer".to_owned(),
            credential: CredentialReference::Environment("CUSTOM_TOKEN".to_owned()),
            header_names: Vec::new(),
            region: None,
        }
    }

    pub(super) fn profile(workspace: &Path) -> AgentProfile {
        AgentProfile::new(
            Arc::new(SilentProvider),
            provider_descriptor(),
            resolved_model(),
            workspace.to_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use qq_protocol::PromptVersion;

    use super::{tests_support::*, *};

    #[test]
    fn explicit_exposure_narrows_the_catalog_and_empty_exposes_nothing() {
        let workspace = canonical_temp();
        let default = CompiledAgentPlan::compile_blocking(profile(workspace.path())).unwrap();
        let narrow = CompiledAgentPlan::compile_blocking(
            profile(workspace.path())
                .with_exposed_tools(vec!["read_file".to_owned(), "search".to_owned()]),
        )
        .unwrap();
        assert_eq!(
            narrow.catalog().names().collect::<Vec<_>>(),
            ["read_file", "search"]
        );
        assert_ne!(default.digest(), narrow.digest());
        let empty = CompiledAgentPlan::compile_blocking(
            profile(workspace.path()).with_exposed_tools(Vec::new()),
        )
        .unwrap();
        assert_eq!(empty.catalog().names().count(), 0);
        assert_ne!(narrow.digest(), empty.digest());
        let same = CompiledAgentPlan::compile_blocking(
            profile(workspace.path()).with_exposed_tools(vec![
                "search".to_owned(),
                "read_file".to_owned(),
                "read_file".to_owned(),
            ]),
        )
        .unwrap();
        assert_eq!(narrow.digest(), same.digest());
    }

    #[test]
    fn unknown_exposed_names_fail_compilation_before_provider_work() {
        let workspace = canonical_temp();
        for name in ["missing", "mcp__server__missing"] {
            let result = CompiledAgentPlan::compile_blocking(
                profile(workspace.path()).with_exposed_tools(vec![name.to_owned()]),
            );
            assert!(
                matches!(result, Err(PlanCompileError::UnknownExposedTool { name: rejected }) if rejected == name)
            );
        }
    }

    #[test]
    fn exposed_skill_loader_follows_workspace_skill_availability() {
        let workspace = canonical_temp();
        let compile = || {
            CompiledAgentPlan::compile_blocking(
                profile(workspace.path())
                    .with_exposed_tools(vec!["read_file".to_owned(), "load_skill".to_owned()]),
            )
            .unwrap()
        };
        let empty = compile();
        assert_eq!(empty.catalog().names().collect::<Vec<_>>(), ["read_file"]);
        let skill = workspace.path().join(".qq/skills/example/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            skill,
            "---\ndescription: Example guidance\n---\nBe brief.\n",
        )
        .unwrap();
        let installed = compile();
        assert_eq!(
            installed.catalog().names().collect::<Vec<_>>(),
            ["load_skill", "read_file"]
        );
    }

    /// A descriptor with fixed inputs, independent of the filesystem, so the
    /// golden digest below is stable across machines.
    fn golden_descriptor() -> AgentPlanDescriptor {
        AgentPlanDescriptor {
            version: DESCRIPTOR_VERSION,
            profile: AgentProfileId::new("review").unwrap(),
            adapter_build: "qq-provider/0.1.0+bedrock".to_owned(),
            provider: provider_descriptor(),
            model: resolved_model(),
            workspace: "/work".to_owned(),
            prompt_version: crate::runtime::AGENT_PROMPT_VERSION,
            instruction_hash: InstructionHash::from_bytes([1; 32]),
            instruction_source: Some("AGENTS.md".to_owned()),
            tools: ToolCatalogDescriptor {
                catalog_digest: qq_protocol::ContentHash::from_bytes([2; 32]),
                exposure: crate::catalog::Exposure::Full,
                names: vec!["read_file".to_owned(), "spawn_agent".to_owned()],
                hosts: vec![crate::catalog::HostSummary {
                    name: "mcp".to_owned(),
                    generation: 4,
                    tool_count: 1,
                    ready: true,
                    readiness_message: None,
                }],
                excluded: vec![crate::catalog::ExcludedTool {
                    name: "mcp__executor__huge".to_owned(),
                    host: "mcp".to_owned(),
                    reason: crate::catalog::ExclusionReason::SchemaTooLarge { bytes: 20_000 },
                }],
                spawn_model_routes: vec!["custom/worker".to_owned()],
                config_grants: Vec::new(),
            },
            delegation: DelegationRoster {
                roster: vec![qq_protocol::DelegationRosterEntry {
                    route: "custom/worker".to_owned(),
                    role: qq_protocol::DelegationRole::Balanced,
                    note: Some("everyday".to_owned()),
                    context_window: Some(200_000),
                    max_output_tokens: Some(8_192),
                    relative_cost_permille: Some(400),
                }],
                default_role: qq_protocol::DelegationRole::Balanced,
                max_depth: 1,
                write_children: false,
            },
            audit: AuditDescriptor {
                mode: AuditModeDescriptor::Heuristic,
                max_revisions: 1,
                role: qq_protocol::DelegationRole::Strong,
            },
            skills: SkillIndexDescriptor {
                digest: qq_protocol::ContentHash::from_bytes([3; 32]),
                indexed: 2,
                disclosed: 1,
                truncated: false,
            },
            pack: Some(PackDescriptor {
                id: "review-kit".to_owned(),
                version: "1.2.0".to_owned(),
                manifest_digest: "ab".repeat(32),
                persona_hash: Some(qq_protocol::ContentHash::from_bytes([4; 32])),
                tool_allow: vec!["read_file".to_owned(), "mcp__*".to_owned()],
                tool_deny: vec!["shell".to_owned()],
            }),
            mcp_servers: vec![McpServerDescriptor {
                name: "executor".to_owned(),
                transport: McpTransportKind::Stdio,
                target: "executor".to_owned(),
                args: vec!["mcp".to_owned()],
                env: Vec::new(),
                credential: CredentialReference::None,
                eager: true,
                allow: vec!["execute".to_owned()],
                call_timeout_seconds: 60,
                max_concurrent_calls: 4,
            }],
            provenance: vec!["compiled defaults".to_owned()],
        }
    }

    #[test]
    fn canonical_encoding_and_digest_are_stable() {
        let descriptor = golden_descriptor();
        let bytes = descriptor.canonical_bytes().unwrap();
        assert!(
            bytes.starts_with(
                b"qq-agent-plan-descriptor-v5\0{\"version\":5,\"profile\":\"review\","
            )
        );
        // The golden digest pins the canonical encoding. A change here means
        // DESCRIPTOR_VERSION must be bumped and every recorded digest is
        // from a different encoding.
        assert_eq!(
            descriptor.digest().unwrap().to_string(),
            "724ffeb55a54288aebdf946424f81deb4a6e87f07c04b722b850b5e847dd0ca7"
        );
        let round_trip: AgentPlanDescriptor =
            serde_json::from_slice(&bytes[b"qq-agent-plan-descriptor-v5\0".len()..]).unwrap();
        assert_eq!(round_trip, descriptor);
        assert_eq!(round_trip.digest().unwrap(), descriptor.digest().unwrap());
    }

    #[test]
    fn every_behavior_affecting_field_changes_the_digest() {
        let base = golden_descriptor();
        let base_digest = base.digest().unwrap();
        type Mutation = Box<dyn Fn(&mut AgentPlanDescriptor)>;
        let variants: Vec<(&str, Mutation)> = vec![
            ("adapter_build", Box::new(|d| d.adapter_build.push('x'))),
            ("provider.id", Box::new(|d| d.provider.id.push('x'))),
            ("provider.api", Box::new(|d| d.provider.api.push('x'))),
            (
                "provider.endpoint",
                Box::new(|d| d.provider.endpoint = None),
            ),
            (
                "provider.endpoint_mode",
                Box::new(|d| {
                    d.provider.endpoint_mode = Some("exact".to_owned());
                }),
            ),
            (
                "provider.auth_scheme",
                Box::new(|d| d.provider.auth_scheme.push('x')),
            ),
            (
                "provider.credential",
                Box::new(|d| {
                    d.provider.credential = CredentialReference::Inline;
                }),
            ),
            (
                "provider.header_names",
                Box::new(|d| {
                    d.provider.header_names.push("X-Extra".to_owned());
                }),
            ),
            (
                "provider.region",
                Box::new(|d| d.provider.region = Some("us-east-1".to_owned())),
            ),
            (
                "model.provider_model",
                Box::new(|d| d.model.provider_model.push('x')),
            ),
            (
                "model.max_output_tokens",
                Box::new(|d| d.model.max_output_tokens += 1),
            ),
            (
                "model.context_window",
                Box::new(|d| d.model.context_window = None),
            ),
            (
                "model.request_shape",
                Box::new(|d| d.model.request_shape = None),
            ),
            ("workspace", Box::new(|d| d.workspace.push('x'))),
            (
                "prompt_version",
                Box::new(|d| {
                    d.prompt_version = PromptVersion::new(1).unwrap();
                }),
            ),
            (
                "instruction_hash",
                Box::new(|d| {
                    d.instruction_hash = InstructionHash::from_bytes([9; 32]);
                }),
            ),
            (
                "instruction_source",
                Box::new(|d| d.instruction_source = None),
            ),
            (
                "tools.catalog_digest",
                Box::new(|d| {
                    d.tools.catalog_digest = qq_protocol::ContentHash::from_bytes([9; 32]);
                }),
            ),
            (
                "tools.exposure",
                Box::new(|d| d.tools.exposure = crate::catalog::Exposure::Progressive),
            ),
            (
                "tools.hosts.generation",
                Box::new(|d| d.tools.hosts[0].generation += 1),
            ),
            ("tools.excluded", Box::new(|d| d.tools.excluded.clear())),
            (
                "skills.digest",
                Box::new(|d| d.skills.digest = qq_protocol::ContentHash::from_bytes([9; 32])),
            ),
            ("skills.disclosed", Box::new(|d| d.skills.disclosed = 0)),
            (
                "delegation.roster",
                Box::new(|d| d.delegation.roster.clear()),
            ),
            (
                "delegation.role",
                Box::new(|d| d.delegation.roster[0].role = qq_protocol::DelegationRole::Fast),
            ),
            (
                "delegation.relative_cost",
                Box::new(|d| d.delegation.roster[0].relative_cost_permille = None),
            ),
            (
                "delegation.default_role",
                Box::new(|d| d.delegation.default_role = qq_protocol::DelegationRole::Strong),
            ),
            (
                "delegation.max_depth",
                Box::new(|d| d.delegation.max_depth = 2),
            ),
            (
                "delegation.write_children",
                Box::new(|d| d.delegation.write_children = true),
            ),
            ("pack", Box::new(|d| d.pack = None)),
            (
                "pack.version",
                Box::new(|d| d.pack.as_mut().unwrap().version.push('x')),
            ),
            (
                "pack.manifest_digest",
                Box::new(|d| d.pack.as_mut().unwrap().manifest_digest.push('x')),
            ),
            (
                "pack.persona_hash",
                Box::new(|d| d.pack.as_mut().unwrap().persona_hash = None),
            ),
            (
                "pack.tool_deny",
                Box::new(|d| d.pack.as_mut().unwrap().tool_deny.clear()),
            ),
            (
                "tools.names",
                Box::new(|d| d.tools.names.push("shell".to_owned())),
            ),
            (
                "tools.spawn_model_routes",
                Box::new(|d| d.tools.spawn_model_routes.clear()),
            ),
            (
                "tools.config_grants",
                Box::new(|d| {
                    d.tools
                        .config_grants
                        .push("mcp__executor__execute".to_owned());
                }),
            ),
            ("mcp_servers", Box::new(|d| d.mcp_servers.clear())),
            (
                "mcp_servers.allow",
                Box::new(|d| d.mcp_servers[0].allow.clear()),
            ),
            (
                "mcp_servers.credential",
                Box::new(|d| {
                    d.mcp_servers[0].credential = CredentialReference::Stored("tok".to_owned());
                }),
            ),
            (
                "provenance",
                Box::new(|d| d.provenance.push("extra".to_owned())),
            ),
        ];
        for (name, mutate) in variants {
            let mut variant = golden_descriptor();
            mutate(&mut variant);
            assert_ne!(
                variant.digest().unwrap(),
                base_digest,
                "changing {name} must change the digest"
            );
        }
    }

    #[test]
    fn compiled_plan_reads_instructions_once_and_records_secret_free_identity() {
        let directory = canonical_temp();
        std::fs::write(directory.path().join("AGENTS.md"), "Answer tersely.\n").unwrap();
        let plan = CompiledAgentPlan::compile_blocking(
            profile(directory.path())
                .with_spawn_model_routes(vec!["custom/worker".to_owned()])
                .with_provenance(vec!["inline".to_owned()])
                .with_credential_epoch(CredentialEpoch::new(3)),
        )
        .unwrap();

        let descriptor = plan.descriptor();
        assert_eq!(descriptor.version, DESCRIPTOR_VERSION);
        assert_eq!(descriptor.adapter_build, qq_provider::BUILD_IDENTITY);
        assert_eq!(descriptor.workspace, directory.path().display().to_string());
        assert_eq!(descriptor.instruction_source.as_deref(), Some("AGENTS.md"));
        assert_eq!(descriptor.instruction_hash, plan.instruction_hash());
        assert_eq!(
            descriptor.tools.spawn_model_routes,
            vec!["custom/worker".to_owned()]
        );
        for name in [
            "read_file",
            "shell",
            "spawn_agent",
            "search_history",
            "select_tools",
        ] {
            assert!(descriptor.tools.names.iter().any(|n| n == name), "{name}");
        }
        assert!(
            !descriptor.tools.names.iter().any(|n| n == "load_skill"),
            "no disclosed skills, no loader"
        );
        assert_eq!(descriptor.tools.exposure, crate::catalog::Exposure::Full);
        assert_eq!(descriptor.tools.catalog_digest, plan.catalog().digest());
        assert_eq!(descriptor.skills.indexed, 0);
        assert_eq!(plan.credential_epoch(), CredentialEpoch::new(3));
        assert_eq!(plan.digest(), descriptor.digest().unwrap());
        assert_eq!(plan.resolved_model().route, "custom/test-model");
        assert!(plan.estimated_bytes() > "Answer tersely.\n".len());

        // Both instruction candidates and all five skill roots are
        // fingerprinted, present or absent.
        let sources = plan.instruction_sources();
        assert_eq!(sources.len(), 7);
        assert!(sources[0].path().ends_with("AGENTS.md") && sources[0].is_present());
        assert!(sources[1].path().ends_with("CLAUDE.md") && !sources[1].is_present());

        // The epoch is not part of behavioral identity.
        let other_epoch = CompiledAgentPlan::compile_blocking(
            profile(directory.path())
                .with_spawn_model_routes(vec!["custom/worker".to_owned()])
                .with_provenance(vec!["inline".to_owned()])
                .with_credential_epoch(CredentialEpoch::new(4)),
        )
        .unwrap();
        assert_eq!(other_epoch.digest(), plan.digest());

        // Changing the instructions changes the digest.
        std::fs::write(directory.path().join("AGENTS.md"), "Answer verbosely.\n").unwrap();
        let edited = CompiledAgentPlan::compile_blocking(
            profile(directory.path())
                .with_spawn_model_routes(vec!["custom/worker".to_owned()])
                .with_provenance(vec!["inline".to_owned()]),
        )
        .unwrap();
        assert_ne!(edited.digest(), plan.digest());
    }

    #[test]
    fn compile_rejects_non_canonical_paths_missing_workspaces_and_bad_instructions() {
        let directory = canonical_temp();
        let relative = CompiledAgentPlan::compile_blocking(profile(Path::new("relative/path")));
        assert!(matches!(
            relative,
            Err(PlanCompileError::NonCanonicalWorkspace { .. })
        ));
        let dotted = CompiledAgentPlan::compile_blocking(profile(&directory.path().join("..")));
        assert!(matches!(
            dotted,
            Err(PlanCompileError::NonCanonicalWorkspace { .. })
        ));
        let missing =
            CompiledAgentPlan::compile_blocking(profile(&directory.path().join("absent")));
        assert!(matches!(
            missing,
            Err(PlanCompileError::OpenWorkspace { .. })
        ));

        std::fs::create_dir(directory.path().join("AGENTS.md")).unwrap();
        let not_a_file = CompiledAgentPlan::compile_blocking(profile(directory.path()));
        assert!(matches!(
            not_a_file,
            Err(PlanCompileError::Instructions(
                WorkspaceInstructionError::NotAFile { .. }
            ))
        ));
    }

    #[test]
    fn embedded_profiles_and_runtime_mismatches_are_reported() {
        let directory = canonical_temp();
        let runtime = Runtime::new(SilentProvider, "embedded-model", 512)
            .unwrap()
            .with_context_window(Some(2_048));
        let plan = CompiledAgentPlan::compile_blocking(AgentProfile::embedded(
            &runtime,
            directory.path().to_owned(),
        ))
        .unwrap();
        assert_eq!(plan.descriptor().provider, ProviderDescriptor::embedded());
        assert_eq!(plan.resolved_model().route, "embedded/embedded-model");
        assert_eq!(plan.resolved_model().max_output_tokens, 512);
        assert_eq!(plan.resolved_model().context_window, Some(2_048));

        let mut mismatched = resolved_model();
        mismatched.provider_model = "other".to_owned();
        let error = crate::LoadedRuntime::compile_blocking(
            &runtime,
            mismatched,
            directory.path().to_owned(),
        )
        .err()
        .expect("mismatch must fail");
        assert!(matches!(error, PlanCompileError::ModelMismatch { .. }));
    }
}

#[cfg(test)]
mod pack_tests {
    use std::sync::Arc;

    use super::{tests_support::*, *};

    #[test]
    fn a_selected_pack_contributes_persona_skills_and_tool_policy_to_the_plan() {
        let workspace = canonical_temp();
        std::fs::write(workspace.path().join("AGENTS.md"), "Workspace rules.\n").unwrap();
        let pack_dir = canonical_temp();
        for (path, content) in [
            ("prompts/persona.md", "You review code.\n"),
            (
                "skills/audit/SKILL.md",
                "---\ndescription: Audit a change\n---\nAudit steps.\n",
            ),
            (
                "commands/lint.md",
                "---\ndescription: Lint it\n---\nRun lint.\n",
            ),
        ] {
            let path = pack_dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let selection = || PackSelection {
            id: "review-kit".to_owned(),
            version: "1.2.0".to_owned(),
            manifest_digest: "cd".repeat(32),
            directory: pack_dir.path().to_owned(),
            persona: Some("prompts/persona.md".to_owned()),
            skill_roots: vec!["skills".to_owned()],
            command_roots: vec!["commands".to_owned()],
            tool_allow: vec!["read_file".to_owned(), "search".to_owned()],
            tool_deny: Vec::new(),
        };
        let plan =
            CompiledAgentPlan::compile_blocking(profile(workspace.path()).with_pack(selection()))
                .unwrap();

        let descriptor = plan.descriptor();
        let pack = descriptor.pack.as_ref().unwrap();
        assert_eq!(pack.id, "review-kit");
        assert_eq!(pack.tool_allow, ["read_file", "search"]);
        assert!(pack.persona_hash.is_some());
        // The policy filtered the static tools; the selector and loader stay.
        let names: Vec<&str> = plan.catalog().names().collect();
        assert!(names.contains(&"read_file") && names.contains(&"search"));
        assert!(!names.contains(&"shell") && !names.contains(&"edit_file"));
        assert!(
            !names.contains(&"spawn_agent"),
            "policy filters session tools too"
        );
        assert!(names.contains(&"select_tools") && names.contains(&"load_skill"));
        let intersected = CompiledAgentPlan::compile_blocking(
            profile(workspace.path())
                .with_pack(selection())
                .with_exposed_tools(vec!["read_file".to_owned(), "write_file".to_owned()]),
        )
        .unwrap();
        assert_eq!(
            intersected.catalog().names().collect::<Vec<_>>(),
            ["read_file"]
        );
        // Pack documents are indexed, disclosed, and provenance-labelled.
        let skills = plan.skills();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills.disclosed_count(), 2);
        let audit = skills.resolve_disclosed("audit").unwrap();
        assert_eq!(audit.source, "pack:review-kit/skills/audit/SKILL.md");
        assert_eq!(audit.root, Some(0));
        assert_eq!(audit.description, "Audit a change");
        // Loading a pack document reads from the pack root.
        let loaded = crate::workspace::load_entry(
            &plan.workspace,
            &plan.pack_roots,
            audit,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(loaded.render_for_tool().contains("Audit steps."));
        // The persona is in the system prompt after workspace instructions.
        let persona = plan.persona.as_ref().unwrap();
        let mut prompt = String::new();
        plan.instructions.append_to_prompt(&mut prompt);
        persona.append_to_prompt(&mut prompt);
        let rules = prompt.find("Workspace rules.").unwrap();
        let persona_at = prompt.find("You review code.").unwrap();
        assert!(rules < persona_at);
        assert!(prompt.contains("Agent pack `review-kit` persona from prompts/persona.md"));
        // Pack roots and the persona are fingerprinted for revalidation.
        assert!(
            plan.instruction_sources()
                .iter()
                .any(|s| s.path().ends_with("prompts/persona.md"))
        );
        assert!(
            plan.instruction_sources()
                .iter()
                .any(|s| s.path().ends_with("skills"))
        );

        // Same inputs, same digest; a persona edit changes it.
        let again =
            CompiledAgentPlan::compile_blocking(profile(workspace.path()).with_pack(selection()))
                .unwrap();
        assert_eq!(again.digest(), plan.digest());
        std::fs::write(
            pack_dir.path().join("prompts/persona.md"),
            "You audit code.\n",
        )
        .unwrap();
        let edited =
            CompiledAgentPlan::compile_blocking(profile(workspace.path()).with_pack(selection()))
                .unwrap();
        assert_ne!(edited.digest(), plan.digest());
        // No pack: no persona, no pack section, full static catalog.
        let plain = CompiledAgentPlan::compile_blocking(profile(workspace.path())).unwrap();
        assert!(plain.descriptor().pack.is_none());
        assert!(plain.persona.is_none());
        assert!(plain.catalog().names().any(|n| n == "shell"));
        drop(Arc::clone(&plain));

        // A missing persona fails the compile with a typed error.
        let mut missing = selection();
        missing.persona = Some("prompts/absent.md".to_owned());
        assert!(matches!(
            CompiledAgentPlan::compile_blocking(profile(workspace.path()).with_pack(missing)),
            Err(PlanCompileError::Persona { .. })
        ));
    }
}
